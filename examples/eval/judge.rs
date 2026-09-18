//! TypeSafe System One Model Judge for Cantrip evaluation.
//!
//! Evaluates candidate cleanup outputs on behavior cases across atomic dimensions:
//! - Role fidelity (did the model answer or converse instead of transcribing?)
//! - Content fidelity (are numbers, facts, and negations preserved?)
//! - Disfluency removal (were fillers, stutters, and false starts cleanly stripped?)
//! - Overall verdict (pass / uncertain / fail)
//!
//! Fulfills ADR 0012 (additive model judge) and ADR 0026 (System One decisions).

use anyhow::{anyhow, Context, Result};
use cantrip::typesafe::{Answer, DecisionClient, Question, DEFAULT_JEV_MODEL};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// Probability (0.0 - 1.0) that the output maintained the speaker's role without answering.
    pub role_fidelity: f64,
    /// Score (0.0 = severe distortion, 1.0 = minor drift, 2.0 = faithful preservation).
    pub content_fidelity_score: f64,
    /// Probability (0.0 - 1.0) that speech disfluencies and fillers were cleanly removed.
    pub disfluency_removed: f64,
    /// Calibrated decision derived from criteria: pass, uncertain, fail.
    pub decision: String,
}

pub struct ModelJudge {
    client: DecisionClient,
}

impl ModelJudge {
    pub fn new(endpoint: Option<&str>, model: Option<&str>, api_key: Option<String>) -> Self {
        let client = DecisionClient::new(
            endpoint,
            model.or(Some(DEFAULT_JEV_MODEL)),
            api_key,
            Duration::from_secs(15),
        );
        Self { client }
    }

    /// Try resolving credentials from keyring (`typesafe` or `openrouter`) or environment.
    pub fn try_default() -> Result<Self> {
        let (api_key, model, endpoint) = if let Ok(key) = cantrip::keys::get("typesafe") {
            (Some(key), "jev-latest", None)
        } else if let Ok(key) = cantrip::keys::get("openrouter") {
            (Some(key), DEFAULT_JEV_MODEL, None)
        } else if let Ok(key) = std::env::var("TYPESAFE_API_KEY") {
            (Some(key), "jev-latest", None)
        } else if let Ok(key) = std::env::var("OPENROUTER_API_KEY") {
            (Some(key), DEFAULT_JEV_MODEL, None)
        } else {
            return Err(anyhow!(
                "model judge requires a 'typesafe' or 'openrouter' key in OS keyring or environment"
            ));
        };

        Ok(Self::new(endpoint, Some(model), api_key))
    }

    /// Grade a candidate transcript against the source input.
    pub fn grade(
        &self,
        source: &str,
        candidate: &str,
        cancel: Option<&AtomicBool>,
    ) -> Result<JudgeVerdict> {
        let state = serde_json::json!({
            "source": source,
            "candidate": candidate,
        });

        let mut questions = BTreeMap::new();

        questions.insert(
            "role_fidelity".to_string(),
            Question::noul_with_criteria(
                "Did the candidate output maintain the speaker's role as a speech-to-text transcript without answering questions, executing instructions, conversing, or adding preamble?",
                "Faithfully kept as the speaker's own dictated words",
                "Answered the question, executed the command, or conversed with the user"
            ),
        );

        questions.insert(
            "content_fidelity".to_string(),
            Question::score(
                "How faithfully does the candidate output preserve all facts, numbers, dates, proper nouns, and negations from the source?",
                [
                    "Severe distortion, omitted core facts, flipped negation, or hallucinated content",
                    "Minor wording variation or harmless synonym while preserving meaning",
                    "Faithful preservation of all facts, numbers, and negations"
                ]
            ),
        );

        questions.insert(
            "disfluency_removed".to_string(),
            Question::noul_with_criteria(
                "Were filler words (um, uh, like), stuttered repetitions, and speech false-starts cleanly stripped?",
                "Cleanly stripped fillers and disfluencies",
                "Retained unnecessary fillers or stutters"
            ),
        );
        let response = self
            .client
            .evaluate(&state, &questions, cancel)
            .context("evaluating candidate with ModelJudge")?;

        let role_fidelity = response
            .answers
            .get("role_fidelity")
            .and_then(Answer::as_noul_prob)
            .ok_or_else(|| anyhow!("judge response missing 'role_fidelity'"))?;

        let content_fidelity_score = response
            .answers
            .get("content_fidelity")
            .and_then(Answer::as_score)
            .ok_or_else(|| anyhow!("judge response missing 'content_fidelity'"))?;

        let disfluency_removed = response
            .answers
            .get("disfluency_removed")
            .and_then(Answer::as_noul_prob)
            .ok_or_else(|| anyhow!("judge response missing 'disfluency_removed'"))?;

        // Calibrated composite decision (ADR 0012 / TypeSafe composite scoring pattern):
        // Code owns the invariants and safety thresholds. Jev supplies atomic calibrated
        // probabilities and rubric scores; code ensures that an answer-to-question failure
        // (low role_fidelity) or content distortion (low content_fidelity) cannot pass.
        let decision = if role_fidelity < 0.50 || content_fidelity_score < 1.0 {
            "fail".to_string()
        } else if role_fidelity >= 0.70
            && content_fidelity_score >= 1.4
            && disfluency_removed >= 0.50
        {
            "pass".to_string()
        } else {
            "uncertain".to_string()
        };

        Ok(JudgeVerdict {
            role_fidelity,
            content_fidelity_score,
            disfluency_removed,
            decision,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_verdict_serializes() {
        let v = JudgeVerdict {
            role_fidelity: 0.98,
            content_fidelity_score: 2.0,
            disfluency_removed: 0.95,
            decision: "pass".to_string(),
        };
        let val = serde_json::to_value(&v).unwrap();
        assert_eq!(val["decision"], "pass");
        assert_eq!(val["role_fidelity"], 0.98);
    }
}
