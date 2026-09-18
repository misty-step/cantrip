//! TypeSafe System One structured decision client.
//!
//! Evaluates a state against typed questions (`noul`, `choice`, `score`) returning
//! calibrated probabilities without free-form text generation.
//!
//! Follows ADR 0026: lightweight, content-safe transport, no credential leakage.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const DEFAULT_JEV_MODEL: &str = "typesafe/jev-1.13";
pub const DEFAULT_TYPESAFE_ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_OPENROUTER_DECISIONS_ENDPOINT: &str = "https://openrouter.ai/api/alpha/decisions";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoulCriteria {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#true: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#false: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
    Choice {
        instructions: String,
        criteria: BTreeMap<String, Option<String>>,
    },
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

impl Question {
    pub fn noul(instructions: impl Into<String>) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: None,
        }
    }

    pub fn noul_with_criteria(
        instructions: impl Into<String>,
        true_desc: impl Into<String>,
        false_desc: impl Into<String>,
    ) -> Self {
        Self::Noul {
            instructions: instructions.into(),
            criteria: Some(NoulCriteria {
                r#true: Some(true_desc.into()),
                r#false: Some(false_desc.into()),
            }),
        }
    }

    pub fn choice(
        instructions: impl Into<String>,
        options: impl IntoIterator<Item = (impl Into<String>, Option<impl Into<String>>)>,
    ) -> Self {
        Self::Choice {
            instructions: instructions.into(),
            criteria: options
                .into_iter()
                .map(|(k, v)| (k.into(), v.map(Into::into)))
                .collect(),
        }
    }

    pub fn score(
        instructions: impl Into<String>,
        levels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self::Score {
            instructions: instructions.into(),
            criteria: levels.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: f64,
    },
    Score {
        score: f64,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        #[serde(default)]
        confidence: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        legend: Option<BTreeMap<String, String>>,
    },
}

impl Answer {
    pub fn as_noul_prob(&self) -> Option<f64> {
        match self {
            Self::Noul { noul } => Some(*noul),
            _ => None,
        }
    }

    pub fn as_choice(&self) -> Option<&str> {
        match self {
            Self::Choice { choice, .. } => Some(choice.as_str()),
            _ => None,
        }
    }

    pub fn as_score(&self) -> Option<f64> {
        match self {
            Self::Score { score, .. } => Some(*score),
            _ => None,
        }
    }

    pub fn confidence(&self) -> Option<f64> {
        match self {
            Self::Choice { confidence, .. } | Self::Score { confidence, .. } => Some(*confidence),
            Self::Noul { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionRequest<'a> {
    pub model: &'a str,
    pub state: &'a serde_json::Value,
    pub questions: &'a BTreeMap<String, Question>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    #[serde(default)]
    pub usage: Option<DecisionUsage>,
}

/// Resolve appropriate endpoint based on explicit URL or model naming convention.
pub fn resolve_endpoint(custom: Option<&str>, model: &str) -> String {
    if let Some(custom) = custom {
        if !custom.trim().is_empty() {
            return custom.trim().to_string();
        }
    }
    if model.starts_with("typesafe/") || model.contains("openrouter") {
        DEFAULT_OPENROUTER_DECISIONS_ENDPOINT.to_string()
    } else {
        DEFAULT_TYPESAFE_ENDPOINT.to_string()
    }
}

#[derive(Debug, Clone)]
pub struct DecisionClient {
    pub endpoint: String,
    pub model: String,
    pub api_key: Option<String>,
    pub timeout: Duration,
}

impl DecisionClient {
    pub fn new(
        endpoint: Option<&str>,
        model: Option<&str>,
        api_key: Option<String>,
        timeout: Duration,
    ) -> Self {
        let model = model.unwrap_or(DEFAULT_JEV_MODEL).to_string();
        let endpoint = resolve_endpoint(endpoint, &model);
        Self {
            endpoint,
            model,
            api_key,
            timeout,
        }
    }

    pub fn evaluate(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        cancel: Option<&AtomicBool>,
    ) -> Result<DecisionResponse> {
        if let Some(cancel) = cancel {
            if cancel.load(Ordering::Relaxed) {
                bail!("decision evaluation cancelled before request");
            }
        }

        let request = DecisionRequest {
            model: &self.model,
            state,
            questions,
        };

        let body = serde_json::to_vec(&request).context("serializing decision request")?;

        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();

        let mut req = agent
            .post(&self.endpoint)
            .set("Content-Type", "application/json");

        if let Some(api_key) = &self.api_key {
            req = req.set("Authorization", &format!("Bearer {api_key}"));
        }

        let response = match req.send_bytes(&body) {
            Ok(response) => response,
            Err(ureq::Error::Status(code, resp)) => {
                let _ = resp;
                bail!("decision endpoint returned HTTP {code}");
            }
            Err(ureq::Error::Transport(transport)) => {
                let mut cause = std::error::Error::source(&transport);
                while let Some(error) = cause {
                    if error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
                    {
                        bail!("decision request timed out");
                    }
                    cause = error.source();
                }
                bail!("decision request failed ({:?})", transport.kind());
            }
        };

        if let Some(cancel) = cancel {
            if cancel.load(Ordering::Relaxed) {
                bail!("decision evaluation cancelled after response");
            }
        }

        let parsed: DecisionResponse = serde_json::from_reader(response.into_reader())
            .map_err(|_| anyhow!("decision endpoint returned unexpected response shape"))?;

        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_noul_question() {
        let q =
            Question::noul_with_criteria("Is this urgent?", "Very time-sensitive", "Not urgent");
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "noul");
        assert_eq!(v["instructions"], "Is this urgent?");
        assert_eq!(v["criteria"]["true"], "Very time-sensitive");
        assert_eq!(v["criteria"]["false"], "Not urgent");
    }

    #[test]
    fn serialize_choice_question() {
        let q = Question::choice(
            "Which department?",
            [
                ("billing", Some("Payment queries")),
                ("tech", Some("Technical issues")),
            ],
        );
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "choice");
        assert_eq!(v["instructions"], "Which department?");
        assert_eq!(v["criteria"]["billing"], "Payment queries");
        assert_eq!(v["criteria"]["tech"], "Technical issues");
    }

    #[test]
    fn serialize_score_question() {
        let q = Question::score("Rate quality", ["Bad", "Fair", "Excellent"]);
        let v = serde_json::to_value(&q).unwrap();
        assert_eq!(v["type"], "score");
        assert_eq!(v["instructions"], "Rate quality");
        assert_eq!(v["criteria"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn deserialize_typesafe_sample_response() {
        let raw = r#"{
            "model": "jev-latest",
            "answers": {
                "urgent": {
                    "type": "noul",
                    "noul": 0.88
                },
                "category": {
                    "type": "choice",
                    "choice": "tech",
                    "probabilities": { "tech": 0.9, "billing": 0.1 },
                    "confidence": 0.85
                },
                "rating": {
                    "type": "score",
                    "score": 2.4,
                    "probabilities": { "0": 0.1, "1": 0.2, "2": 0.7 },
                    "confidence": 0.75
                }
            },
            "usage": {
                "input_tokens": 120,
                "output_tokens": 0
            }
        }"#;

        let res: DecisionResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(res.model, "jev-latest");
        assert_eq!(res.answers.len(), 3);

        let urgent = &res.answers["urgent"];
        assert_eq!(urgent.as_noul_prob(), Some(0.88));

        let cat = &res.answers["category"];
        assert_eq!(cat.as_choice(), Some("tech"));
        assert_eq!(cat.confidence(), Some(0.85));

        let rating = &res.answers["rating"];
        assert_eq!(rating.as_score(), Some(2.4));
        assert_eq!(rating.confidence(), Some(0.75));

        assert_eq!(res.usage.unwrap().input_tokens, 120);
    }

    #[test]
    fn endpoint_resolution() {
        assert_eq!(
            resolve_endpoint(None, "typesafe/jev-1.13"),
            DEFAULT_OPENROUTER_DECISIONS_ENDPOINT
        );
        assert_eq!(
            resolve_endpoint(None, "jev-latest"),
            DEFAULT_TYPESAFE_ENDPOINT
        );
        assert_eq!(
            resolve_endpoint(Some("http://custom:8080"), "typesafe/jev-1.13"),
            "http://custom:8080"
        );
    }
}
