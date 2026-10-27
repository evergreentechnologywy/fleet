//! fleet — a harness for harnesses.
//!
//! One orchestrator, many agent harnesses. A harness is a TOML file: add one by
//! writing it, remove one by deleting it, and neither needs a restart, because
//! the harness directory is re-read on every call. Each call asks a chooser —
//! Jev, an alternative, or nothing at all — which model and effort to run with,
//! so the model is a per-call decision rather than a per-tool configuration.
//!
//! The pieces:
//!
//! * [`registry`] — the hot-pluggable harness set. The file IS the harness.
//! * [`router`] — the model chooser, pluggable, with a usable no-chooser path.
//! * [`planner`] — one harness, several in unison, or a serial pipeline.
//! * [`engine`] — the run: plan, route, execute, merge, remember.
//! * [`plugins`] — engine-level hooks, so one extension applies to every harness.
//! * [`providers`] — credentials declared once and shared, with explicit blocks.
//! * [`mesh`] — peer nodes over Tailscale SSH, plain SSH, or Reason Network.
//! * [`memory`] — instant durable shared memory and full session transcripts.
//! * [`lessons`] — the self-improvement loop, written from evidence.
//! * [`selfmod`] — verifier-gated self-modification; the ledger is truth.
//! * [`deploy`] — one line to a server.
//! * [`api`] — the same engine over HTTP.

pub mod api;
pub mod config;
pub mod deploy;
pub mod engine;
pub mod ledger;
pub mod lessons;
pub mod memory;
pub mod mesh;
pub mod paths;
pub mod planner;
pub mod plugins;
pub mod providers;
pub mod registry;
pub mod router;
pub mod runner;
pub mod selfmod;
pub mod templates;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// One timestamp format everywhere, so ledger rows and memory rows sort
/// lexicographically as well as chronologically.
pub fn now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// A short, time-ordered, collision-resistant id. Time first so a directory
/// listing of runs is already in order.
pub fn short_id() -> String {
    let t = OffsetDateTime::now_utc().unix_timestamp_nanos();
    let pid = std::process::id() as u128;
    let mut x = (t as u128) ^ ((pid as u128) << 64);
    // xorshift: enough to separate two runs in the same nanosecond.
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let s = format!("{:x}", (t as u64));
    format!("{}{}", &s[s.len().saturating_sub(8)..], (x % 0xffff) as u16)
}
