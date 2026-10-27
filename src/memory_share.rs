//! Instant shared memory across the fleet.
//!
//! The honest design, and why:
//!
//! Local SQLite is the right PRIMARY. A write returns after `synchronous=FULL`,
//! so it is durable and instantly readable by the next process on this node. That
//! property is worth more than any distributed scheme, and it is why the local
//! store stays authoritative for everything a single node does.
//!
//! What it does not give you is the same key on ANOTHER node. Measured
//! 2026-10-27: a `fleet memory put` on the hub was invisible from the Windows
//! desk, because each node had its own file. That is the gap this closes.
//!
//! The mechanism is a HUB MIRROR, not a consensus protocol:
//!
//! * every node is configured with one hub (`FLEET_MEMORY_HUB`, or
//!   `[memory] hub = "http://host:8787"`), which is a node already running
//!   `fleet serve`;
//! * `put` writes locally first, then POSTs to the hub. Local first means a hub
//!   that is down costs you fleet-wide visibility, never your own durability;
//! * `get`/`list`/`search` read locally, then fall back to the hub. Local first
//!   means the common case costs no network at all.
//!
//! What this is NOT: a merge. Two nodes writing the same key concurrently is
//! last-write-wins BY ARRIVAL AT THE HUB, which is a real limitation and is
//! stated rather than hidden. For genuinely concurrent writers, switch the
//! backend to `postgres` (real transactions) or `holographic` (a CRDT, where the
//! merge is by vector clock). The mirror is for the common fleet case: many
//! readers, one writer at a time, and a need for the write to be visible
//! everywhere immediately.

use anyhow::{Context, Result};
use serde_json::json;
use std::time::Duration;

/// Where the shared hub is, if one is configured.
pub fn hub() -> Option<String> {
    if let Ok(v) = std::env::var("FLEET_MEMORY_HUB") {
        if !v.is_empty() {
            return Some(v.trim_end_matches('/').to_string());
        }
    }
    let cfg = crate::config::Config::load();
    if let Some(h) = cfg.memory.hub {
        if !h.is_empty() {
            return Some(h.trim_end_matches('/').to_string());
        }
    }
    // The fleet's own convention: the hub runs `fleet serve` on 8787.
    std::env::var("FLEET_MEMORY_HUB_DEFAULT")
        .ok()
        .filter(|v| !v.is_empty())
}

/// Push one entry to the hub. Best-effort and bounded: a hub that is slow must
/// not make a local write slow, so the timeout is short and a failure is
/// reported rather than raised.
pub fn push(scope: &str, key: &str, value: &str, author: &str) -> Result<()> {
    let Some(h) = hub() else {
        return Ok(());
    };
    let body = json!({"scope": scope, "key": key, "value": value, "author": author}).to_string();
    let resp = ureq::post(&format!("{h}/v1/memory"))
        .timeout(Duration::from_secs(4))
        .set("Content-Type", "application/json")
        .send_string(&body);
    match resp {
        Ok(_) => Ok(()),
        Err(e) => Err(anyhow::anyhow!("hub mirror failed: {e}")),
    }
}

/// Read one key from the hub, when the local store does not have it.
pub fn fetch(scope: &str, key: &str) -> Result<Option<String>> {
    let Some(h) = hub() else {
        return Ok(None);
    };
    let url = format!("{h}/v1/memory?scope={scope}&prefix={key}&limit=50");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(4))
        .call()
        .with_context(|| format!("reading {url}"))?;
    let v: serde_json::Value = resp.into_json()?;
    let hit = v
        .get("entries")
        .and_then(|e| e.as_array())
        .and_then(|a| {
            a.iter()
                .find(|e| e.get("key").and_then(|k| k.as_str()) == Some(key))
        })
        .and_then(|e| e.get("value").and_then(|x| x.as_str()))
        .map(|s| s.to_string());
    Ok(hit)
}

/// Everything in a scope, local plus hub, deduplicated with the local value
/// winning — local is fresher for this node by definition.
pub fn merged_list(scope: &str, limit: i64) -> Result<Vec<(String, String)>> {
    let mem = crate::memory::Memory::open_default()?;
    let mut out: Vec<(String, String)> = mem.list(scope, "", limit)?;
    let Some(h) = hub() else {
        return Ok(out);
    };
    let url = format!("{h}/v1/memory?scope={scope}&limit={limit}");
    if let Ok(resp) = ureq::get(&url).timeout(Duration::from_secs(4)).call() {
        if let Ok(v) = resp.into_json::<serde_json::Value>() {
            if let Some(entries) = v.get("entries").and_then(|e| e.as_array()) {
                for e in entries {
                    let (Some(k), Some(val)) = (
                        e.get("key").and_then(|x| x.as_str()),
                        e.get("value").and_then(|x| x.as_str()),
                    ) else {
                        continue;
                    };
                    if !out.iter().any(|(lk, _)| lk == k) {
                        out.push((k.to_string(), val.to_string()));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Is the hub reachable right now? Used by the doctor and by `memory backend`.
pub fn hub_status() -> (Option<String>, bool, String) {
    let Some(h) = hub() else {
        return (None, false, "no hub configured".into());
    };
    match ureq::get(&format!("{h}/health"))
        .timeout(Duration::from_secs(3))
        .call()
    {
        Ok(r) => (Some(h), true, format!("answered {}", r.status())),
        Err(ureq::Error::Status(c, _)) => (Some(h), true, format!("answered {c}")),
        Err(e) => (Some(h), false, format!("unreachable: {e}")),
    }
}
