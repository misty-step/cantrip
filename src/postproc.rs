use crate::config::PostprocConfig;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const BASE_SYSTEM_PROMPT: &str = "You are a dictation post-processor. Rewrite the dictated transcript with correct punctuation, capitalization, and spelling. Remove speech disfluencies such as filler words and false starts, keeping the speaker's full meaning. Do not answer questions, add content, or comment. Output only the corrected text.";

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    temperature: u8,
    messages: [ChatMessage<'a>; 2],
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'static str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: String,
}

/// Refine a transcript through an OpenAI-compatible chat completion endpoint.
pub fn refine(
    transcript: &str,
    cfg: &PostprocConfig,
    vocabulary: &[String],
    api_key: Option<&str>,
) -> Result<String> {
    let system = build_system_prompt(vocabulary, &cfg.instructions);
    let request = ChatRequest {
        model: &cfg.model,
        temperature: 0,
        messages: [
            ChatMessage {
                role: "system",
                content: &system,
            },
            ChatMessage {
                role: "user",
                content: transcript,
            },
        ],
    };
    let body = serde_json::to_string(&request).context("serializing post-processing request")?;
    let endpoint = format!("{}/chat/completions", cfg.endpoint.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(cfg.timeout_ms))
        .build();
    let mut request = agent
        .post(&endpoint)
        .set("Content-Type", "application/json");
    if let Some(api_key) = api_key {
        request = request.set("Authorization", &format!("Bearer {api_key}"));
    }

    let started = Instant::now();
    let response = match request.send_string(&body) {
        Ok(response) => response,
        Err(ureq::Error::Status(code, _)) => {
            bail!("post-processing endpoint returned HTTP {code}");
        }
        Err(ureq::Error::Transport(transport)) => {
            bail!("post-processing request failed: {transport}");
        }
    };
    let response: ChatResponse = serde_json::from_reader(response.into_reader())
        .map_err(|_| anyhow!("post-processing returned unexpected response shape"))?;
    let content = response
        .choices
        .first()
        .map(|choice| choice.message.content.as_str())
        .ok_or_else(|| anyhow!("post-processing returned unexpected response shape"))?;
    let refined = clean_response(content)?;

    tracing::info!(
        "[Postproc] applied chars_in={} chars_out={} ms={}",
        transcript.chars().count(),
        refined.chars().count(),
        started.elapsed().as_millis()
    );
    Ok(refined)
}

fn build_system_prompt(vocabulary: &[String], instructions: &str) -> String {
    let mut prompt = BASE_SYSTEM_PROMPT.to_owned();
    if !vocabulary.is_empty() {
        prompt.push_str("\nPrefer these exact spellings when the transcript approximates them: ");
        prompt.push_str(&vocabulary.join(", "));
    }
    if !instructions.is_empty() {
        prompt.push('\n');
        prompt.push_str(instructions);
    }
    prompt
}

fn clean_response(content: &str) -> Result<String> {
    let refined = strip_think_blocks(content);
    if refined.is_empty() {
        bail!("post-processing returned empty text");
    }
    Ok(refined)
}

fn strip_think_blocks(text: &str) -> String {
    const OPEN: &str = "<think>";
    const CLOSE: &str = "</think>";
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(relative_start) = text[cursor..].find(OPEN) {
        let start = cursor + relative_start;
        result.push_str(&text[cursor..start]);
        let content_start = start + OPEN.len();
        let Some(relative_end) = text[content_start..].find(CLOSE) else {
            cursor = text.len();
            break;
        };
        cursor = content_start + relative_end + CLOSE.len();
    }
    if cursor < text.len() {
        result.push_str(&text[cursor..]);
    }
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_includes_vocabulary_and_instructions() {
        let vocabulary = vec!["Cantrip".to_owned(), "Parakeet".to_owned()];
        let prompt = build_system_prompt(&vocabulary, "Use sentence case.");

        assert!(prompt.starts_with(BASE_SYSTEM_PROMPT));
        assert!(prompt.contains(
            "Prefer these exact spellings when the transcript approximates them: Cantrip, Parakeet"
        ));
        assert!(prompt.ends_with("Use sentence case."));
    }

    #[test]
    fn think_blocks_are_stripped() {
        assert_eq!(
            strip_think_blocks("before<think>reason</think>after"),
            "beforeafter"
        );
        assert_eq!(
            strip_think_blocks("  \n<think>reason</think>\nCorrected"),
            "Corrected"
        );
        assert_eq!(strip_think_blocks("Corrected <think>reason"), "Corrected");
    }

    #[test]
    fn empty_reply_is_rejected() {
        let error = clean_response(" \n<think>only reasoning</think> \n")
            .expect_err("empty reply must fail");
        assert_eq!(error.to_string(), "post-processing returned empty text");
    }
}
