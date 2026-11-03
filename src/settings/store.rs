//! Reading and writing the configuration file for the Settings windows and the
//! default-mode controls.
//!
//! Loading keeps parsed-but-invalid values editable without weakening the
//! strict `Config::load` the engine uses. Saving edits the TOML document in
//! place with `toml_edit`, so comments, ordering and keys the window does not
//! show survive, and replaces the file atomically. Every save writes cleanup in
//! its current shape (`mode` and the per-mode tables) and removes the legacy
//! flat keys, after checking that the file still reads back as intended.
//! Nothing here touches a toolkit; both platform windows, the macOS menu and
//! `cantrip mode` share it.

use anyhow::{Context, Result};
use cantrip_engine::config::{Config, Mode, ModeModelConfig, PostprocConfig, VocabularyEntry};
use cantrip_engine::delivery::InjectionMode;
use cantrip_engine::paths;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

/// `[postproc]` names from before explicit modes. The engine still reads them;
/// a save writes what they meant in the current shape and removes them.
const LEGACY_POSTPROC: &[&str] = &["enabled", "model", "reasoning_effort"];

pub(crate) enum EditableConfigLoad {
    Ready {
        config: Box<Config>,
        text: String,
        /// Why the parsed values fail validation; they stay editable.
        warning: Option<String>,
    },
    Blocked {
        message: String,
    },
}

/// Load the real file for the structured editor without weakening the strict
/// `Config::load` path used by the daemon. Parsed values remain editable when
/// validation fails; unreadable or malformed TOML is never replaced.
pub(crate) fn load_editable_config(path: &Path) -> EditableConfigLoad {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
        Err(error) => {
            return EditableConfigLoad::Blocked {
                message: format!(
                    "Cannot read config; file was not changed: {error}. Check file ownership and permissions."
                ),
            };
        }
    };
    let config = match toml::from_str::<Config>(&text) {
        Ok(config) => config,
        Err(error) => {
            return EditableConfigLoad::Blocked {
                message: format!(
                    "Cannot parse config; file was not changed: {error}. Choose Repair configuration to edit the original text."
                ),
            };
        }
    };
    let warning = config.validate().err().map(|error| {
        format!(
            "Configuration needs repair: {error:#}. Correct the values, then choose Save & apply."
        )
    });
    EditableConfigLoad::Ready {
        config: Box::new(config),
        text,
        warning,
    }
}

/// Wire form of `InjectionMode` (matches the `lowercase` serde rename).
pub(crate) fn injection_str(mode: InjectionMode) -> &'static str {
    match mode {
        InjectionMode::Auto => "auto",
        InjectionMode::Paste => "paste",
        InjectionMode::Type => "type",
        InjectionMode::Clipboard => "clipboard",
    }
}

/// Write the edited config back to disk while preserving comments and ordering
/// for every key the window did not touch (so the annotated template survives).
pub(crate) fn save_config_preserving(
    path: &Path,
    config: &Config,
    expected: &str,
) -> Result<String> {
    let existing = read_config(path)?;
    anyhow::ensure!(
        existing == expected,
        "Config changed on disk since opened — click Reload from disk first"
    );
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let root = doc.as_table_mut();

    set_preserving_decor(
        root,
        "injection",
        toml_edit::value(injection_str(config.injection)),
    );
    set_preserving_decor(root, "keep_warm", toml_edit::value(config.keep_warm));
    set_or_remove(root, "audio_source", config.audio_source.as_deref());
    #[cfg(target_os = "macos")]
    set_or_remove(root, "hotkey", config.hotkey.as_deref());
    let original: Config = toml::from_str(&existing)?;
    if config.vocabulary != original.vocabulary {
        let old = root.get("vocabulary").and_then(toml_edit::Item::as_array);
        let mut vocab = toml_edit::Array::new();
        for entry in &config.vocabulary {
            if let VocabularyEntry::Term(term) = entry {
                vocab.push(term.as_str());
            } else if let Some(value) = original
                .vocabulary
                .iter()
                .position(|old| old == entry)
                .and_then(|index| old.and_then(|array| array.get(index)))
            {
                vocab.push_formatted(value.clone());
            } else if let VocabularyEntry::Aliased { term, aliases } = entry {
                let mut table = toml_edit::InlineTable::new();
                table.insert("term", term.as_str().into());
                let mut values = toml_edit::Array::new();
                for alias in aliases {
                    values.push(alias.as_str());
                }
                table.insert("aliases", values.into());
                vocab.push(table);
            }
        }
        set_preserving_decor(root, "vocabulary", toml_edit::value(vocab));
    }

    let hud = ensure_table(root, "hud")?;
    set_preserving_decor(hud, "labels", toml_edit::value(config.hud.labels));
    if let Some(reduced) = config.hud.reduced_motion {
        set_preserving_decor(hud, "reduced_motion", toml_edit::value(reduced));
    } else {
        hud.remove("reduced_motion");
    }

    let stt = ensure_table(root, "stt")?;
    set_preserving_decor(stt, "model", toml_edit::value(config.stt.model.clone()));
    set_or_remove(stt, "endpoint", config.stt.endpoint.as_deref());
    set_or_remove(stt, "api_key_id", config.stt.api_key_id.as_deref());

    let postproc = ensure_table(root, "postproc")?;
    // Timeout is a small positive integer (ms); far below i64::MAX in practice.
    set_preserving_decor(
        postproc,
        "timeout_ms",
        toml_edit::value(config.postproc.timeout_ms as i64),
    );
    set_preserving_decor(
        postproc,
        "passes",
        toml_edit::value(config.postproc.passes as i64),
    );
    set_preserving_decor(
        postproc,
        "min_chars",
        toml_edit::value(config.postproc.min_chars as i64),
    );
    set_preserving_decor(
        postproc,
        "instructions",
        toml_edit::value(config.postproc.instructions.clone()),
    );
    write_modes(&mut doc, &config.postproc)?;

    let text = doc.to_string();
    anyhow::ensure!(
        read_back(&text)? == config.postproc,
        "Not saved: the file would not read back with these cleanup settings"
    );
    write_config_atomically(path, &text)?;
    Ok(text)
}

/// Make `mode` the default for new takes in the file at `path`, keeping its
/// comments and every other setting; the caller asks a running Cantrip to
/// reload. A file that does not parse or validate is refused unchanged.
pub fn save_default_mode(path: &Path, mode: Mode) -> Result<()> {
    update_default_mode(path, |_| mode).map(drop)
}

/// Save the mode after the current default, in `Mode::ALL` order and wrapping
/// from the last to the first, as [`save_default_mode`] does; returns it.
pub fn cycle_default_mode(path: &Path) -> Result<Mode> {
    update_default_mode(path, |current| {
        let index = Mode::ALL.iter().position(|mode| *mode == current).unwrap_or(0);
        Mode::ALL[(index + 1) % Mode::ALL.len()]
    })
}

/// Read the saved default, choose the next one, and write it back. Nothing is
/// written unless the result reads back as the original with only the mode
/// changed, so completing the legacy cutover never alters another setting.
fn update_default_mode(path: &Path, choose: impl FnOnce(Mode) -> Mode) -> Result<Mode> {
    let existing = read_config(path)?;
    let mut expected: Config = toml::from_str(&existing)
        .with_context(|| format!("Mode not saved: {} does not parse", path.display()))?;
    expected
        .validate()
        .with_context(|| format!("Mode not saved: {} needs repair", path.display()))?;
    expected.postproc.mode = choose(expected.postproc.mode);
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    write_modes(&mut doc, &expected.postproc)?;
    let text = doc.to_string();
    anyhow::ensure!(
        toml::from_str::<Config>(&text).ok().as_ref() == Some(&expected),
        "Mode not saved: rewriting {} would change another setting",
        path.display()
    );
    if text != existing {
        write_config_atomically(path, &text)?;
    }
    Ok(expected.postproc.mode)
}

/// Make `[postproc]` read back as `wanted`: write `mode`, remove the legacy
/// names, then write each shared or per-mode value that would otherwise read
/// back differently. Keys left alone keep their comments, and a mode's model
/// table the operator never set keeps following the defaults.
fn write_modes(doc: &mut toml_edit::DocumentMut, wanted: &PostprocConfig) -> Result<()> {
    let postproc = ensure_table(doc.as_table_mut(), "postproc")?;
    set_preserving_decor(postproc, "mode", toml_edit::value(wanted.mode.as_str()));
    for name in LEGACY_POSTPROC {
        postproc.remove(name);
    }
    if read_back(&doc.to_string())?.endpoint != wanted.endpoint {
        let postproc = ensure_table(doc.as_table_mut(), "postproc")?;
        set_preserving_decor(postproc, "endpoint", toml_edit::value(wanted.endpoint.as_str()));
    }
    // An absent credential id's default follows the endpoint; compare it afterwards.
    let current = read_back(&doc.to_string())?;
    let postproc = ensure_table(doc.as_table_mut(), "postproc")?;
    if current.api_key_id != wanted.api_key_id {
        // An empty id is how the file says "no credential".
        let id = wanted.api_key_id.as_deref().unwrap_or_default();
        set_preserving_decor(postproc, "api_key_id", toml_edit::value(id));
    }
    for (name, saved, wanted) in [
        ("clean", &current.clean, &wanted.clean),
        ("elevate", &current.elevate, &wanted.elevate),
    ] {
        if saved != wanted {
            write_mode_model(postproc, name, wanted)?;
        }
    }
    Ok(())
}

/// One mode's `[postproc.<name>]` model and reasoning effort; an omitted
/// effort is written empty, because an absent one means the default effort.
fn write_mode_model(
    postproc: &mut toml_edit::Table,
    name: &str,
    wanted: &ModeModelConfig,
) -> Result<()> {
    let table = postproc
        .entry(name)
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .with_context(|| format!("config key 'postproc.{name}' is not a TOML table"))?;
    set_preserving_decor(table, "model", toml_edit::value(wanted.model.as_str()));
    let effort = wanted.reasoning_effort.as_deref().unwrap_or_default();
    set_preserving_decor(table, "reasoning_effort", toml_edit::value(effort));
    Ok(())
}

/// The cleanup settings the engine would load from `text`.
fn read_back(text: &str) -> Result<PostprocConfig> {
    let config: Config =
        toml::from_str(text).context("the edited configuration does not parse")?;
    Ok(config.postproc)
}

/// The file's text, or none when it does not exist yet.
fn read_config(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replace a file that could not be parsed with the operator's repaired text,
/// exactly as typed, once it parses and validates. The original is kept as a
/// private backup beside it, and a concurrent change refuses the replacement.
pub(crate) fn save_repaired(path: &Path, original: &str, edited: &str) -> Result<Config> {
    anyhow::ensure!(
        fs::read_to_string(path)? == original,
        "Configuration changed on disk; reopen repair before saving"
    );
    let config: Config = toml::from_str(edited).context("TOML is not valid yet")?;
    config.validate()?;
    backup_config(path, original)?;
    write_config_atomically(path, edited)?;
    Ok(config)
}

fn write_config_atomically(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Atomic write: temp file in the same directory, then rename, so a crash
    // mid-write can never truncate the user's only config (models.rs convention).
    // The counter keeps concurrent saves from colliding on one temp path.
    let parent = path
        .parent()
        .context("config path has no parent directory")?;
    paths::ensure_dir(parent.to_path_buf())?;
    let tmp = parent.join(format!(
        ".cantrip-config-{}-{}.tmp",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("saving {}", path.display()))
}

fn backup_config(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().context("config path has no parent")?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let backup = parent.join(format!(
        "config.toml.before-repair-{stamp}-{}.bak",
        std::process::id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&backup)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Assign `item` to `key`, carrying over any decor (inline comment) the key
/// already has, so edited values keep their trailing annotations and column
/// alignment instead of being flattened. The key itself, with the comments
/// above it, stays in place. Works on standard and inline tables alike.
fn set_preserving_decor<T: toml_edit::TableLike + ?Sized>(
    table: &mut T,
    key: &str,
    item: toml_edit::Item,
) {
    let mut item = item;
    match table.get_mut(key) {
        Some(slot) => {
            if let (Some(old), Some(value)) = (slot.as_value(), item.as_value_mut()) {
                *value.decor_mut() = old.decor().clone();
            }
            *slot = item;
        }
        None => {
            table.insert(key, item);
        }
    }
}

/// Set `key` to `value`, or remove it entirely when the optional is absent.
fn set_or_remove(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    match value {
        Some(value) => set_preserving_decor(table, key, toml_edit::value(value)),
        None => {
            table.remove(key);
        }
    }
}

fn ensure_table<'a>(root: &'a mut toml_edit::Table, key: &str) -> Result<&'a mut toml_edit::Table> {
    let item = root.entry(key).or_insert(toml_edit::table());
    item.as_table_mut()
        .with_context(|| format!("config key '{key}' is not a TOML table"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::fixture::{sample_config, ConfigFixture};
    use cantrip_engine::config::SttConfig;

    #[test]
    fn save_preserves_comments_and_applies_edits() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        fs::write(
            &path,
            "# top-level comment\ninjection = \"auto\"  # trailing\n\n[stt]\nmodel = \"parakeet-tdt-0.6b-v3-int8\"\n\n[postproc]\nenabled = false\nmodel = \"\"\n",
        )
        .expect("write fixture");

        let config = sample_config();
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save");
        let text = fs::read_to_string(&path).expect("read back");

        assert!(
            text.contains("# top-level comment"),
            "comment must survive: {text}"
        );
        assert!(
            text.contains("injection = \"type\""),
            "edited value present: {text}"
        );
        assert!(
            text.contains("# trailing"),
            "inline comment on an edited key must survive: {text}"
        );

        // Round-trip: the saved file reads back as the edited cleanup, written
        // only in its current shape.
        let parsed: Config = toml::from_str(&text).expect("re-parse");
        assert_eq!(parsed.postproc, config.postproc, "{text}");
        assert_eq!(parsed.injection, InjectionMode::Type);
        let document: toml::Value = toml::from_str(&text).expect("document");
        for key in LEGACY_POSTPROC {
            assert!(
                document["postproc"].get(*key).is_none(),
                "{key} removed: {text}"
            );
        }

        fs::remove_file(&path).ok();
    }

    #[test]
    fn save_removes_optional_key_when_cleared() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        fs::write(
            &path,
            "[stt]\nmodel = \"parakeet-tdt-0.6b-v3-int8\"\nendpoint = \"https://api.xyz/v1\"\napi_key_id = \"abc\"\n",
        )
        .expect("write fixture");

        let config = Config {
            stt: SttConfig {
                model: "parakeet-tdt-0.6b-v3-int8".to_owned(),
                endpoint: None,
                api_key_id: None,
            },
            ..sample_config()
        };
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save");
        let text = fs::read_to_string(&path).expect("read back");
        // The [stt] endpoint/api_key_id must be gone; postproc.endpoint is
        // unrelated and legitimately still present.
        let parsed: Config = toml::from_str(&text).expect("re-parse");
        assert_eq!(parsed.stt.endpoint, None, "stt endpoint removed: {text}");
        assert_eq!(
            parsed.stt.api_key_id, None,
            "stt api_key_id removed: {text}"
        );

        fs::remove_file(&path).ok();
    }

    #[test]
    fn choosing_a_mode_migrates_legacy_cleanup_without_changing_its_meaning() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        let legacy = "# mine\n[postproc]\nenabled = true\nmodel = \"qwen3:8b\"  # local\nreasoning_effort = \"high\"\npersonal = 1\n";
        fs::write(&path, legacy).expect("legacy fixture");
        let mut expected: Config = toml::from_str(legacy).expect("legacy parses");

        save_default_mode(&path, Mode::Elevate).expect("mode saved");

        let text = fs::read_to_string(&path).expect("saved");
        expected.postproc.mode = Mode::Elevate;
        let saved: Config = toml::from_str(&text).expect("saved parses");
        assert_eq!(saved, expected, "{text}");
        let document: toml::Value = toml::from_str(&text).expect("document");
        let postproc = &document["postproc"];
        for key in LEGACY_POSTPROC {
            assert!(postproc.get(*key).is_none(), "{key} removed: {text}");
        }
        assert_eq!(postproc["clean"]["model"].as_str(), Some("qwen3:8b"));
        assert_eq!(postproc["personal"].as_integer(), Some(1));
        assert!(
            postproc.get("elevate").is_none(),
            "an untouched Elevate keeps following its defaults: {text}"
        );
        assert!(text.contains("# mine"));
    }

    #[test]
    fn choosing_a_mode_changes_only_the_mode_and_cycles_in_order() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        // From no file (the shipped Raw default), each mode in order, wrapping.
        for expected in [Mode::Clean, Mode::Elevate, Mode::Raw, Mode::Clean] {
            assert_eq!(cycle_default_mode(&path).expect("cycled"), expected);
            let saved: Config =
                toml::from_str(&fs::read_to_string(&path).expect("saved")).expect("parses");
            assert_eq!(saved.postproc.mode, expected);
        }
        let current = "[postproc]\nmode = \"clean\"  # mine\nendpoint = \"http://localhost:11434/v1\"\n\n[postproc.clean]\nmodel = \"qwen3:8b\"\nreasoning_effort = \"\"\n";
        fs::write(&path, current).expect("current fixture");
        save_default_mode(&path, Mode::Elevate).expect("mode saved");
        assert_eq!(
            fs::read_to_string(&path).expect("saved"),
            current.replace("\"clean\"  # mine", "\"elevate\"  # mine")
        );
    }

    #[test]
    fn choosing_a_mode_refuses_a_file_it_cannot_read_safely() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        for text in [
            "[postproc\nmode = \"raw\"\n",
            "[stt]\nmodel = \"retired-model\"\n",
        ] {
            fs::write(&path, text).expect("fixture");
            assert!(save_default_mode(&path, Mode::Elevate).is_err(), "{text}");
            assert!(cycle_default_mode(&path).is_err(), "{text}");
            assert_eq!(fs::read_to_string(&path).expect("unchanged"), text);
        }
    }

    #[test]
    fn saver_rejects_an_unparseable_existing_file() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        fs::write(&path, "this is [ not toml").expect("write fixture");
        assert!(save_config_preserving(&path, &sample_config(), "this is [ not toml").is_err());
        fs::remove_file(&path).ok();
    }

    #[test]
    fn parsed_invalid_config_keeps_real_values_editable() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        let text = "injection = \"type\"\n[stt]\nmodel = \"retired-model\"\n";
        fs::write(&path, text).expect("write fixture");

        match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                text: loaded_text,
                warning: Some(_),
            } => {
                assert_eq!(config.injection, InjectionMode::Type);
                assert_eq!(config.stt.model, "retired-model");
                assert_eq!(loaded_text, text);
            }
            _ => panic!("parsed validation failure must remain editable"),
        }

        fs::remove_file(path).ok();
    }

    #[test]
    fn malformed_config_is_blocked_without_changing_file() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        let text = b"injection = [not valid TOML";
        fs::write(&path, text).expect("write fixture");

        match load_editable_config(&path) {
            EditableConfigLoad::Blocked { .. } => {}
            EditableConfigLoad::Ready { .. } => {
                panic!("malformed config must disable structured saving")
            }
        }
        assert_eq!(fs::read(&path).expect("read fixture"), text);

        fs::remove_file(path).ok();
    }

    #[test]
    fn corrected_validation_failure_saves_and_reloads() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        fs::write(
            &path,
            "# keep me\ninjection = \"type\"\n[stt]\nmodel = \"retired-model\"\n",
        )
        .expect("write fixture");

        let mut config = match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                warning: Some(_),
                ..
            } => config,
            _ => panic!("fixture must be a parsed validation failure"),
        };
        config.stt.model = "parakeet-tdt-0.6b-v3-int8".to_owned();
        config.validate().expect("correction must validate");
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save corrected config");

        match load_editable_config(&path) {
            EditableConfigLoad::Ready {
                config,
                warning: None,
                ..
            } => {
                assert_eq!(config.injection, InjectionMode::Type);
                assert_eq!(config.stt.model, "parakeet-tdt-0.6b-v3-int8");
            }
            _ => panic!("corrected config must reload without a warning"),
        }
        assert!(
            fs::read_to_string(&path)
                .expect("read corrected config")
                .contains("# keep me"),
            "repair must preserve existing comments"
        );

        fs::remove_file(path).ok();
    }

    #[test]
    fn following_desktop_removes_motion_override_without_losing_unknown_preferences() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        fs::write(&path, "[hud]\nlabels = true\nreduced_motion = true\n# keep personal preference\npersonal_scale = 2\n").expect("fixture");
        let mut config: Config =
            toml::from_str(&fs::read_to_string(&path).expect("read")).expect("parse");
        config.hud.reduced_motion = None;
        let original = fs::read_to_string(&path).expect("original config");
        save_config_preserving(&path, &config, &original).expect("save desktop preference");
        let saved = fs::read_to_string(&path).expect("saved");
        let parsed: Config = toml::from_str(&saved).expect("reparse");
        assert_eq!(parsed.hud.reduced_motion, None);
        assert!(parsed.hud.labels);
        let document: toml::Value = toml::from_str(&saved).expect("document");
        assert_eq!(document["hud"]["personal_scale"].as_integer(), Some(2));
        assert!(saved.contains("# keep personal preference"));
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn saving_refuses_to_adopt_concurrent_disk_edits() {
        let fixture = ConfigFixture::new();
        let path = fixture.path();
        let original = "injection = \"auto\"\n";
        let changed = "injection = \"clipboard\"\n# external edit\n";
        fs::write(&path, changed).expect("external edit");
        assert!(save_config_preserving(&path, &sample_config(), original).is_err());
        assert_eq!(
            fs::read_to_string(&path).expect("preserved config"),
            changed
        );
        fs::remove_file(path).expect("cleanup");
    }
}
