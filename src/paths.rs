//! Filesystem layout. Everything is overridable by env so the same binary runs
//! from a checkout, a container, or a system service without a config file.

use std::path::{Path, PathBuf};

/// Root of all fleet state: `$FLEET_HOME`, else `$HOME/.fleet`, else `./.fleet`.
pub fn home() -> PathBuf {
    if let Some(p) = std::env::var_os("FLEET_HOME") {
        return PathBuf::from(p);
    }
    if let Some(h) = std::env::var_os("HOME") {
        return PathBuf::from(h).join(".fleet");
    }
    PathBuf::from(".fleet")
}

fn sub(name: &str) -> PathBuf {
    home().join(name)
}

/// Directory of harness descriptors (`*.toml`). Hot-reloaded on every read.
pub fn harness_dir() -> PathBuf {
    std::env::var_os("FLEET_HARNESS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| sub("harnesses"))
}

/// Directory of verifier packages used by the self-modification gate.
pub fn verifier_dir() -> PathBuf {
    sub("verifiers")
}

/// Shared memory + session database.
pub fn db_path() -> PathBuf {
    std::env::var_os("FLEET_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| sub("fleet.db"))
}

/// Append-only ledger of engine decisions and self-modification marks.
pub fn ledger_path() -> PathBuf {
    sub("ledger.jsonl")
}

/// Per-run working directories and captured transcripts.
pub fn runs_dir() -> PathBuf {
    sub("runs")
}

pub fn config_path() -> PathBuf {
    sub("fleet.toml")
}

pub fn log_path() -> PathBuf {
    sub("fleet.log")
}

/// Read `$FLEET_HOME/env` into the process environment, if it exists.
///
/// Keys are never deployed and never written into a descriptor, so they have to
/// live somewhere. A systemd unit can name that file with `EnvironmentFile=`,
/// but the engine is also run directly — by `ssh host fleet run …`, by a peer
/// driving a harness over the mesh, by a cron job — and in those cases there is
/// no unit to do it. Loading the file here makes a key behave the same however
/// the process started.
///
/// Existing environment variables always win, so a caller can override one for a
/// single run without editing a file.
pub fn load_env_file() {
    let path = home().join("env");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Both `KEY=value` and `export KEY=value` are accepted, because an env
        // file is often also sourced by a human in a shell.
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        std::env::set_var(k, v);
    }
}

/// Create every directory the engine writes to. Idempotent.
pub fn ensure_dirs() -> std::io::Result<()> {
    for d in [home(), harness_dir(), verifier_dir(), runs_dir()] {
        std::fs::create_dir_all(&d)?;
    }
    Ok(())
}

/// Expand a leading `~` and make relative paths absolute against the fleet home.
pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        home().join(path)
    }
}
