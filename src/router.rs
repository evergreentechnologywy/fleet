//! Model choice: every harness call asks the chooser which model to run with.
//!
//! Two decisions are made per call, never cached beyond a short TTL:
//!
//! * **which model**, and with what reasoning effort — asked of whichever
//!   chooser is configured (Jev by default);
//! * **which provider serves it** — that is the chooser's answer too
//!   (`serving_provider`), or the harness's own business when it routes itself.
//!
//! The chooser is pluggable, because a task that only needs one provider should
//! not need a fleet policy engine to start:
//!
//! * `cli` — run a command, read `MODEL=... EFFORT=...` or JSON from stdout.
//!   This is how Jev is reached here.
//! * `http` — POST the task to a JSON endpoint and read the fields back.
//! * `static` — no chooser at all: the configured defaults win. The from-zero
//!   path, usable with one API key and nothing else.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ChooserKind {
    Cli,
    Http,
    /// No chooser at all. The default, because a fleet with no chooser is a
    /// working fleet — just one whose model is fixed.
    #[default]
    Static,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChooserConfig {
    #[serde(default)]
    pub kind: ChooserKind,
    /// `cli`: executable.
    #[serde(default)]
    pub command: Option<String>,
    /// `cli`: argv, with `{prompt}` and `{harness}` substituted.
    #[serde(default)]
    pub args: Vec<String>,
    /// `http`: endpoint taking `{"prompt": ..., "harness": ...}`.
    #[serde(default)]
    pub url: Option<String>,
    /// `http`: header name carrying the key.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// `http`: env var holding the key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// How long a decision is reused for an identical (harness, task) pair.
    #[serde(default)]
    pub ttl_s: Option<u64>,
    /// Give up on the chooser after this long and use the fallback.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

impl ChooserConfig {
    /// The Jev default: the fleet's own chooser script, if it is installed.
    pub fn detect() -> Self {
        let jev = PathBuf::from("/opt/fleet/scripts/jev_text_route.py");
        if jev.exists() {
            return Self {
                kind: ChooserKind::Cli,
                command: Some(jev.to_string_lossy().to_string()),
                args: vec!["--request".into(), "{prompt}".into(), "--json".into()],
                ..Default::default()
            };
        }
        Self::default()
    }

    pub fn ttl(&self) -> Duration {
        Duration::from_secs(self.ttl_s.unwrap_or(300))
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_s.unwrap_or(20))
    }
}

/// What the chooser answered.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Decision {
    pub model: String,
    pub effort: String,
    #[serde(default)]
    pub lane: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub dispatch: String,
    /// Where this answer came from: `cli`, `http`, `harness`, `default`, `cache`.
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub reason: String,
}

impl Decision {
    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
    }
}

/// Defaults used when nothing else is configured — the two-key onboarding path.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Defaults {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// OpenAI-compatible base URL of a provider.
    #[serde(default)]
    pub provider_base: Option<String>,
    #[serde(default)]
    pub provider_key_env: Option<String>,
}

/// Engine configuration, loaded from `$FLEET_HOME/fleet.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub chooser: ChooserConfig,
    #[serde(default)]
    pub defaults: Defaults,
    /// Hard ceiling on concurrent harness processes in one run.
    #[serde(default)]
    pub max_parallel: Option<usize>,
}

impl Config {
    pub fn load() -> Self {
        let p = crate::paths::config_path();
        match std::fs::read_to_string(&p) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("fleet: {} is invalid ({e}); using defaults", p.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let p = crate::paths::config_path();
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", p.display()))?;
        Ok(())
    }

    pub fn parallel(&self) -> usize {
        self.max_parallel.unwrap_or(4).clamp(1, 32)
    }
}

struct CacheEntry {
    decision: Decision,
    at: Instant,
}

/// The router. Owns the chooser config, a short-lived decision cache, and the
/// per-harness policy that decides whether the chooser is consulted at all.
pub struct Router {
    pub config: Config,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl Router {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Choose model + effort for one harness call.
    ///
    /// `harness` carries the harness's own preference (route, pinned model,
    /// accepted model list); the router always wins last so a harness never
    /// receives a model it declared it cannot serve.
    pub fn decide(&self, harness: &crate::registry::Harness, task: &str) -> Decision {
        use crate::registry::Route;

        // A pinned harness spends no chooser call.
        if harness.route == Route::Fixed {
            return Decision {
                model: harness.effective_model().unwrap_or_default().to_string(),
                effort: harness.effort.clone().unwrap_or_default(),
                lane: "fixed".into(),
                source: "harness".into(),
                reason: "route = fixed".into(),
                ..Default::default()
            };
        }

        // A self-routing harness picks its own model; fleet still supplies the
        // chooser's endpoint so the harness's own choice goes through it.
        if harness.route == Route::SelfRouting {
            return Decision {
                model: harness.effective_model().unwrap_or_default().to_string(),
                effort: harness.effort.clone().unwrap_or_default(),
                lane: "self".into(),
                source: "harness".into(),
                reason: "harness routes itself through the chooser".into(),
                ..Default::default()
            };
        }

        if harness.route == Route::Native {
            return Decision {
                source: "harness".into(),
                reason: "native auth, no model injection".into(),
                ..Default::default()
            };
        }

        let key = format!("{}\u{1}{}", harness.name, task);
        if let Some(hit) = self.cache_get(&key) {
            return hit;
        }

        let mut d = self.ask(task, &harness.name).unwrap_or_else(|e| Decision {
            source: "fallback".into(),
            reason: format!("chooser failed: {e}"),
            ..Default::default()
        });

        // Harness ceiling: never hand a harness a model it did not declare.
        if !d.model.is_empty() && !harness.accepts(&d.model) {
            let fallback = harness
                .effective_model()
                .map(|s| s.to_string())
                .or_else(|| harness.models.first().cloned())
                .unwrap_or_default();
            d.reason = format!(
                "{}; {} not in harness ceiling {:?}, using {}",
                d.reason, d.model, harness.models, fallback
            );
            d.model = fallback;
        }
        if d.model.is_empty() {
            d.model = harness
                .effective_model()
                .map(|s| s.to_string())
                .or_else(|| self.config.defaults.model.clone())
                .unwrap_or_default();
        }
        if d.effort.is_empty() {
            d.effort = harness
                .effort
                .clone()
                .or_else(|| self.config.defaults.effort.clone())
                .unwrap_or_else(|| "medium".to_string());
        }
        self.cache_put(key, d.clone());
        d
    }

    fn cache_get(&self, key: &str) -> Option<Decision> {
        let ttl = self.config.chooser.ttl();
        let mut c = self.cache.lock().unwrap();
        match c.get(key) {
            Some(e) if e.at.elapsed() < ttl => {
                let mut d = e.decision.clone();
                d.source = "cache".into();
                Some(d)
            }
            Some(_) => {
                c.remove(key);
                None
            }
            None => None,
        }
    }

    fn cache_put(&self, key: String, d: Decision) {
        let mut c = self.cache.lock().unwrap();
        if c.len() > 512 {
            c.clear();
        }
        c.insert(
            key,
            CacheEntry {
                decision: d,
                at: Instant::now(),
            },
        );
    }

    /// Ask the configured chooser. Any failure is recoverable by design.
    fn ask(&self, task: &str, harness: &str) -> Result<Decision> {
        match self.config.chooser.kind {
            ChooserKind::Static => bail!("chooser kind is `static`"),
            ChooserKind::Cli => self.ask_cli(task, harness),
            ChooserKind::Http => self.ask_http(task, harness),
        }
    }

    fn ask_cli(&self, task: &str, harness: &str) -> Result<Decision> {
        let cmd = self
            .config
            .chooser
            .command
            .clone()
            .ok_or_else(|| anyhow!("chooser.command is not set"))?;
        // A very long task is bad for argv; the head decides the routing.
        let brief: String = task.chars().take(4000).collect();
        let args: Vec<String> = self
            .config
            .chooser
            .args
            .iter()
            .map(|a| a.replace("{prompt}", &brief).replace("{harness}", harness))
            .collect();
        let out = crate::runner::run_capture(&cmd, &args, &[], None, self.config.chooser.timeout())
            .context("running chooser")?;
        if !out.ok {
            bail!(
                "chooser exited {}: {}",
                out.code,
                crate::runner::truncate(&out.stderr, 400)
            );
        }
        parse_decision(&out.stdout)
    }

    fn ask_http(&self, task: &str, harness: &str) -> Result<Decision> {
        let url = self
            .config
            .chooser
            .url
            .clone()
            .ok_or_else(|| anyhow!("chooser.url is not set"))?;
        let mut req = ureq::post(&url);
        if let Some(h) = &self.config.chooser.auth_header {
            if let Some(env) = &self.config.chooser.api_key_env {
                if let Ok(k) = std::env::var(env) {
                    req = req.set(h, &k);
                }
            }
        }
        let body = serde_json::json!({ "prompt": task, "harness": harness, "protocol": "fleet/chooser/1" });
        let resp = req
            .timeout(self.config.chooser.timeout())
            .send_json(body)
            .context("chooser http call")?;
        let text = resp.into_string()?;
        parse_decision(&text)
    }

    /// Client configuration for a harness that routes itself: point it at the
    /// chooser's OpenAI-compatible door so its own model choice is still made
    /// by the fleet, not by a hardcoded id in its config.
    pub fn self_routing_env(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let base = std::env::var("FLEET_CHOOSER_OPENAI_BASE")
            .ok()
            .or_else(|| self.config.defaults.provider_base.clone());
        match base {
            Some(b) => {
                out.push(("OPENAI_BASE_URL".into(), b.clone()));
                out.push(("OPENAI_API_BASE".into(), b.clone()));
                out.push(("FLEET_CHOOSER_BASE_URL".into(), b.clone()));
                out.push(("FLEET_CHOOSER_BASE".into(), b));
            }
            None => out.push((
                "FLEET_CHOOSER_BASE".into(),
                "http://127.0.0.1:8792/v1".into(),
            )),
        }
        if let Some(env) = &self.config.defaults.provider_key_env {
            if let Ok(k) = std::env::var(env) {
                out.push(("OPENAI_API_KEY".into(), k));
            }
        }
        out
    }

    /// The endpoint a self-routing harness should be pointed at, or `None` when
    /// nothing is configured (in which case the harness keeps its own config).
    pub fn chooser_base(&self) -> Option<String> {
        if let Ok(v) = std::env::var("FLEET_CHOOSER_OPENAI_BASE") {
            return Some(v);
        }
        if let Some(v) = &self.config.defaults.provider_base {
            return Some(v.clone());
        }
        None
    }
}

/// Read a decision out of chooser output. Accepts three shapes so any chooser
/// works without a shim: a `MODEL=... EFFORT=...` line, a JSON object, or JSON
/// embedded in other output.
pub fn parse_decision(text: &str) -> Result<Decision> {
    let trimmed = text.trim();
    // JSON first: an object anywhere in the output.
    if let Some(start) = trimmed.find('{') {
        if let Some(end) = trimmed.rfind('}') {
            if end > start {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                    let get = |keys: &[&str]| -> String {
                        for k in keys {
                            if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                                return s.to_string();
                            }
                        }
                        String::new()
                    };
                    let d = Decision {
                        model: get(&["model", "MODEL", "model_id", "id"]),
                        effort: get(&["effort", "EFFORT", "reasoning_effort", "thinking"]),
                        lane: get(&["lane", "LANE", "provider_lane", "route"]),
                        provider: get(&["serving_provider", "provider", "PROVIDER"]),
                        dispatch: get(&["dispatch", "DISPATCH"]),
                        source: "chooser".into(),
                        reason: get(&["reason", "REASON", "why", "rationale"]),
                    };
                    if !d.model.is_empty() {
                        return Ok(d);
                    }
                }
            }
        }
    }
    // KEY=VALUE lines.
    let mut d = Decision {
        source: "chooser".into(),
        ..Default::default()
    };
    for tok in trimmed.split_whitespace() {
        if let Some((k, v)) = tok.split_once('=') {
            match k.trim().to_ascii_uppercase().as_str() {
                "MODEL" => d.model = v.trim().to_string(),
                "EFFORT" => d.effort = v.trim().to_string(),
                "LANE" => d.lane = v.trim().to_string(),
                "PROVIDER" | "SERVING_PROVIDER" => d.provider = v.trim().to_string(),
                "DISPATCH" => d.dispatch = v.trim().to_string(),
                "REASON" => d.reason = v.trim().to_string(),
                _ => {}
            }
        }
    }
    if d.model.is_empty() {
        bail!(
            "chooser returned no model: {}",
            crate::runner::truncate(trimmed, 300)
        );
    }
    Ok(d)
}
