//! Planning: turn a task into a set of harness calls.
//!
//! The planner exists because one harness is rarely the whole answer while two
//! would be. It asks the chooser for a *plan* — a JSON object — and the chooser
//! is free to be a model, a policy engine, or nothing at all:
//!
//! * no chooser  → **every enabled harness whose tags overlap the task** runs.
//!   Unison is the default, because the point of the engine is to use them all.
//! * one harness named on the command line → **single**, no chooser call.
//! * `--unison` → force every candidate to run and their answers to be joined.
//! * `--plan-strategy serial` → pipeline instead of fan-out: each stage sees the
//!   previous stage's output, so a verifier can check a worker's answer.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::registry::Harness;

/// A chooser callback: task prompt in, an answer (usually a plan as JSON) out.
/// Named because the bare `dyn Fn` type is unreadable at a call site.
pub type ChooserFn<'a> = &'a dyn Fn(&str) -> Result<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// One harness answers. The default, because one harness is the least
    /// surprising thing to do with a task.
    #[default]
    Single,
    /// Several harnesses answer the same brief and the answers are merged.
    Unison,
    /// Harnesses run in sequence; each sees what came before.
    Serial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub harness: String,
    /// What this step is asked. For `Serial`, later steps also receive the prior
    /// answers, injected by the engine.
    pub brief: String,
    /// Optional role label: `work`, `verify`, `critique`, `merge`.
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub mode: Mode,
    pub steps: Vec<Step>,
    /// Who wrote the plan: `chooser`, `flag`, `tags`, `override`.
    pub source: String,
    #[serde(default)]
    pub notes: String,
}

impl Plan {
    pub fn harness_names(&self) -> Vec<String> {
        self.steps.iter().map(|s| s.harness.clone()).collect()
    }
}

/// Build a plan.
///
/// `hint` is the user's `--harness` (single mode, planner bypassed) and
/// `force_unison` is `--unison`. Both short-circuit the chooser: an explicit
/// instruction is never second-guessed.
pub fn plan(
    task: &str,
    candidates: &[Harness],
    hint: Option<&str>,
    force_unison: bool,
    serial: bool,
    chooser: Option<ChooserFn<'_>>,
) -> Plan {
    if let Some(name) = hint {
        if name != "auto" {
            return Plan {
                mode: Mode::Single,
                steps: vec![Step {
                    harness: name.to_string(),
                    brief: task.to_string(),
                    role: "work".into(),
                    why: "named on the command line".into(),
                }],
                source: "flag".into(),
                notes: String::new(),
            };
        }
    }

    // Ask the chooser for a plan when one is available. A chooser that answers
    // with anything unusable falls through to tag selection rather than failing
    // the run: routing is a decision, not a precondition.
    if let Some(ask) = chooser {
        if let Ok(raw) = ask(&plan_prompt(task, candidates)) {
            if let Some(p) = parse_plan(&raw, candidates) {
                return p;
            }
        }
    }

    by_tags(task, candidates, force_unison, serial)
}

/// Deterministic fallback: overlap between the task's own words and each
/// harness's tags and description. No model, no network, always available.
fn by_tags(task: &str, candidates: &[Harness], force_unison: bool, serial: bool) -> Plan {
    let lower = task.to_ascii_lowercase();
    let mut scored: Vec<(i32, &Harness)> = candidates
        .iter()
        .map(|h| {
            let mut score = h.priority;
            if h.verifier {
                score -= 5; // a verifier joins when asked, not by default
            }
            for t in &h.tags {
                if lower.contains(&t.to_ascii_lowercase()) {
                    score += 10;
                }
            }
            for w in h
                .description
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| w.len() > 3)
            {
                if lower.contains(&w.to_ascii_lowercase()) {
                    score += 2;
                }
            }
            (score, h)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));

    let mode = if force_unison {
        Mode::Unison
    } else if serial {
        Mode::Serial
    } else if scored.len() > 1 && scored[0].0 > 0 && scored[0].0 >= scored[1].0 + 10 {
        Mode::Single
    } else if scored.len() > 1 {
        Mode::Unison
    } else {
        Mode::Single
    };

    // How many stages each mode wants. A pipeline with one stage is not a
    // pipeline, so Serial takes several; Single is the only mode that means
    // "one harness answers".
    let take = match mode {
        Mode::Unison => 4,
        Mode::Serial => 3,
        Mode::Single => 1,
    };
    let steps: Vec<Step> = scored
        .iter()
        .filter(|(s, _)| *s > -5)
        .take(take)
        .map(|(s, h)| Step {
            harness: h.name.clone(),
            brief: task.to_string(),
            role: "work".into(),
            why: format!("tag score {s}"),
        })
        .collect();

    // A serial plan that has a verifier available ends with that verifier.
    let steps = if mode == Mode::Serial {
        let mut s = steps;
        if let Some(v) = candidates.iter().find(|c| c.verifier) {
            if !s.iter().any(|x| x.harness == v.name) {
                s.push(Step {
                    harness: v.name.clone(),
                    brief: format!("Check the work above against this brief:\n{task}"),
                    role: "verify".into(),
                    why: "declared verifier".into(),
                });
            }
        }
        s
    } else {
        steps
    };

    Plan {
        mode,
        steps,
        source: "tags".into(),
        notes: "selected by tag overlap (no chooser answered)".into(),
    }
}

/// The prompt handed to a model-backed chooser. Kept explicit about the schema
/// so any chat model can answer it in one shot.
pub fn plan_prompt(task: &str, candidates: &[Harness]) -> String {
    let list = candidates
        .iter()
        .map(|h| {
            format!(
                "- {} (kind={:?}, tags=[{}], {:?})",
                h.name,
                h.kind,
                h.tags.join(", "),
                h.description
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are routing one task across agent harnesses. Choose one or more.\n\
         TASK:\n{task}\n\nAVAILABLE HARNESSES:\n{list}\n\n\
         Reply with ONLY this JSON and nothing else:\n\
         {{\"mode\":\"single|unison|serial\",\"steps\":[{{\"harness\":\"<name>\",\"role\":\"work|verify|critique\",\"why\":\"<short>\"}}]}}\n\
         Use unison when two harnesses would each contribute something different. \
         Use serial when a later harness should check an earlier one. \
         Maximum 4 steps."
    )
}

/// Accept a plan in several shapes, because choosers are models and models
/// sometimes wrap their answer in prose or use a different key.
pub fn parse_plan(raw: &str, candidates: &[Harness]) -> Option<Plan> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&raw[start..=end]).ok()?;
    let mode = match v.get("mode").and_then(|m| m.as_str()).unwrap_or("single") {
        m if m.eq_ignore_ascii_case("unison") => Mode::Unison,
        m if m.eq_ignore_ascii_case("serial") || m.eq_ignore_ascii_case("pipeline") => Mode::Serial,
        _ => Mode::Single,
    };
    let mut steps = Vec::new();
    if let Some(arr) = v.get("steps").and_then(|s| s.as_array()) {
        for item in arr {
            let name = item
                .get("harness")
                .or_else(|| item.get("name"))
                .and_then(|x| x.as_str())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            // A plan naming a harness that is not installed is a plan for a
            // different machine: drop that step rather than fail the run.
            if !candidates.iter().any(|c| c.name == name) {
                continue;
            }
            steps.push(Step {
                harness: name.to_string(),
                brief: item
                    .get("brief")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                role: item
                    .get("role")
                    .and_then(|x| x.as_str())
                    .unwrap_or("work")
                    .to_string(),
                why: item
                    .get("why")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
    }
    // A single-harness answer can also arrive as `{"harness": "codex"}`.
    if steps.is_empty() {
        if let Some(n) = v.get("harness").and_then(|x| x.as_str()) {
            if candidates.iter().any(|c| c.name == n) {
                steps.push(Step {
                    harness: n.to_string(),
                    brief: String::new(),
                    role: "work".into(),
                    why: "chooser".into(),
                });
            }
        }
    }
    if steps.is_empty() {
        return None;
    }
    Some(Plan {
        mode,
        steps,
        source: "chooser".into(),
        notes: v
            .get("why")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
    })
}
