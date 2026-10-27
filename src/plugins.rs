//! Plugins: one extension point that every harness gets, because it lives in
//! the engine's call path rather than in each CLI's own config.
//!
//! A plugin is a directory with `plugin.toml` and a script. The script is
//! invoked with the event name and a JSON payload, and may print JSON on stdout
//! to change what happens next. That is the whole contract:
//!
//!   event = "pre_call"   payload = {task, harness, model, effort, dir}
//!                        stdout  = {"env":{"K":"V"}, "args_append":["..."],
//!                                   "prompt_prefix":"...", "skip":false,
//!                                   "replace_prompt":"..."}
//!   event = "post_call"  payload = {…, ok, ms, output, error}
//!                        stdout  = {"memory":{"key":"value"}, "note":"…"}
//!   event = "pre_run" / "post_run"  payload = {task, plan, calls, answer}
//!
//! Because plugins are engine-level, a plugin written once applies to **every**
//! harness — omp, codex, a raw provider, a harness someone adds tomorrow. A
//! plugin may narrow itself with `harnesses = [...]`, and a harness may opt out
//! with `plugins = false`. Nothing else is required: drop the directory in,
//! it is picked up on the next call.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Events this plugin wants. Empty means every event.
    #[serde(default)]
    pub events: Vec<String>,
    /// Restrict to these harnesses. Empty means all of them.
    #[serde(default)]
    pub harnesses: Vec<String>,
    /// Restrict to these providers, when the payload names one.
    #[serde(default)]
    pub providers: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Higher runs first within an event.
    #[serde(default)]
    pub priority: i32,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// What a plugin may change about the call it is attached to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Effects {
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub args_append: Vec<String>,
    #[serde(default)]
    pub prompt_prefix: String,
    #[serde(default)]
    pub prompt_suffix: String,
    #[serde(default)]
    pub replace_prompt: Option<String>,
    /// Veto the call. The engine records a skipped call in the ledger.
    #[serde(default)]
    pub skip: bool,
    #[serde(default)]
    pub skip_reason: String,
    /// Values the plugin wants written to shared memory after the call.
    #[serde(default)]
    pub memory: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub note: String,
}

impl Effects {
    fn merge(&mut self, other: Effects) {
        for (k, v) in other.env {
            self.env.insert(k, v);
        }
        self.args_append.extend(other.args_append);
        if !other.prompt_prefix.is_empty() {
            self.prompt_prefix = format!("{}{}", other.prompt_prefix, self.prompt_prefix);
        }
        if !other.prompt_suffix.is_empty() {
            self.prompt_suffix = format!("{}{}", self.prompt_suffix, other.prompt_suffix);
        }
        if other.replace_prompt.is_some() {
            self.replace_prompt = other.replace_prompt;
        }
        if other.skip {
            self.skip = true;
            self.skip_reason = other.skip_reason;
        }
        for (k, v) in other.memory {
            self.memory.insert(k, v);
        }
        if !other.note.is_empty() {
            self.note = other.note;
        }
    }
}

/// The loaded plugin set. Re-read per call so adding or deleting a plugin needs
/// no restart, exactly like a harness.
pub struct Plugins {
    dir: PathBuf,
}

impl Plugins {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn default_dir() -> Self {
        Self::new(crate::paths::home().join("plugins"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every installed plugin, valid ones only; a broken plugin is reported and
    /// skipped rather than taking the engine down.
    pub fn all(&self) -> Vec<PluginSpec> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            if !d.is_dir() {
                continue;
            }
            let spec_path = d.join("plugin.toml");
            let Ok(text) = std::fs::read_to_string(&spec_path) else {
                continue;
            };
            match toml::from_str::<PluginSpec>(&text) {
                Ok(s) if s.enabled => out.push(s),
                Ok(_) => {}
                Err(e) => eprintln!("fleet: plugin {} invalid: {e}", d.display()),
            }
        }
        out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.name.cmp(&b.name)));
        out
    }

    /// Plugins that should run for this event on this harness/provider.
    pub fn matching(&self, event: &str, harness: &str, provider: &str) -> Vec<PluginSpec> {
        self.all()
            .into_iter()
            .filter(|p| p.events.is_empty() || p.events.iter().any(|e| e == event))
            .filter(|p| {
                p.harnesses.is_empty() || p.harnesses.iter().any(|h| h == harness || h == "*")
            })
            .filter(|p| {
                p.providers.is_empty()
                    || provider.is_empty()
                    || p.providers.iter().any(|x| x == provider || x == "*")
            })
            .collect()
    }

    /// Fire one event. Each plugin sees the payload, and its effects are merged
    /// in name order so the result is deterministic.
    pub fn fire(
        &self,
        event: &str,
        harness: &str,
        provider: &str,
        payload: &serde_json::Value,
    ) -> Effects {
        let mut effects = Effects::default();
        for p in self.matching(event, harness, provider) {
            let body = serde_json::json!({
                "protocol": "fleet/plugin/1",
                "event": event,
                "harness": harness,
                "provider": provider,
                "payload": payload,
            });
            let mut args = p.args.clone();
            args.push(event.to_string());
            // The payload goes in on stdin; args carry the event name so a
            // two-line shell plugin can branch on it without a JSON parser.
            // Run from the plugin's own directory: `args = ["plugin.sh"]` is
            // the natural way to write a plugin, and a relative path only works
            // if the working directory is the package.
            let dir = self.dir.join(&p.name);
            let cwd = if dir.is_dir() {
                Some(dir.as_path())
            } else {
                None
            };
            match run_with_stdin(
                &p.command,
                &args,
                &serde_json::to_string(&body).unwrap_or_default(),
                Duration::from_secs(p.timeout_s.unwrap_or(30)),
                cwd,
            ) {
                Ok((code, out, err)) => {
                    if code != 0 {
                        eprintln!(
                            "fleet: plugin `{}` ({event}) exited {code}: {}",
                            p.name,
                            crate::runner::truncate(err.trim(), 200)
                        );
                        continue;
                    }
                    let trimmed = out.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Effects>(trimmed) {
                        Ok(mut e) => {
                            if !p.name.is_empty() {
                                e.note = if e.note.is_empty() {
                                    p.name.clone()
                                } else {
                                    format!("{};{}", p.name, e.note)
                                };
                            }
                            effects.merge(e);
                        }
                        Err(e) => eprintln!(
                            "fleet: plugin `{}` ({event}) returned non-JSON: {e}",
                            p.name
                        ),
                    }
                }
                Err(e) => eprintln!("fleet: plugin `{}` ({event}) failed: {e}", p.name),
            }
        }
        effects
    }
}

/// Run a helper with a body on stdin and a hard deadline.
fn run_with_stdin(
    cmd: &str,
    args: &[String],
    input: &str,
    timeout: Duration,
    cwd: Option<&Path>,
) -> Result<(i32, String, String)> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut cmd_builder = Command::new(cmd);
    cmd_builder
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd_builder.current_dir(d);
    }
    let mut child = cmd_builder
        .spawn()
        .with_context(|| format!("spawning `{cmd}`"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(input.as_bytes());
        let _ = si.flush();
    }
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("`{cmd}` exceeded {}s", timeout.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(25)),
        }
    }
    let out = child.wait_with_output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// Install a plugin directory into the fleet, then report it as loaded.
pub fn install(src: &Path) -> Result<PluginSpec> {
    let spec_path = if src.is_dir() {
        src.join("plugin.toml")
    } else {
        src.to_path_buf()
    };
    let text = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let spec: PluginSpec =
        toml::from_str(&text).with_context(|| format!("parsing {}", spec_path.display()))?;
    let dest_dir = Plugins::default_dir().dir().join(&spec.name);
    std::fs::create_dir_all(&dest_dir)?;
    std::fs::write(dest_dir.join("plugin.toml"), &text)?;
    // Copy sibling scripts so a plugin package is self-contained after install.
    if let Some(parent) = spec_path.parent() {
        if parent != dest_dir {
            if let Ok(entries) = std::fs::read_dir(parent) {
                for e in entries.flatten() {
                    let p = e.path();
                    if p.is_dir() || p.file_name().map(|f| f == "plugin.toml").unwrap_or(false) {
                        continue;
                    }
                    let name = p.file_name().unwrap().to_string_lossy().to_string();
                    let _ = std::fs::copy(&p, dest_dir.join(&name));
                    // Preserve the executable bit; a plugin that cannot run is
                    // indistinguishable from a plugin that is not installed.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(meta) = std::fs::metadata(&p) {
                            let mode = meta.permissions().mode();
                            let _ = std::fs::set_permissions(
                                dest_dir.join(&name),
                                std::fs::Permissions::from_mode(mode),
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(spec)
}

/// Remove an installed plugin.
pub fn remove(name: &str) -> Result<PathBuf> {
    let dir = Plugins::default_dir().dir().join(name);
    if !dir.exists() {
        anyhow::bail!(
            "no plugin named `{name}` in {}",
            Plugins::default_dir().dir().display()
        );
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(dir)
}

/// The built-in example plugin, written by `fleet onboard` so the hook shape is
/// visible as a working file instead of only as documentation.
pub const EXAMPLE: &str = r#"#!/bin/sh
# An example fleet plugin. It sees every event and changes one thing: it adds a
# stamp to the prompt so a harness can tell that the fleet wrapped it.
event="$1"
payload=$(cat)
case "$event" in
  pre_call)
    echo '{"prompt_prefix":"[fleet] "}'
    ;;
  post_call)
    # Record something every run, from any harness, with no per-harness config.
    echo '{"memory":{"last_wrapped_call":"yes"}}'
    ;;
  *)
    echo '{}'
    ;;
esac
"#;

/// Descriptor text for the example plugin.
pub const EXAMPLE_SPEC: &str = r#"name = "example"
description = "Stamp every prompt; proves the hook path end to end"
# Empty means every event and every harness.
events = ["pre_call", "post_call"]
harnesses = []
command = "sh"
args = ["plugin.sh"]
timeout_s = 15
priority = 0
enabled = false
"#;
