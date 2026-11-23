//! Speech/rewrite contract: payloads, prompts, parsing and Raw fallbacks.
mod support;

use cantrip_core::ports::TransportError;
use cantrip_core::speech::*;
use std::time::{Duration, Instant};
use support::*;

fn terms() -> Vec<String> {
    vec!["Scry".into(), "T3 Code".into()]
}

#[test]
fn transcription_payload_carries_keyterms_language_and_deny() {
    let b = transcription_body(b"RIFFxxxxWAVE", &terms(), false);
    assert_eq!(b["model"], "microsoft/mai-transcribe-2");
    assert_eq!(b["language"], "en");
    assert_eq!(b["keyterms"], serde_json::json!(["Scry", "T3 Code"]));
    assert_eq!(b["provider"]["data_collection"], "deny");
    assert_eq!(b["input_audio"]["format"], "wav");
    assert_eq!(b["input_audio"]["data"], "UklGRnh4eHhXQVZF");
    assert!(b.get("response_format").is_none());
    let t = transcription_body(b"x", &[], true);
    assert!(t.get("keyterms").is_none());
    assert_eq!(t["response_format"], "verbose_json");
    assert_eq!(t["timestamp_granularities"], serde_json::json!(["segment"]));
}

#[test]
fn rewrite_payload_has_model_effort_tokens_deny_and_source() {
    let b = rewrite_body(Mode::Clean, "um hi", &terms()).unwrap();
    assert_eq!(b["model"], "openai/gpt-6-luna");
    assert_eq!(b["reasoning"]["effort"], "none");
    assert_eq!(b["max_tokens"], 400);
    assert_eq!(b["provider"]["data_collection"], "deny");
    assert_eq!(b["messages"][0]["role"], "system");
    assert_eq!(b["messages"][1]["content"], "Source:\num hi");
    assert!(rewrite_body(Mode::Raw, "x", &terms()).is_none());
}

#[test]
fn clean_v4_prompt_is_exact() {
    let p = system_prompt(Mode::Clean, &terms()).unwrap();
    assert!(p.starts_with(
        "You clean up dictated text.\nThe user message is a dictated transcript after \"Source:\"."
    ));
    assert!(p.contains("delete the entire retracted claim or clause together with the correction phrase (\"wait, no\", \"actually, no\") and keep only the speaker's final corrected statement. Never keep both versions."));
    assert!(p.contains("\"Have Serenity review it. Wait, actually, have Persephone do it.\" becomes \"Have Persephone do it.\""));
    assert!(p.contains("Do not summarize, add facts, or make it more formal.\nCustom Vocabulary (canonical spellings): Scry, T3 Code.\n"));
    assert!(p.ends_with(
        "Do not insert an entry the speaker did not say.\nOutput only the cleaned text."
    ));
}

#[test]
fn elevated_prompt_is_exact_and_empty_vocab_omits_block() {
    let p = system_prompt(Mode::Elevated, &terms()).unwrap();
    assert!(p.starts_with(
        "You rewrite dictated text into crisp, direct prose ready for a message, spec, or email."
    ));
    assert!(p.contains("drop the entire retracted claim and keep only the final corrected statement. Preserve every other fact, name, number, request, question, and degree of certainty. Keep the speaker's person."));
    assert!(p.contains("no greetings, sign-offs, headings, filler, or corporate or generic AI phrasing.\nCustom Vocabulary"));
    assert!(p.ends_with("\nOutput only the rewritten text."));
    let bare = system_prompt(Mode::Elevated, &[]).unwrap();
    assert!(!bare.contains("Custom Vocabulary"));
    assert!(bare.ends_with("generic AI phrasing.\nOutput only the rewritten text."));
    assert!(system_prompt(Mode::Raw, &terms()).is_none());
}

#[test]
fn transcript_parsing_handles_whitespace_segments_cost_and_empty() {
    let t = parse_transcript(
        "\n  {\"text\":\" Hi there. \",\"usage\":{\"cost\":0.0003},\"segments\":[{\"start\":0.5,\"end\":1.25,\"text\":\"Hi there.\"}]}",
    )
    .unwrap();
    assert_eq!(t.text, "Hi there.");
    assert_eq!(t.segments[0].start_ms, 500);
    assert_eq!(t.segments[0].end_ms, 1250);
    assert_eq!(t.cost_usd, Some(0.0003));
    assert_eq!(
        parse_transcript("{\"text\":\"  \"}"),
        Err(SpeechError::Empty)
    );
    assert!(matches!(
        parse_transcript("<html>"),
        Err(SpeechError::Malformed(_))
    ));
}

#[test]
fn transcribe_maps_errors() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    std::fs::write(&wav, b"RIFF").unwrap();
    let or = |r: fn() -> Result<cantrip_core::ports::HttpReply, TransportError>| {
        OpenRouter::with_base(FakeTransport::new(move |_, _| r()), "http://stub/api/v1/")
    };
    assert!(matches!(
        or(|| Err(TransportError::Offline("dns".into()))).transcribe("k", &wav, &[], false),
        Err(SpeechError::Offline(_))
    ));
    assert_eq!(
        or(|| status(503)).transcribe("k", &wav, &[], false),
        Err(SpeechError::Http {
            status: 503,
            retryable: true,
            retry_after: None
        })
    );
    assert!(matches!(
        or(|| status(401)).transcribe("k", &wav, &[], false),
        Err(SpeechError::Http {
            retryable: false,
            ..
        })
    ));
    assert!(matches!(
        or(|| status(429)).transcribe("k", &wav, &[], false),
        Err(SpeechError::Http {
            retryable: true,
            ..
        })
    ));
    let t = FakeTransport::new(|_, _| ok(&transcript_json("hello")));
    let client = OpenRouter::with_base(t.clone(), "http://stub/api/v1/");
    assert_eq!(
        client.transcribe("k", &wav, &[], false).unwrap().text,
        "hello"
    );
    assert_eq!(
        t.calls.lock().unwrap()[0].0,
        "http://stub/api/v1/audio/transcriptions"
    );
}

fn rewrite_with(
    reply: impl Fn() -> Result<cantrip_core::ports::HttpReply, TransportError> + Send + Sync + 'static,
) -> Result<String, RewriteSkip> {
    let t = FakeTransport::new(move |_, _| reply());
    OpenRouter::with_base(t, "http://stub").rewrite("k", Mode::Clean, "x", &[], REWRITE_TIMEOUT)
}

#[test]
fn rewrite_skips_on_error_empty_truncation() {
    assert_eq!(
        rewrite_with(|| ok(&chat_json("Clean text.", "stop"))),
        Ok("Clean text.".into())
    );
    assert!(matches!(
        rewrite_with(|| status(500)),
        Err(RewriteSkip::Error(_))
    ));
    assert!(matches!(
        rewrite_with(|| Err(TransportError::Failed("reset".into()))),
        Err(RewriteSkip::Error(_))
    ));
    assert_eq!(
        rewrite_with(|| ok(&chat_json("   ", "stop"))),
        Err(RewriteSkip::Empty)
    );
    assert_eq!(
        rewrite_with(|| ok(&chat_json("Clean te", "length"))),
        Err(RewriteSkip::Truncated("length".into()))
    );
    assert_eq!(
        rewrite_with(|| Err(TransportError::Timeout)),
        Err(RewriteSkip::Timeout)
    );
}

#[test]
fn rewrite_gives_up_at_the_real_four_second_deadline() {
    let started = Instant::now();
    let r = rewrite_with(|| {
        std::thread::sleep(Duration::from_secs(6));
        ok(&chat_json("late", "stop"))
    });
    let took = started.elapsed();
    assert_eq!(r, Err(RewriteSkip::Timeout));
    assert!(took >= Duration::from_millis(3990), "{took:?}");
    assert!(took < Duration::from_millis(4600), "{took:?}");
}

#[test]
fn chat_carries_deny_and_returns_content() {
    let t = FakeTransport::new(|_, _| ok(&chat_json("Summary", "stop")));
    let c = OpenRouter::with_base(t.clone(), "http://stub");
    assert_eq!(c.chat("k", "sys", "user", 900, None).unwrap(), "Summary");
    let b = &t.bodies("/chat/completions")[0];
    assert_eq!(b["provider"]["data_collection"], "deny");
    assert_eq!(b["max_tokens"], 900);
    assert_eq!(b["reasoning"]["effort"], "none");
}
