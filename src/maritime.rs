//! Maritime: the fleet's cloud agent machines.
//!
//! Maritime runs Firecracker micro-VMs, one per agent, with a persistent `/data`
//! and an auto-sleep policy. Measured 2026-10-27 from the hub: 4 machines exist
//! (`omp-worker-1..3` and `scarlet-asesor`), all `tier = always_on`, all asleep,
//! and the API answers in ~13 ms.
//!
//! Two keys, and they are NOT interchangeable — the API says so itself:
//!
//! * a **computers**-scoped key works on `/api/v1/computers` and the Computers
//!   MCP server, and on nothing else. Using it elsewhere returns 403 with a
//!   message naming the endpoint it IS allowed to use;
//! * an **agents**-scoped key works on `/api/agents` and the machine lifecycle.
//!
//! Getting that wrong is why a valid key reads as a rejected one, so this module
//! picks the key BY the call rather than by a global setting.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One Maritime machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub framework: Option<String>,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default, rename = "invocationCount")]
    pub invocation_count: Option<i64>,
    #[serde(default, rename = "totalComputeSeconds")]
    pub total_compute_seconds: Option<i64>,
    #[serde(default, rename = "lastActiveAt")]
    pub last_active_at: Option<String>,
    #[serde(default, rename = "sleepPolicy")]
    pub sleep_policy: Option<String>,
    #[serde(default, rename = "hasWebUi")]
    pub has_web_ui: Option<bool>,
}

fn base() -> String {
    std::env::var("MARITIME_API_BASE")
        .unwrap_or_else(|_| "https://api.maritime.sh".into())
        .trim_end_matches('/')
        .to_string()
}

/// The key for a scope. A key that is scoped to one thing is a 403 on the other,
/// so the scope is part of the call rather than a setting someone must remember.
pub fn key(scope: Scope) -> Result<String> {
    let name = match scope {
        Scope::Agents => "MARITIME_AGENTS_API_KEY",
        Scope::Computers => "MARITIME_COMPUTERS_API_KEY",
    };
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Ok(v);
        }
    }
    // Fall back to the secrets file the fleet keeps per service, so a process
    // that did not inherit the environment still works.
    let p = std::path::Path::new("/opt/fleet/secrets/maritime.env");
    if let Ok(text) = std::fs::read_to_string(p) {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix(&format!("{name}=")) {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    return Ok(v.to_string());
                }
            }
        }
    }
    anyhow::bail!("{name} is not set (see /opt/fleet/secrets/maritime.env)")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Agents,
    Computers,
}

fn get(scope: Scope, path: &str) -> Result<serde_json::Value> {
    let url = format!("{}{path}", base());
    let k = key(scope)?;
    let r = ureq::get(&url)
        .timeout(Duration::from_secs(30))
        .set("Authorization", &format!("Bearer {k}"))
        .call()
        .with_context(|| format!("GET {url}"))?;
    Ok(r.into_json().unwrap_or_default())
}

fn post(scope: Scope, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let url = format!("{}{path}", base());
    let k = key(scope)?;
    let resp = ureq::post(&url)
        .timeout(Duration::from_secs(120))
        .set("Authorization", &format!("Bearer {k}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match resp {
        Ok(r) => Ok(r.into_json().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => {
            let b = r.into_string().unwrap_or_default();
            anyhow::bail!("POST {url} -> {c}: {}", crate::runner::truncate(&b, 300))
        }
        Err(e) => anyhow::bail!("POST {url}: {e}"),
    }
}

/// Every machine on the account.
pub fn agents() -> Result<Vec<Agent>> {
    let v = get(Scope::Agents, "/api/agents")?;
    // The API returns a bare array; a wrapped object is accepted too, because a
    // future version may add pagination and this should not break silently.
    let arr = if v.is_array() {
        v
    } else {
        v.get("agents").cloned().unwrap_or(serde_json::json!([]))
    };
    serde_json::from_value(arr).context("parsing the agent list")
}

/// Machines the fleet can use, by name.
pub fn find(name: &str) -> Result<Agent> {
    agents()?
        .into_iter()
        .find(|a| a.name == name)
        .ok_or_else(|| {
            let known: Vec<String> = agents().map(|a| a.into_iter().map(|x| x.name).collect()).unwrap_or_default();
            anyhow::anyhow!("no Maritime agent named `{name}`; known: {}", known.join(", "))
        })
}

/// Wake a machine. Idempotent: an already-running machine is left alone, because
/// a restart on every call would throw away whatever is in memory.
pub fn start(name: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    if a.status == "running" || a.status == "awake" {
        return Ok(serde_json::json!({
            "agent": a.name, "status": a.status, "action": "already awake"
        }));
    }
    let v = post(
        Scope::Agents,
        &format!("/api/agents/{}/start", a.id),
        &serde_json::json!({}),
    )?;
    Ok(serde_json::json!({ "agent": a.name, "action": "started", "result": v }))
}

/// Put a machine to sleep, to stop paying for compute.
pub fn stop(name: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    let v = post(
        Scope::Agents,
        &format!("/api/agents/{}/stop", a.id),
        &serde_json::json!({}),
    )?;
    Ok(serde_json::json!({ "agent": a.name, "action": "stopped", "result": v }))
}

/// Ask a machine something. This is the one that makes a Maritime VM usable as a
/// fleet runner: the fleet's own task text goes in, an answer comes back.
pub fn chat(name: &str, message: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    post(
        Scope::Agents,
        &format!("/api/agents/{}/chat", a.id),
        &serde_json::json!({ "message": message }),
    )
}

/// Run a command on a machine.
pub fn exec(name: &str, command: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    post(
        Scope::Agents,
        &format!("/api/agents/{}/exec", a.id),
        &serde_json::json!({ "command": command }),
    )
}

/// The Computers-scoped view: a different resource with a different key.
pub fn computers() -> Result<serde_json::Value> {
    get(Scope::Computers, "/api/v1/computers")
}

/// A one-line summary per machine, for `fleet runners list`.
pub fn summary(a: &Agent) -> String {
    let mins = a.total_compute_seconds.unwrap_or(0) / 60;
    format!(
        "{:<16} {:<10} {:<10} {} invocation(s), {} compute min",
        a.name,
        a.status,
        a.tier.clone().unwrap_or_default(),
        a.invocation_count.unwrap_or(0),
        mins
    )
}

/// Is the account reachable with the keys the fleet holds?
pub fn reachable() -> (bool, String) {
    match agents() {
        Ok(a) => (true, format!("{} machine(s)", a.len())),
        Err(e) => (false, format!("{e}")),
    }
}

/// The visible screen: a Maritime machine can run a desktop, and the fleet can
/// take it over. This is the case ego lite does NOT cover — a VM with a screen
/// where the task needs a real display (an installer, a GUI-only app, a captcha
/// in a native window) rather than a browser on the operator's own machine.
///
/// Measured 2026-10-27: `desktopEnabled` starts false; enabling it restarts the
/// machine, and `desktop/mode` then reports `{"mode":"agent"}`. Enabling takes
/// ~45 s because the machine reboots, and calling `desktop/mode` before that
/// returns "Desktop control unreachable" — a timing state, not a fault.
pub fn desktop_mode(name: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    get(Scope::Agents, &format!("/api/agents/{}/desktop/mode", a.id))
}

/// Turn the desktop on or off. A no-op when it already matches, because the call
/// RESTARTS the machine and doing that on every invocation would throw away
/// whatever the agent was doing.
pub fn desktop_enable(name: &str, on: bool) -> Result<serde_json::Value> {
    let a = find(name)?;
    patch(
        Scope::Agents,
        &format!("/api/agents/{}/desktop-config", a.id),
        &serde_json::json!({ "enabled": on }),
    )
}

/// Take the screen: hand it to the agent, or back to a human.
///
/// `mode` is `agent` or `human`. Taking it from a human is recorded with a
/// reason, because a machine that silently steals a person's screen is the kind
/// of behaviour that must be attributable afterwards.
pub fn desktop_takeover(name: &str, mode: &str, reason: &str) -> Result<serde_json::Value> {
    let a = find(name)?;
    post(
        Scope::Agents,
        &format!("/api/agents/{}/desktop/takeover", a.id),
        &serde_json::json!({ "mode": mode, "reason": reason, "success": true }),
    )
}

fn patch(scope: Scope, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let url = format!("{}{path}", base());
    let k = key(scope)?;
    let resp = ureq::request("PATCH", &url)
        .timeout(Duration::from_secs(60))
        .set("Authorization", &format!("Bearer {k}"))
        .set("Content-Type", "application/json")
        .send_string(&body.to_string());
    match resp {
        Ok(r) => Ok(r.into_json().unwrap_or_default()),
        Err(ureq::Error::Status(c, r)) => {
            let b = r.into_string().unwrap_or_default();
            anyhow::bail!("PATCH {url} -> {c}: {}", crate::runner::truncate(&b, 300))
        }
        Err(e) => anyhow::bail!("PATCH {url}: {e}"),
    }
}
