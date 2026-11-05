//! How every surface presents a mode and the pipeline a take runs: the HUD,
//! the macOS menu and both Settings windows read the same names, glyphs,
//! promises, plain-language stages and privacy cue from here. The engine's
//! [`PipelineDisplay`] supplies the facts; friendly model and provider names
//! come from small tables, and the raw ids stay available for tooltips and
//! Settings details.

use cantrip_engine::config::{Mode, PipelineDisplay};
use cantrip_engine::models::PARAKEET_V3_INT8;
use std::borrow::Cow;
use std::net::IpAddr;

/// The computer Cantrip runs on, as its windows and HUD name it.
#[cfg(target_os = "macos")]
pub const THIS_DEVICE: &str = "this Mac";
#[cfg(not(target_os = "macos"))]
pub const THIS_DEVICE: &str = "this computer";

/// Columns and rows of every mode glyph.
pub const GLYPH_SIZE: usize = 5;

/// Display names for the models Cantrip documents, by the id's last path
/// segment. Any other model shows that segment as configured.
const MODEL_NAMES: [(&str, &str); 6] = [
    (PARAKEET_V3_INT8.dir_name, "Parakeet v3"),
    ("gpt-6.1-sol", "GPT-6.1 Sol"),
    ("gpt-4o-mini-transcribe", "GPT-4o mini Transcribe"),
    ("gpt-4o-transcribe", "GPT-4o Transcribe"),
    ("whisper-1", "Whisper"),
    ("whisper-large-v3", "Whisper Large v3"),
];

/// Display names for the providers Cantrip documents, by host or parent
/// domain. Any other provider shows its host.
const PROVIDER_NAMES: [(&str, &str); 7] = [
    ("openrouter.ai", "OpenRouter"),
    ("api.openai.com", "OpenAI"),
    ("api.typesafe.ai", "TypeSafe"),
    ("api.groq.com", "Groq"),
    ("api.elevenlabs.io", "ElevenLabs"),
    ("api.deepgram.com", "Deepgram"),
    ("api.mistral.ai", "Mistral"),
];

/// A mode's mark, lit cells on a 5×5 grid from the top row: Raw quotes your
/// exact words, Clean shortens the lines, Elevate's chevrons raise them.
pub const fn glyph(mode: Mode) -> [&'static str; GLYPH_SIZE] {
    match mode {
        Mode::Raw => ["##.##", "##.##", ".#..#", "#..#.", "....."],
        Mode::Clean => ["#####", ".....", "###..", ".....", "#...."],
        Mode::Elevate => ["..#..", ".#.#.", "#.#.#", ".#.#.", "#...#"],
    }
}

/// The lit `(column, row)` cells of `mode`'s glyph.
pub fn glyph_cells(mode: Mode) -> impl Iterator<Item = (usize, usize)> {
    glyph(mode)
        .into_iter()
        .enumerate()
        .flat_map(|(row, cells)| {
            cells
                .bytes()
                .enumerate()
                .filter(|(_, cell)| *cell == b'#')
                .map(move |(column, _)| (column, row))
        })
}

/// What the mode promises to do with your words.
pub const fn promise(mode: Mode) -> &'static str {
    match mode {
        Mode::Raw => "Exact words",
        Mode::Clean => "Tidy & concise",
        Mode::Elevate => "Expert rewrite",
    }
}

/// One stage of a take, in the order it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Transcribe,
    /// Vocabulary aliases replace misheard terms with their spellings.
    Spellings,
    Refine,
}

impl Step {
    /// The stage at work, as the HUD lights it.
    pub const fn working(self) -> &'static str {
        match self {
            Self::Transcribe => "transcribing…",
            Self::Spellings => "fixing spellings…",
            Self::Refine => "refining…",
        }
    }
}

/// Where a stage runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// Inside Cantrip on this computer.
    Here,
    /// Another program on this computer, at a loopback host.
    LocalServer(String),
    /// A provider at this host.
    Provider(String),
    /// A provider an older engine did not name.
    Unnamed,
}

impl Place {
    fn at(host: &str) -> Self {
        let address = host.parse::<IpAddr>().ok();
        if host == "localhost"
            || host.ends_with(".localhost")
            || address.is_some_and(|address| address.is_loopback())
        {
            Self::LocalServer(host.to_owned())
        } else {
            Self::Provider(host.to_owned())
        }
    }

    /// The take leaves this computer.
    pub fn leaves(&self) -> bool {
        matches!(self, Self::Provider(_) | Self::Unnamed)
    }

    /// The name people know it by.
    pub fn name(&self) -> Cow<'static, str> {
        match self {
            Self::Here => Cow::Owned(THIS_DEVICE.to_owned()),
            Self::LocalServer(_) => Cow::Borrowed("a local server"),
            Self::Provider(host) => PROVIDER_NAMES
                .iter()
                .find(|(domain, _)| {
                    host == domain
                        || host
                            .strip_suffix(domain)
                            .is_some_and(|sub| sub.ends_with('.'))
                })
                .map_or_else(
                    || Cow::Owned(host.clone()),
                    |(_, name)| Cow::Borrowed(*name),
                ),
            Self::Unnamed => Cow::Borrowed("a provider"),
        }
    }

    /// The host behind the name, for details.
    pub fn host(&self) -> Option<&str> {
        match self {
            Self::LocalServer(host) | Self::Provider(host) => Some(host),
            Self::Here | Self::Unnamed => None,
        }
    }
}

/// A model id as people know it.
pub fn model_name(id: &str) -> &str {
    let segment = id.rsplit('/').next().unwrap_or(id);
    MODEL_NAMES
        .iter()
        .find(|(known, _)| *known == segment)
        .map_or(segment, |(_, name)| name)
}

fn count(number: usize, singular: &str, plural: &str) -> String {
    format!("{number} {}", if number == 1 { singular } else { plural })
}

fn names(places: &[Place]) -> String {
    let names: Vec<_> = places.iter().map(Place::name).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => one.to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// What reaches a program beyond Cantrip, named by where it goes.
enum Route {
    Nowhere,
    Audio(String),
    Text(String),
    /// Audio and text reach the same places.
    Both(String),
    Split(String, String),
}

fn capitalized(text: &str) -> String {
    let mut characters = text.chars();
    characters.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(characters).collect()
    })
}

/// One mode's pipeline, as people read it.
#[derive(Clone, Copy)]
pub struct ModeView<'a> {
    pipeline: &'a PipelineDisplay,
}

impl<'a> ModeView<'a> {
    pub fn new(pipeline: &'a PipelineDisplay) -> Self {
        Self { pipeline }
    }

    pub fn mode(self) -> Mode {
        self.pipeline.mode
    }

    pub fn name(self) -> &'static str {
        self.pipeline.mode.label()
    }

    fn refines(self) -> bool {
        self.pipeline.llm_model.is_some()
    }

    fn fixes_spellings(self) -> bool {
        self.pipeline.mode != Mode::Raw && self.pipeline.spellings > 0
    }

    /// The mode's promise, unless its configuration cannot keep it.
    pub fn headline(self) -> &'static str {
        if self.pipeline.mode == Mode::Raw || self.refines() {
            promise(self.pipeline.mode)
        } else if self.fixes_spellings() {
            "Spelling fixes only"
        } else {
            "No model set"
        }
    }

    /// The stages this take runs, in order: Spellings only where aliases
    /// exist, Refine only with a model.
    pub fn steps(self) -> Vec<Step> {
        let mut steps = vec![Step::Transcribe];
        if self.fixes_spellings() {
            steps.push(Step::Spellings);
        }
        if self.refines() {
            steps.push(Step::Refine);
        }
        steps
    }

    /// Where `step` runs.
    pub fn places(self, step: Step) -> Vec<Place> {
        let pipeline = self.pipeline;
        match step {
            Step::Transcribe if pipeline.stt_local => vec![Place::Here],
            Step::Transcribe => {
                vec![pipeline
                    .stt_host
                    .as_deref()
                    .map_or(Place::Unnamed, Place::at)]
            }
            Step::Spellings => vec![Place::Here],
            Step::Refine if pipeline.llm_hosts.is_empty() => vec![Place::Unnamed],
            Step::Refine => pipeline
                .llm_hosts
                .iter()
                .map(|host| Place::at(host))
                .collect(),
        }
    }

    /// Everywhere beyond Cantrip that `step` sends the take.
    fn destinations(self, step: Step) -> Vec<Place> {
        if self.steps().contains(&step) {
            self.places(step)
                .into_iter()
                .filter(|place| *place != Place::Here)
                .collect()
        } else {
            Vec::new()
        }
    }

    /// The providers that receive the take, in order, each once.
    pub fn providers(self) -> Vec<Place> {
        let mut providers = Vec::new();
        for place in [Step::Transcribe, Step::Refine]
            .into_iter()
            .flat_map(|step| self.destinations(step))
            .filter(Place::leaves)
        {
            if !providers.contains(&place) {
                providers.push(place);
            }
        }
        providers
    }

    /// `step` sends the take to `place`.
    pub fn sends(self, step: Step, place: &Place) -> bool {
        self.destinations(step).contains(place)
    }

    /// Where the audio and the text go beyond Cantrip.
    fn routes(self) -> Route {
        let audio = self.destinations(Step::Transcribe);
        let text = self.destinations(Step::Refine);
        match (audio.is_empty(), text.is_empty()) {
            (true, true) => Route::Nowhere,
            (false, true) => Route::Audio(names(&audio)),
            (true, false) => Route::Text(names(&text)),
            (false, false) if audio == text => Route::Both(names(&text)),
            (false, false) => Route::Split(names(&audio), names(&text)),
        }
    }

    /// Where the take goes beyond Cantrip, in a phrase: "on this Mac",
    /// "text to OpenRouter", "audio to OpenAI, text to OpenRouter".
    pub fn privacy(self) -> String {
        match self.routes() {
            Route::Nowhere => format!("on {THIS_DEVICE}"),
            Route::Audio(audio) => format!("audio to {audio}"),
            Route::Text(text) => format!("text to {text}"),
            Route::Both(both) => format!("audio and text to {both}"),
            Route::Split(audio, text) => format!("audio to {audio}, text to {text}"),
        }
    }

    /// The menu's one line under the mode's name.
    pub fn subtitle(self) -> String {
        format!("{} · {}", self.headline(), self.privacy())
    }

    /// What the mode does to the text, in one short sentence.
    pub fn effect(self) -> String {
        let spellings = count(self.pipeline.spellings, "spelling", "spellings");
        let change = match self.pipeline.mode {
            Mode::Raw => return "Delivers the words exactly as recognized.".to_owned(),
            Mode::Clean => "trims filler and repetition, keeping your meaning",
            Mode::Elevate => "rewrites what you said as a clearer, more articulate version",
        };
        match (self.fixes_spellings(), self.refines()) {
            (true, true) => format!("Fixes {spellings}, then {change}."),
            (false, true) => format!("{}.", capitalized(change)),
            (true, false) => format!("Fixes {spellings} only; no model is set."),
            (false, false) => {
                "Delivers the words as recognized; no model or spellings are set.".to_owned()
            }
        }
    }

    /// What leaves this computer, then the model that shapes the text.
    pub fn summary(self) -> String {
        let leaves = match self.routes() {
            Route::Nowhere => format!("Nothing leaves {THIS_DEVICE}"),
            Route::Audio(audio) => format!("Audio goes to {audio}"),
            Route::Text(text) => format!("Text goes to {text}"),
            Route::Both(both) => format!("Audio and text go to {both}"),
            Route::Split(audio, text) => format!("Audio goes to {audio}, text to {text}"),
        };
        let model = match &self.pipeline.llm_model {
            Some(model) => format!("Refines with {}", model_name(model)),
            None if self.pipeline.mode == Mode::Raw => {
                format!("Transcribes with {}", model_name(&self.pipeline.stt_model))
            }
            None => "No model set".to_owned(),
        };
        format!("{leaves} · {model}")
    }

    /// `step`'s plain-language name: "Transcribe on this Mac", "Fix
    /// spellings", "Refine with GPT-6.1 Sol".
    pub fn title(self, step: Step) -> String {
        match step {
            Step::Transcribe => match self.places(step).first() {
                Some(Place::Here) | None => format!("Transcribe on {THIS_DEVICE}"),
                Some(place) => format!("Transcribe at {}", place.name()),
            },
            Step::Spellings => "Fix spellings".to_owned(),
            Step::Refine => format!(
                "Refine with {}",
                self.pipeline.llm_model.as_deref().map_or("", model_name)
            ),
        }
    }

    /// The model or count behind `step`'s title.
    pub fn detail(self, step: Step) -> String {
        match step {
            Step::Transcribe => model_name(&self.pipeline.stt_model).to_owned(),
            Step::Spellings => count(
                self.pipeline.spellings,
                "vocabulary term",
                "vocabulary terms",
            ),
            Step::Refine => format!("at {}", names(&self.places(step))),
        }
    }

    /// The raw ids behind `step`: its model id and every host it reaches.
    pub fn raw(self, step: Step) -> String {
        let model = match step {
            Step::Transcribe => Some(self.pipeline.stt_model.as_str()),
            Step::Spellings => None,
            Step::Refine => self.pipeline.llm_model.as_deref(),
        };
        let places = self.places(step);
        let hosts: Vec<_> = places.iter().filter_map(Place::host).collect();
        let mut raw = model.unwrap_or_default().to_owned();
        if !hosts.is_empty() {
            if !raw.is_empty() {
                raw.push_str(" at ");
            }
            raw.push_str(&hosts.join(", "));
        }
        raw
    }

    /// Each stage on its own line with its raw ids, for a tooltip.
    pub fn tooltip(self) -> String {
        self.steps()
            .into_iter()
            .map(|step| {
                let raw = self.raw(step);
                let mut line = format!("{} — {}", self.title(step), self.detail(step));
                if !raw.is_empty() {
                    line.push_str(&format!(" ({raw})"));
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The stages in one line: "Transcribe on this Mac · Fix spellings ·
    /// Refine with GPT-6.1 Sol".
    pub fn stages(self) -> String {
        self.steps()
            .into_iter()
            .map(|step| self.title(step))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pipeline(mode: Mode) -> PipelineDisplay {
        PipelineDisplay {
            mode,
            stt_model: PARAKEET_V3_INT8.dir_name.to_owned(),
            stt_local: true,
            stt_host: None,
            llm_model: (mode != Mode::Raw).then(|| "openai/gpt-6.1-sol".to_owned()),
            llm_hosts: if mode == Mode::Raw {
                Vec::new()
            } else {
                vec!["openrouter.ai".to_owned()]
            },
            spellings: 2,
        }
    }

    #[test]
    fn each_mode_leads_with_its_name_promise_and_where_the_take_goes() {
        let lines: Vec<_> = Mode::ALL
            .into_iter()
            .map(|mode| {
                let pipeline = pipeline(mode);
                let view = ModeView::new(&pipeline);
                (view.name(), view.subtitle())
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("Raw", format!("Exact words · on {THIS_DEVICE}")),
                ("Clean", "Tidy & concise · text to OpenRouter".to_owned()),
                ("Elevate", "Expert rewrite · text to OpenRouter".to_owned()),
            ]
        );
    }

    #[test]
    fn stages_read_as_plain_words_with_friendly_names_and_keep_raw_ids_for_details() {
        let clean = pipeline(Mode::Clean);
        let view = ModeView::new(&clean);
        assert_eq!(
            view.steps(),
            [Step::Transcribe, Step::Spellings, Step::Refine]
        );
        assert_eq!(
            view.stages(),
            format!("Transcribe on {THIS_DEVICE} · Fix spellings · Refine with GPT-6.1 Sol")
        );
        assert_eq!(view.detail(Step::Transcribe), "Parakeet v3");
        assert_eq!(view.detail(Step::Spellings), "2 vocabulary terms");
        assert_eq!(view.detail(Step::Refine), "at OpenRouter");
        assert_eq!(
            view.tooltip(),
            format!(
                "Transcribe on {THIS_DEVICE} — Parakeet v3 (parakeet-tdt-0.6b-v3-int8)\n\
                 Fix spellings — 2 vocabulary terms\n\
                 Refine with GPT-6.1 Sol — at OpenRouter (openai/gpt-6.1-sol at openrouter.ai)"
            )
        );
        // Raw runs neither aliases nor a model, whatever the vocabulary holds.
        let raw = pipeline(Mode::Raw);
        assert_eq!(ModeView::new(&raw).steps(), [Step::Transcribe]);
        // Spellings appear only where aliases exist; unknown ids show as configured.
        let mut plain = pipeline(Mode::Elevate);
        plain.spellings = 0;
        plain.llm_model = Some("anthropic/claude-sonnet-5".to_owned());
        let view = ModeView::new(&plain);
        assert_eq!(view.steps(), [Step::Transcribe, Step::Refine]);
        assert_eq!(view.title(Step::Refine), "Refine with claude-sonnet-5");
    }

    #[test]
    fn settings_explain_the_change_what_leaves_and_the_model() {
        let explained: Vec<_> = Mode::ALL
            .into_iter()
            .map(|mode| {
                let pipeline = pipeline(mode);
                let view = ModeView::new(&pipeline);
                (view.effect(), view.summary())
            })
            .collect();
        assert_eq!(
            explained,
            [
                (
                    "Delivers the words exactly as recognized.".to_owned(),
                    format!("Nothing leaves {THIS_DEVICE} · Transcribes with Parakeet v3"),
                ),
                (
                    "Fixes 2 spellings, then trims filler and repetition, keeping your meaning."
                        .to_owned(),
                    "Text goes to OpenRouter · Refines with GPT-6.1 Sol".to_owned(),
                ),
                (
                    "Fixes 2 spellings, then rewrites what you said as a clearer, more \
                     articulate version."
                        .to_owned(),
                    "Text goes to OpenRouter · Refines with GPT-6.1 Sol".to_owned(),
                ),
            ]
        );
        // A mode left without a model says what it still does.
        let mut aliases_only = pipeline(Mode::Clean);
        aliases_only.llm_model = None;
        aliases_only.llm_hosts.clear();
        let view = ModeView::new(&aliases_only);
        assert_eq!(view.headline(), "Spelling fixes only");
        assert_eq!(view.effect(), "Fixes 2 spellings only; no model is set.");
        assert_eq!(
            view.summary(),
            format!("Nothing leaves {THIS_DEVICE} · No model set")
        );
    }

    #[test]
    fn the_privacy_cue_names_what_leaves_and_never_assumes_it_stays() {
        let mut cloud = pipeline(Mode::Elevate);
        cloud.stt_local = false;
        cloud.stt_model = "gpt-4o-mini-transcribe".to_owned();
        cloud.stt_host = Some("api.openai.com".to_owned());
        cloud.llm_hosts = vec!["eu.openrouter.ai".to_owned(), "api.typesafe.ai".to_owned()];
        let view = ModeView::new(&cloud);
        assert_eq!(
            view.privacy(),
            "audio to OpenAI, text to OpenRouter and TypeSafe"
        );
        assert_eq!(view.title(Step::Transcribe), "Transcribe at OpenAI");
        assert_eq!(view.detail(Step::Transcribe), "GPT-4o mini Transcribe");
        assert_eq!(
            view.raw(Step::Transcribe),
            "gpt-4o-mini-transcribe at api.openai.com"
        );
        assert_eq!(
            view.summary(),
            "Audio goes to OpenAI, text to OpenRouter and TypeSafe · Refines with GPT-6.1 Sol"
        );
        let openai = Place::Provider("api.openai.com".to_owned());
        assert!(view.sends(Step::Transcribe, &openai) && !view.sends(Step::Refine, &openai));
        // One provider for audio and text is named once.
        cloud.stt_host = Some("openrouter.ai".to_owned());
        cloud.llm_hosts = vec!["openrouter.ai".to_owned()];
        assert_eq!(
            ModeView::new(&cloud).privacy(),
            "audio and text to OpenRouter"
        );
        assert_eq!(
            ModeView::new(&cloud).summary(),
            "Audio and text go to OpenRouter · Refines with GPT-6.1 Sol"
        );
        // A server on this computer receives the text without it leaving.
        let mut local = pipeline(Mode::Clean);
        local.llm_hosts = vec!["127.0.0.1".to_owned()];
        let view = ModeView::new(&local);
        assert!(view.providers().is_empty());
        assert_eq!(view.privacy(), "text to a local server");
        assert_eq!(
            view.summary(),
            "Text goes to a local server · Refines with GPT-6.1 Sol"
        );
        // An older engine that names no hosts is never read as local.
        let mut older = pipeline(Mode::Clean);
        older.stt_local = false;
        older.stt_host = None;
        older.llm_hosts.clear();
        let view = ModeView::new(&older);
        assert_eq!(view.providers(), [Place::Unnamed]);
        assert_eq!(view.privacy(), "audio and text to a provider");
        // Other providers show their host.
        let mut other = pipeline(Mode::Clean);
        other.llm_hosts = vec!["llm.example.com".to_owned()];
        assert_eq!(ModeView::new(&other).privacy(), "text to llm.example.com");
    }

    #[test]
    fn glyphs_fill_the_shared_grid_and_differ_by_mode() {
        let shapes: Vec<Vec<_>> = Mode::ALL
            .into_iter()
            .map(|mode| glyph_cells(mode).collect())
            .collect();
        for (mode, cells) in Mode::ALL.into_iter().zip(&shapes) {
            assert!(glyph(mode).iter().all(|row| row.len() == GLYPH_SIZE));
            assert!(!cells.is_empty(), "{mode:?}");
        }
        assert!(shapes[0] != shapes[1] && shapes[1] != shapes[2] && shapes[0] != shapes[2]);
    }
}
