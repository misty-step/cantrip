//! The Markdown note: file naming, slug, frontmatter and body. Rendering is a
//! pure function of staged data, so the same staging gives a byte-identical
//! note.

use super::transcript::{Turn, TurnBody};
use crate::surface::mm_ss;
use serde::{Deserialize, Serialize};

/// Title / summary / decisions / actions from the summary pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub title: String,
    pub summary: String,
    pub decisions: Vec<String>,
    pub actions: Vec<String>,
    /// True when the summary pass failed (fixed "Summary failed" content).
    pub failed: bool,
}

impl Summary {
    pub fn failed() -> Summary {
        Summary {
            title: "Meeting".into(),
            summary: "Summary failed".into(),
            decisions: vec![],
            actions: vec![],
            failed: true,
        }
    }
}

/// Outcome of the cleanup pass for one text turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cleaned {
    Text(String),
    Failed,
}

/// `<slug>`: lower-case ASCII alphanumerics, other runs `-`, trimmed, at most
/// 60 chars, else `meeting`.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    if out.len() > 60 {
        out.truncate(60);
        while out.ends_with('-') {
            out.pop();
        }
    }
    if out.is_empty() {
        "meeting".into()
    } else {
        out
    }
}

fn span(start_ms: u64, end_ms: u64) -> String {
    format!(
        "{}–{}",
        mm_ss(start_ms / 1000),
        mm_ss(end_ms.div_ceil(1000))
    )
}

/// One transcript paragraph, e.g. `[00:04] You: Hello.`
pub fn turn_line(turn: &Turn, cleaned: Option<&Cleaned>) -> String {
    let head = format!(
        "[{}] {}:",
        mm_ss(turn.start_ms / 1000),
        turn.speaker.label()
    );
    match &turn.body {
        TurnBody::Failed => format!(
            "{head} [transcription failed: {}]",
            span(turn.start_ms, turn.end_ms)
        ),
        TurnBody::Text { text, approximate } => {
            let mut body = String::new();
            if *approximate {
                body.push_str(&format!(
                    "[approximate order: {}] ",
                    span(turn.start_ms, turn.end_ms)
                ));
            }
            match cleaned {
                Some(Cleaned::Text(t)) => body.push_str(t.trim()),
                Some(Cleaned::Failed) => body.push_str(&format!(
                    "[cleanup failed: {}] {}",
                    span(turn.start_ms, turn.end_ms),
                    text.trim()
                )),
                None => body.push_str(text.trim()),
            }
            format!("{head} {body}")
        }
    }
}

fn yaml_str(s: &str) -> String {
    // A JSON string is a valid YAML double-quoted scalar.
    serde_json::to_string(s).expect("string serializes")
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Render the whole note.
pub fn render(
    source_id: &str,
    recorded_at: &str,
    partial: bool,
    summary: &Summary,
    lines: &[String],
) -> String {
    let title = {
        let t = one_line(&summary.title);
        if t.is_empty() {
            "Meeting".to_string()
        } else {
            t
        }
    };
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str("source: \"cantrip-meeting\"\n");
    out.push_str(&format!("source_id: {}\n", yaml_str(source_id)));
    out.push_str(&format!(
        "status: {}\n",
        if partial { "partial" } else { "complete" }
    ));
    out.push_str(&format!("recorded_at: {}\n", yaml_str(recorded_at)));
    out.push_str(&format!("title: {}\n", yaml_str(&title)));
    out.push_str("---\n\n");
    out.push_str(&format!("# {title}\n\n"));
    out.push_str("## Summary\n\n");
    out.push_str(summary.summary.trim());
    out.push_str("\n\n## Decisions\n\n");
    let none = if summary.failed {
        "Summary failed"
    } else {
        "None."
    };
    if summary.decisions.is_empty() {
        out.push_str(none);
        out.push('\n');
    } else {
        for d in &summary.decisions {
            out.push_str(&format!("- {}\n", one_line(d)));
        }
    }
    out.push_str("\n## Actions\n\n");
    if summary.actions.is_empty() {
        out.push_str(none);
        out.push('\n');
    } else {
        for a in &summary.actions {
            out.push_str(&format!("- [ ] {}\n", one_line(a)));
        }
    }
    out.push_str("\n## Transcript\n\n");
    out.push_str(&lines.join("\n\n"));
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::super::parts::Track;
    use super::*;

    #[test]
    fn slug_rules() {
        assert_eq!(slug("Weekly Sync: Q4 plans!"), "weekly-sync-q4-plans");
        assert_eq!(slug("  --Ça va? Überall  "), "a-va-berall");
        assert_eq!(slug("日本語"), "meeting");
        assert_eq!(slug(""), "meeting");
        let long = slug(&"abc ".repeat(40));
        assert!(long.len() <= 60 && !long.ends_with('-'), "{long}");
    }

    #[test]
    fn note_has_frontmatter_sections_and_turns() {
        let turns = [
            Turn {
                speaker: Track::You,
                start_ms: 4_200,
                end_ms: 6_000,
                body: TurnBody::Text {
                    text: "um hello".into(),
                    approximate: false,
                },
            },
            Turn {
                speaker: Track::Others,
                start_ms: 65_000,
                end_ms: 70_000,
                body: TurnBody::Failed,
            },
            Turn {
                speaker: Track::Others,
                start_ms: 71_000,
                end_ms: 72_000,
                body: TurnBody::Text {
                    text: "raw".into(),
                    approximate: false,
                },
            },
        ];
        let lines = vec![
            turn_line(&turns[0], Some(&Cleaned::Text("Hello.".into()))),
            turn_line(&turns[1], None),
            turn_line(&turns[2], Some(&Cleaned::Failed)),
        ];
        assert_eq!(lines[0], "[00:04] You: Hello.");
        assert_eq!(
            lines[1],
            "[01:05] Others: [transcription failed: 01:05–01:10]"
        );
        assert_eq!(
            lines[2],
            "[01:11] Others: [cleanup failed: 01:11–01:12] raw"
        );
        let s = Summary {
            title: "Plan \"X\"".into(),
            summary: "We planned.".into(),
            decisions: vec!["Ship".into()],
            actions: vec!["Write docs".into()],
            failed: false,
        };
        let note = render("abc", "2026-11-23T19:24:05Z", true, &s, &lines);
        assert!(note.starts_with(
            "---\nsource: \"cantrip-meeting\"\nsource_id: \"abc\"\nstatus: partial\nrecorded_at: \"2026-11-23T19:24:05Z\"\ntitle: \"Plan \\\"X\\\"\"\n---\n\n# Plan \"X\"\n\n## Summary\n\nWe planned.\n\n## Decisions\n\n- Ship\n\n## Actions\n\n- [ ] Write docs\n\n## Transcript\n\n[00:04] You: Hello.\n\n[01:05]"
        ));
        assert_eq!(
            render("abc", "2026-11-23T19:24:05Z", true, &s, &lines),
            note
        );
    }
}
