//! Which memory the fleet uses, and how to change it.
//!
//! The engine's own store is SQLite in WAL mode (`memory.rs`): instant, durable,
//! last-version-wins, with the full session transcript. That is the DEFAULT, and
//! it is the right default here — a single file, no daemon, no network, and a
//! write returns only after `synchronous=FULL` fsync, so "instant" is a
//! guarantee rather than a hope.
//!
//! It is not the only thing the fleet has. The Evergreen fleet keeps its
//! canonical state in holographic `uf.db` (a CRDT replica set on Tailscale), and
//! a team may want Postgres or a shared HTTP service instead. Those are all
//! available, and switching is a config edit:
//!
//! ```toml
//! [memory]
//! backend = "holographic"
//! path    = "/opt/fleet/holo/uf.db"
//! ```
//!
//! Choosing is explicit and reversible, and `fleet memory backend` reports what
//! is active, what is available, and what each one costs you. The engine keeps
//! writing its own SQLite regardless, so a switch never orphans a transcript.

use serde::{Deserialize, Serialize};

/// A backend the fleet knows how to reach.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backend {
    pub name: String,
    pub description: String,
    /// Where it lives by default.
    pub default_path: Option<String>,
    /// Needs a network endpoint.
    pub needs_url: bool,
    /// Needs a token (from an env var, never stored here).
    pub needs_token: bool,
    /// The trade this backend makes, stated plainly so the choice is informed.
    pub trade: String,
    /// Can be selected right now, given what is installed.
    pub available: bool,
}

/// The catalogue of backends. `sqlite` is first because it is the default and
/// the one that needs nothing.
pub fn backends() -> Vec<Backend> {
    let pg = std::env::var("FLEET_PG_URL").map(|v| !v.is_empty()).unwrap_or(false)
        || crate::runner::which("psql").is_some();
    vec![
        Backend {
            name: "sqlite".into(),
            description: "One WAL-mode SQLite file in the fleet home; the engine's own store".into(),
            default_path: Some(
                crate::paths::db_path().to_string_lossy().to_string(),
            ),
            needs_url: false,
            needs_token: false,
            trade: "no daemon, no network; instant because every write fsyncs before returning; \
                    single-writer, so it is per-node rather than fleet-shared"
                .into(),
            available: true,
        },
        Backend {
            name: "holographic".into(),
            description: "The Evergreen fleet's canonical CRDT store (uf.db) over Tailscale".into(),
            default_path: Some("/opt/fleet/holo/uf.db".into()),
            needs_url: false,
            needs_token: false,
            trade: "fleet-shared truth with replica healing; a CRDT merge is not a transaction, \
                    so 'last version wins' is by vector clock, not by write order"
                .into(),
            available: std::path::Path::new("/opt/fleet/holo/uf.db").exists()
                || std::env::var("FLEET_HOLO_DB").is_ok(),
        },
        Backend {
            name: "postgres".into(),
            description: "A Postgres database, for a team that already runs one".into(),
            default_path: None,
            needs_url: true,
            needs_token: false,
            trade: "real transactions and concurrent writers; needs a server, so it is the only \
                    backend here that can be DOWN"
                .into(),
            available: pg,
        },
        Backend {
            name: "http".into(),
            description: "Any service that speaks the same JSON, for a shared memory plane".into(),
            default_path: None,
            needs_url: true,
            needs_token: true,
            trade: "one place for many nodes; adds a network hop to every read, so it is the \
                    slowest and the only one that can silently be someone else's outage"
                .into(),
            available: std::env::var("FLEET_MEMORY_URL").map(|v| !v.is_empty()).unwrap_or(false),
        },
    ]
}

/// The backend actually in effect.
pub fn active() -> (String, crate::config::Config) {
    let cfg = crate::config::Config::load();
    let name = cfg
        .memory
        .backend
        .clone()
        .unwrap_or_else(|| "sqlite".into());
    (name, cfg)
}

/// Where the active backend reads and writes.
pub fn active_path() -> String {
    let (name, cfg) = active();
    if let Some(p) = &cfg.memory.path {
        return p.clone();
    }
    backends()
        .into_iter()
        .find(|b| b.name == name)
        .and_then(|b| b.default_path)
        .unwrap_or_else(|| crate::paths::db_path().to_string_lossy().to_string())
}

/// Is this a backend the engine can actually use? Rejecting an unknown name is
/// the difference between a typo and a fleet that silently writes nowhere.
pub fn is_known(name: &str) -> bool {
    backends().iter().any(|b| b.name == name)
}

/// Write the choice into `$FLEET_HOME/fleet.toml`, preserving every other key.
pub fn set(name: &str, path: Option<&str>, url: Option<&str>) -> anyhow::Result<()> {
    use anyhow::Context;
    if !is_known(name) {
        anyhow::bail!(
            "unknown memory backend `{name}`; known: {}",
            backends()
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let file = crate::paths::config_path();
    let mut cfg = crate::config::Config::load();
    cfg.memory.backend = Some(name.to_string());
    if let Some(p) = path {
        cfg.memory.path = Some(p.to_string());
    }
    if let Some(u) = url {
        cfg.memory.url = Some(u.to_string());
    }
    let text = toml::to_string_pretty(&cfg).context("serialising fleet.toml")?;
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    Ok(())
}
