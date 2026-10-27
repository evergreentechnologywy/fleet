//! Lessons: the self-improvement loop.
//!
//! After every run the engine decides — deterministically, from evidence it
//! already has — whether there is something to remember. A lesson is written to
//! shared memory immediately, in the same turn that produced it, and a copy goes
//! to the ledger. Later runs read lessons back before they plan, so the next
//! decision is made with the last one's outcome in hand.
//!
//! Three sources, no model required:
//!
//! * a harness call that **failed** → a `harness` lesson naming the failure;
//! * a routing decision that **fell back** to a default model → a `route`
//!   lesson naming the chooser failure, so a broken chooser is visible;
//! * a run whose answer came from **unison** → a `merge` lesson recording which
//!   pair produced the answer, so the pairing can be reinforced or dropped.
//!
//! A lesson is one durable row, keyed by a stable hash so the same lesson is
//! superseded rather than duplicated, with a hit count that makes repeated
//! advice rank above a one-off.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::memory::Memory;

pub const SCOPE: &str = "lessons";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    pub key: String,
    pub text: String,
    pub kind: String,
    pub seen: i64,
    pub updated: String,
}

/// Stable key so the same lesson supersedes itself instead of piling up.
fn key_for(kind: &str, subject: &str) -> String {
    let norm: String = subject
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    let norm = norm.trim_matches('-');
    let norm: String = norm.chars().take(60).collect();
    format!("{kind}:{norm}")
}

/// Record a lesson. `seen` counts how many times it has been observed; a
/// lesson that keeps recurring outranks one observed once.
pub fn record(mem: &Memory, kind: &str, subject: &str, text: &str) -> Result<Lesson> {
    let key = key_for(kind, subject);
    let prev = mem.get(SCOPE, &key)?;
    let seen = prev
        .as_deref()
        .and_then(|v| serde_json::from_str::<Lesson>(v).ok())
        .map(|l| l.seen + 1)
        .unwrap_or(1);
    let lesson = Lesson {
        key: key.clone(),
        text: text.to_string(),
        kind: kind.to_string(),
        seen,
        updated: crate::now(),
    };
    // Last version wins: the newer text replaces the older at the same key.
    mem.put(SCOPE, &key, &serde_json::to_string(&lesson)?, "fleet")?;
    Ok(lesson)
}

/// The lessons a run should be told about before it starts. Most-seen first,
/// because a lesson observed five times is worth more than one seen once.
pub fn briefing(mem: &Memory, limit: usize) -> Result<Vec<Lesson>> {
    let rows = mem.list(SCOPE, "", 200)?;
    let mut out: Vec<Lesson> = rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<Lesson>(&v).ok())
        .collect();
    out.sort_by(|a, b| b.seen.cmp(&a.seen).then(b.updated.cmp(&a.updated)));
    out.truncate(limit);
    Ok(out)
}

/// Derive lessons from a finished run. Called once per run; cheap and offline.
pub fn harvest(
    mem: &Memory,
    calls: &[crate::runner::Outcome],
    planned: &[String],
) -> Result<Vec<Lesson>> {
    let mut new = Vec::new();
    for c in calls {
        if !c.ok {
            new.push(record(
                mem,
                "harness",
                &format!("{} {}", c.harness, crate::runner::truncate(&c.error, 40)),
                &format!(
                    "`{}` failed and should be treated as suspect until it succeeds again: {}",
                    c.harness,
                    crate::runner::truncate(&c.error, 300)
                ),
            )?);
        }
        if c.route_reason.contains("chooser failed")
            || c.route_reason.contains("not in harness ceiling")
        {
            new.push(record(
                mem,
                "route",
                &format!(
                    "{} {}",
                    c.harness,
                    crate::runner::truncate(&c.route_reason, 40)
                ),
                &format!("routing for `{}`: {}", c.harness, c.route_reason),
            )?);
        }
    }
    if planned.len() > 1 {
        new.push(record(
            mem,
            "pair",
            &planned.join("+"),
            &format!(
                "the pairing [{}] was used together and produced one merged answer; reuse it when the task looks similar",
                planned.join(", ")
            ),
        )?);
    }
    Ok(new)
}
