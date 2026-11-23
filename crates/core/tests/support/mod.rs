//! Fakes for every port, each logging the calls it receives.
#![allow(dead_code)]

use cantrip_core::ports::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ───────────── transport ─────────────

type Handler = dyn Fn(&str, &serde_json::Value) -> Result<HttpReply, TransportError> + Send + Sync;

pub struct FakeTransport {
    pub calls: Mutex<Vec<(String, serde_json::Value)>>,
    handler: Box<Handler>,
}

impl FakeTransport {
    pub fn new(
        f: impl Fn(&str, &serde_json::Value) -> Result<HttpReply, TransportError>
            + Send
            + Sync
            + 'static,
    ) -> Arc<Self> {
        Arc::new(FakeTransport {
            calls: Mutex::new(Vec::new()),
            handler: Box::new(f),
        })
    }
    pub fn count(&self, path: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u.ends_with(path))
            .count()
    }
    pub fn total(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    pub fn bodies(&self, path: &str) -> Vec<serde_json::Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(u, _)| u.ends_with(path))
            .map(|(_, b)| b.clone())
            .collect()
    }
}

impl Transport for FakeTransport {
    fn post_json(
        &self,
        url: &str,
        _key: &str,
        body: &serde_json::Value,
        _timeout: Option<Duration>,
    ) -> Result<HttpReply, TransportError> {
        self.calls
            .lock()
            .unwrap()
            .push((url.to_string(), body.clone()));
        (self.handler)(url, body)
    }
}

pub fn ok(body: &str) -> Result<HttpReply, TransportError> {
    Ok(HttpReply {
        status: 200,
        body: body.to_string(),
        retry_after: None,
    })
}

pub fn status(code: u16) -> Result<HttpReply, TransportError> {
    Ok(HttpReply {
        status: code,
        body: "{\"error\":{}}".into(),
        retry_after: None,
    })
}

pub fn transcript_json(text: &str) -> String {
    serde_json::json!({ "text": text, "usage": { "cost": 0.0003 } }).to_string()
}

pub fn chat_json(content: &str, finish: &str) -> String {
    // Leading whitespace mimics OpenRouter keepalives.
    format!(
        "\n   \n{}",
        serde_json::json!({ "choices": [{ "finish_reason": finish, "message": { "content": content } }] })
    )
}

/// Transcribes to `heard`; rewrites to `rewritten` (or per `rewrite`).
pub fn speech(
    heard: &'static str,
    rewrite: impl Fn() -> Result<HttpReply, TransportError> + Send + Sync + 'static,
) -> Arc<FakeTransport> {
    FakeTransport::new(move |url, _| {
        if url.ends_with("/audio/transcriptions") {
            ok(&transcript_json(heard))
        } else {
            rewrite()
        }
    })
}

// ───────────── desktop ─────────────

pub fn app(id: &str, pid: u32) -> AppIdentity {
    AppIdentity {
        app_id: id.into(),
        pid,
        instance: format!("{pid}:1"),
    }
}

pub fn target_a() -> Target {
    Target {
        app: app("org.example.A", 10),
        window: WindowRef("A1".into()),
        field: Some(FieldRef("f".into())),
    }
}

pub fn focus_of(t: &Target) -> FocusNow {
    FocusNow {
        app: t.app.clone(),
        window: t.window.clone(),
    }
}

pub fn focus_b() -> FocusNow {
    FocusNow {
        app: app("org.example.B", 20),
        window: WindowRef("B1".into()),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InsertMode {
    /// Write and expose the edit.
    Apply,
    /// Write but the field reads back unchanged (e.g. autocorrect / opaque).
    Opaque,
    Ambiguous,
    Refused,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PasteMode {
    /// Paste the clipboard's current text into the field.
    Apply,
    /// Accepted but nothing happens.
    Swallow,
    Unaddressable,
    Failed,
}

pub struct FakeDesktop {
    pub capture: Mutex<Option<Target>>,
    pub capture_delay: Mutex<Duration>,
    pub focus: Mutex<Option<FocusNow>>,
    /// After this many focus_now calls, switch focus to the given value.
    pub switch_after: Mutex<Option<(usize, Option<FocusNow>)>>,
    pub focus_calls: AtomicUsize,
    pub field: Mutex<Option<FieldState>>,
    pub support: Mutex<FieldSupport>,
    pub insert_mode: Mutex<InsertMode>,
    pub paste_mode: Mutex<PasteMode>,
    pub addressable: Mutex<bool>,
    pub permission: Mutex<Option<String>>,
    pub epoch: AtomicU64,
    pub alive: Mutex<bool>,
    pub events: Mutex<Vec<String>>,
    pub clipboard: Mutex<Option<Arc<FakeClipboard>>>,
}

impl FakeDesktop {
    pub fn new(t: Target) -> Arc<Self> {
        Arc::new(FakeDesktop {
            focus: Mutex::new(Some(focus_of(&t))),
            capture: Mutex::new(Some(t)),
            capture_delay: Mutex::new(Duration::ZERO),
            switch_after: Mutex::new(None),
            focus_calls: AtomicUsize::new(0),
            field: Mutex::new(Some(FieldState {
                text: "Hello ".into(),
                selection: (6, 6),
                complete: true,
            })),
            support: Mutex::new(FieldSupport::DirectInsert),
            insert_mode: Mutex::new(InsertMode::Apply),
            paste_mode: Mutex::new(PasteMode::Apply),
            addressable: Mutex::new(true),
            permission: Mutex::new(None),
            epoch: AtomicU64::new(1),
            alive: Mutex::new(true),
            events: Mutex::new(Vec::new()),
            clipboard: Mutex::new(None),
        })
    }
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    pub fn inserts(&self) -> usize {
        self.events().iter().filter(|e| *e == "insert").count()
    }
    pub fn pastes(&self) -> usize {
        self.events().iter().filter(|e| *e == "paste").count()
    }
    pub fn field_text(&self) -> String {
        self.field
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.text.clone())
            .unwrap_or_default()
    }
    fn apply(&self, text: &str) {
        let mut f = self.field.lock().unwrap();
        if let Some(fs) = f.as_mut() {
            let chars: Vec<char> = fs.text.chars().collect();
            let (a, b) = fs.selection;
            let new: String = chars[..a].iter().collect::<String>()
                + text
                + &chars[b..].iter().collect::<String>();
            let caret = a + text.chars().count();
            fs.text = new;
            fs.selection = (caret, caret);
        }
    }
}

impl Desktop for FakeDesktop {
    fn capture_target(&self) -> Option<Target> {
        std::thread::sleep(*self.capture_delay.lock().unwrap());
        self.capture.lock().unwrap().clone()
    }
    fn focus_now(&self) -> Option<FocusNow> {
        let n = self.focus_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((after, f)) = self.switch_after.lock().unwrap().clone() {
            if n > after {
                *self.focus.lock().unwrap() = f;
            }
        }
        self.focus.lock().unwrap().clone()
    }
    fn app_alive(&self, _app: &AppIdentity) -> bool {
        *self.alive.lock().unwrap()
    }
    fn missing_delivery_permission(&self) -> Option<String> {
        self.permission.lock().unwrap().clone()
    }
    fn session_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
    fn field_support(&self, _t: &Target) -> FieldSupport {
        self.support.lock().unwrap().clone()
    }
    fn read_field(&self, _t: &Target) -> Option<FieldState> {
        self.field.lock().unwrap().clone()
    }
    fn insert_text(&self, _t: &Target, text: &str) -> InsertResult {
        self.events.lock().unwrap().push("insert".into());
        match *self.insert_mode.lock().unwrap() {
            InsertMode::Apply => {
                self.apply(text);
                InsertResult::Done
            }
            InsertMode::Opaque => InsertResult::Done,
            InsertMode::Ambiguous => InsertResult::Ambiguous,
            InsertMode::Refused => InsertResult::Refused,
        }
    }
    fn can_address_paste(&self, _app: &AppIdentity) -> bool {
        *self.addressable.lock().unwrap()
    }
    fn post_paste(&self, _t: &Target) -> PasteResult {
        match *self.paste_mode.lock().unwrap() {
            PasteMode::Unaddressable => PasteResult::Unaddressable,
            PasteMode::Failed => PasteResult::Failed,
            mode => {
                self.events.lock().unwrap().push("paste".into());
                if mode == PasteMode::Apply {
                    let text = self
                        .clipboard
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|c| c.text());
                    if let Some(t) = text {
                        self.apply(&t);
                    }
                }
                PasteResult::Posted
            }
        }
    }
}

// ───────────── clipboard ─────────────

/// Identity-based clipboard: every write (Cantrip's or another app's) bumps
/// the generation; Cantrip's token is the generation it wrote.
pub struct FakeClipboard {
    pub generation: AtomicU64,
    pub items: Mutex<ClipSnapshot>,
    pub fail_writes: Mutex<bool>,
    pub deny_snapshot: Mutex<bool>,
    pub writes: AtomicUsize,
    pub restores: AtomicUsize,
    /// Simulate another app copying after N `unchanged_since` checks.
    pub external_after_checks: Mutex<Option<(usize, String)>>,
    pub checks: AtomicUsize,
}

impl FakeClipboard {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeClipboard {
            generation: AtomicU64::new(1),
            items: Mutex::new(ClipSnapshot {
                items: vec![vec![
                    ("text/plain".into(), b"old".to_vec()),
                    ("image/png".into(), vec![0x89, b'P', b'N', b'G']),
                ]],
            }),
            fail_writes: Mutex::new(false),
            deny_snapshot: Mutex::new(false),
            writes: AtomicUsize::new(0),
            restores: AtomicUsize::new(0),
            external_after_checks: Mutex::new(None),
            checks: AtomicUsize::new(0),
        })
    }
    /// Another app copies (rich or plain) `text`.
    pub fn external_copy(&self, text: &str, rich: bool) {
        let mut item = vec![("text/plain".to_string(), text.as_bytes().to_vec())];
        if rich {
            item.push(("text/html".into(), format!("<b>{text}</b>").into_bytes()));
        }
        *self.items.lock().unwrap() = ClipSnapshot { items: vec![item] };
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
    pub fn text(&self) -> Option<String> {
        self.items
            .lock()
            .unwrap()
            .items
            .first()?
            .iter()
            .find(|(t, _)| t == "text/plain")
            .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
    }
    pub fn snapshot_now(&self) -> ClipSnapshot {
        self.items.lock().unwrap().clone()
    }
}

impl Clipboard for FakeClipboard {
    fn snapshot(&self) -> Option<ClipSnapshot> {
        if *self.deny_snapshot.lock().unwrap() {
            None
        } else {
            Some(self.items.lock().unwrap().clone())
        }
    }
    fn write_text(&self, text: &str, transient: bool) -> Result<ClipToken, String> {
        if *self.fail_writes.lock().unwrap() {
            return Err("clipboard write failed".into());
        }
        assert!(transient, "Cantrip's text is always marked transient");
        self.writes.fetch_add(1, Ordering::SeqCst);
        *self.items.lock().unwrap() = ClipSnapshot {
            items: vec![vec![("text/plain".into(), text.as_bytes().to_vec())]],
        };
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(ClipToken(g))
    }
    fn unchanged_since(&self, token: &ClipToken) -> bool {
        let n = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        let ext = self.external_after_checks.lock().unwrap().clone();
        if let Some((after, text)) = ext {
            if n > after {
                *self.external_after_checks.lock().unwrap() = None;
                self.external_copy(&text, true);
            }
        }
        self.generation.load(Ordering::SeqCst) == token.0
    }
    fn restore(&self, snapshot: &ClipSnapshot, token: &ClipToken) -> bool {
        if self.generation.load(Ordering::SeqCst) != token.0 {
            return false;
        }
        self.restores.fetch_add(1, Ordering::SeqCst);
        *self.items.lock().unwrap() = snapshot.clone();
        self.generation.fetch_add(1, Ordering::SeqCst);
        true
    }
}

// ───────────── audio, secrets, local ─────────────

pub struct FakeRecording {
    path: PathBuf,
    samples: Vec<i16>,
}

impl Recording for FakeRecording {
    fn level(&self) -> f32 {
        0.4
    }
    fn stop(self: Box<Self>) -> Result<PathBuf, String> {
        std::fs::write(
            &self.path,
            cantrip_core::wav::encode_pcm16(&self.samples, 16_000),
        )
        .map_err(|e| e.to_string())?;
        Ok(self.path)
    }
    fn cancel(self: Box<Self>) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct FakeRecorder {
    pub blocked: Mutex<Option<MicError>>,
}

impl FakeRecorder {
    pub fn new() -> Arc<Self> {
        Arc::new(FakeRecorder {
            blocked: Mutex::new(None),
        })
    }
}

impl Recorder for FakeRecorder {
    fn devices(&self) -> Vec<(String, String)> {
        vec![("default".into(), "Default".into())]
    }
    fn start(
        &self,
        _device: Option<&str>,
        wav_path: &Path,
    ) -> Result<Box<dyn Recording>, MicError> {
        if let Some(e) = self.blocked.lock().unwrap().clone() {
            return Err(e);
        }
        // Write a partial file at once, like a real recorder.
        std::fs::write(wav_path, cantrip_core::wav::encode_pcm16(&[], 16_000)).unwrap();
        let samples = (0..16_000)
            .map(|i| ((i as f32 * 0.07).sin() * 8000.0) as i16)
            .collect();
        Ok(Box::new(FakeRecording {
            path: wav_path.to_path_buf(),
            samples,
        }))
    }
}

pub struct FakeSecrets(pub Mutex<Result<Option<String>, SecretError>>);

impl FakeSecrets {
    pub fn with_key() -> Arc<Self> {
        Arc::new(FakeSecrets(Mutex::new(Ok(Some("sk-test".into())))))
    }
    pub fn none() -> Arc<Self> {
        Arc::new(FakeSecrets(Mutex::new(Ok(None))))
    }
    pub fn broken() -> Arc<Self> {
        Arc::new(FakeSecrets(Mutex::new(Err(SecretError::Unavailable(
            "locked".into(),
        )))))
    }
}

impl SecretStore for FakeSecrets {
    fn get_key(&self) -> Result<Option<String>, SecretError> {
        self.0.lock().unwrap().clone()
    }
    fn set_key(&self, key: &str) -> Result<(), SecretError> {
        *self.0.lock().unwrap() = Ok(Some(key.into()));
        Ok(())
    }
    fn delete_key(&self) -> Result<(), SecretError> {
        *self.0.lock().unwrap() = Ok(None);
        Ok(())
    }
}

pub struct FakeLocal {
    pub avail: Mutex<LocalAvailability>,
    pub runs: AtomicUsize,
    pub checks: AtomicUsize,
}

impl FakeLocal {
    pub fn new(a: LocalAvailability) -> Arc<Self> {
        Arc::new(FakeLocal {
            avail: Mutex::new(a),
            runs: AtomicUsize::new(0),
            checks: AtomicUsize::new(0),
        })
    }
}

impl LocalRecognizer for FakeLocal {
    fn availability(&self) -> LocalAvailability {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.avail.lock().unwrap().clone()
    }
    fn recognize_on_device(&self, _wav: &Path) -> Result<String, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok("local words".into())
    }
}
