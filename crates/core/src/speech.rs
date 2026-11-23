//! OpenRouter speech and rewrite contracts (DESIGN.md › Speech and rewrite
//! pipeline). One HTTPS base, one key, at most two calls per dictation; every
//! request carries `provider.data_collection: "deny"`.
//!
//! OWNER: core-dictation fork. Public API below is fixed; bodies to implement.

use crate::ports::{Transport, TransportError};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const TRANSCRIBE_MODEL: &str = "microsoft/mai-transcribe-2";
pub const REWRITE_MODEL: &str = "openai/gpt-6-luna";
pub const REWRITE_TIMEOUT: Duration = Duration::from_secs(4);
pub const REWRITE_MAX_TOKENS: u32 = 400;
/// Test hook only (loopback stub servers); not a user setting.
pub const BASE_URL_ENV: &str = "CANTRIP_OPENROUTER_BASE";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Mode {
    Raw,
    Clean,
    Elevated,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Raw => "Raw",
            Mode::Clean => "Clean",
            Mode::Elevated => "Elevated",
        }
    }
}

/// One timestamped segment of a transcript, milliseconds from audio start.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    /// Present only when timestamps were requested and returned.
    pub segments: Vec<Segment>,
    /// `usage.cost` in USD when reported.
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeechError {
    /// No connection could be made; nothing was sent.
    Offline(String),
    /// HTTP status error. `retryable` for 408, 429, 5xx.
    Http {
        status: u16,
        retryable: bool,
        retry_after: Option<Duration>,
    },
    /// Transport failed after the request may have been sent, or timed out.
    Transport(String),
    /// 2xx with no words.
    Empty,
    /// 2xx with an unparseable body.
    Malformed(String),
}

/// Why a rewrite gave Raw (`Raw (cleanup skipped)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RewriteSkip {
    Error(String),
    Empty,
    /// `finish_reason != "stop"`.
    Truncated(String),
    Timeout,
}

/// The exact system prompt for `mode` (`None` for Raw), with the vocabulary
/// block substituted (DESIGN.md › Prompt contract).
pub fn system_prompt(mode: Mode, terms: &[String]) -> Option<String> {
    let _ = (mode, terms);
    todo!()
}

/// JSON body for `POST /audio/transcriptions`: model, base64 `input_audio`
/// (`format: "wav"`), `language: "en"`, `keyterms`, provider deny; with
/// `timestamps`, `response_format: "verbose_json"` + segment granularity.
pub fn transcription_body(wav: &[u8], terms: &[String], timestamps: bool) -> serde_json::Value {
    let _ = (wav, terms, timestamps);
    todo!()
}

/// JSON body for `POST /chat/completions` rewrite: model, `reasoning.effort:
/// "none"`, `max_tokens: 400`, provider deny, system prompt, user message
/// `"Source:\n<text>"`. `None` for Raw.
pub fn rewrite_body(mode: Mode, text: &str, terms: &[String]) -> Option<serde_json::Value> {
    let _ = (mode, text, terms);
    todo!()
}

/// OpenRouter client over an injected transport.
#[derive(Clone)]
pub struct OpenRouter {
    transport: Arc<dyn Transport>,
    base: String,
}

impl OpenRouter {
    /// Base URL from [`BASE_URL_ENV`] if set, else [`DEFAULT_BASE_URL`].
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        let base = std::env::var(BASE_URL_ENV).unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Self::with_base(transport, &base)
    }

    pub fn with_base(transport: Arc<dyn Transport>, base: &str) -> Self {
        OpenRouter {
            transport,
            base: base.trim_end_matches('/').to_string(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Transcribe a WAV file. No total deadline.
    pub fn transcribe(
        &self,
        key: &str,
        wav: &Path,
        terms: &[String],
        timestamps: bool,
    ) -> Result<Transcript, SpeechError> {
        let _ = (
            key,
            wav,
            terms,
            timestamps,
            &self.transport,
            TransportError::Timeout,
        );
        todo!()
    }

    /// Rewrite with the mode's prompt under a hard deadline (watchdog thread;
    /// late replies are discarded). Raw must never be passed here.
    pub fn rewrite(
        &self,
        key: &str,
        mode: Mode,
        text: &str,
        terms: &[String],
        deadline: Duration,
    ) -> Result<String, RewriteSkip> {
        let _ = (key, mode, text, terms, deadline);
        todo!()
    }

    /// Generic `gpt-6-luna` chat call for meeting cleanup and summary
    /// (provider deny, reasoning none). Returns the content when
    /// `finish_reason == "stop"` and non-empty.
    pub fn chat(
        &self,
        key: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
        timeout: Option<Duration>,
    ) -> Result<String, SpeechError> {
        let _ = (key, system, user, max_tokens, timeout);
        todo!()
    }
}
