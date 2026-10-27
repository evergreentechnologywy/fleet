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
use parking_lot::Mutex;
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
    /// What the fleet should optimise for when the chooser has a choice.
    ///
    /// This is the owner's dial, and it is deliberately three words rather than a
    /// number: `cost` (spend as little as possible), `speed` (first token fast),
    /// `quality` (the most capable model for the job, latency be damned), and
    /// `balanced` (the default: cost first with speed as the tie-break, which is
    /// what the fleet has always done).
    #[serde(default)]
    pub priority: Priority,
    /// Per-harness overrides, so a harness that is only used for one job can have
    /// its own priority without changing the fleet's.
    #[serde(default)]
    pub priority_by_harness: std::collections::HashMap<String, Priority>,
}

/// What the fleet optimises for. Maps onto the chooser's own vocabulary
/// (`--latency`) plus `--budget`, so the meaning lives in one place and this enum
/// is just the fleet-facing name for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Cheapest acceptable model. The owner's own default.
    Cost,
    /// Fastest first token.
    Speed,
    /// Most capable model for the job; latency is not a constraint.
    Quality,
    /// Cost first, speed as the tie-break. The historical behaviour.
    #[default]
    Balanced,
}

impl Priority {
    /// The chooser flags this priority turns into.
    ///
    /// `cost` and `balanced` both lean on the chooser's cost model (price per
    /// call x credit pace); `speed` and `quality` set the latency need, which is
    /// the axis the chooser actually balances capability against.
    pub fn chooser_args(&self) -> Vec<String> {
        match self {
            Priority::Cost => vec!["--latency".into(), "balanced".into()],
            Priority::Balanced => vec!["--latency".into(), "balanced".into()],
            Priority::Speed => vec!["--latency".into(), "fast".into()],
            Priority::Quality => vec!["--latency".into(), "quality".into()],
        }
    }

    /// One line explaining what this priority will do, for `fleet config show`.
    pub fn explain(&self) -> &'static str {
        match self {
            Priority::Cost => "cheapest acceptable model; latency is not a goal",
            Priority::Speed => "fastest first token; capability is the tie-break",
            Priority::Quality => "most capable model for the job; latency is not a constraint",
            Priority::Balanced => "cost first, speed as the tie-break (the historical behaviour)",
        }
    }
}

impl ChooserConfig {
    /// The priority in effect for this harness: its override, else the fleet's.
    pub fn priority_for(&self, harness: &str) -> Priority {
        self.priority_by_harness
            .get(harness)
            .copied()
            .unwrap_or(self.priority)
    }
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
    /// Which memory backend to use, and its settings. Absent means `sqlite` at
    /// the default path, which is what every existing install already is.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// What to try when a harness fails.
    #[serde(default)]
    pub fallback: FallbackConfig,
}

/// What to try when a harness fails.
///
/// The chain names HARNESSES, because a provider cannot run a task on its own —
/// it is reached through a harness. The default is the direct-provider harnesses,
/// which need no CLI and so cannot fail for a CLI-shaped reason.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FallbackConfig {
    /// Harness names to try, in order, after the first one fails.
    #[serde(default)]
    pub chain: Vec<String>,
    /// Hard cap on how many alternates one task may try, so a failure cannot
    /// become a storm.
    #[serde(default)]
    pub max_attempts: Option<usize>,
}

/// How the fleet remembers. A backend is a name the engine understands plus the
/// settings that name needs, so switching memory is a config edit rather than a
/// migration, and the doctor can report the active backend and the alternatives.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    /// `sqlite` (the default), `holographic` (the fleet's uf.db), `postgres`,
    /// or `http` for a service that speaks the same JSON.
    #[serde(default)]
    pub backend: Option<String>,
    /// Where the backend lives, when that is not its default.
    #[serde(default)]
    pub path: Option<String>,
    /// Endpoint for an `http` backend.
    #[serde(default)]
    pub url: Option<String>,
    /// Env var holding a token for an `http` backend.
    #[serde(default)]
    pub token_env: Option<String>,
    /// The node whose `fleet serve` is the shared memory hub. Every write is
    /// mirrored there and every read falls back to it, which is what makes one
    /// key visible on every node. Absent means node-local memory.
    #[serde(default)]
    pub hub: Option<String>,
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

        let mut d = match self.ask(task, &harness.name) {
            Ok(d) => d,
            Err(e) => {
                // The chooser is the one component whose failure takes the whole
                // fleet down, so a failure here must still produce a usable
                // decision. The configured default model is used when there is
                // one; otherwise the harness's own first declared model; and
                // only if both are absent does the decision stay empty, which the
                // caller reports rather than silently running nothing.
                let m = self
                    .config
                    .defaults
                    .model
                    .clone()
                    .filter(|m| !m.is_empty())
                    .or_else(|| harness.models.iter().find(|m| *m != "*").cloned())
                    .unwrap_or_default();
                Decision {
                    model: m,
                    effort: self
                        .config
                        .defaults
                        .effort
                        .clone()
                        .or_else(|| harness.effort.clone())
                        .unwrap_or_default(),
                    lane: "fallback".into(),
                    source: "fallback".into(),
                    reason: format!("chooser failed ({e}); used the configured default"),
                    ..Default::default()
                }
            }
        };

        // Harness ceiling: never hand a harness a model it did not declare.
        //
        // But a declaration can name a ROUTER ALIAS rather than a model. A
        // harness with `route = "jev"` that reaches the chooser's OpenAI door
        // declares `models = ["jev/auto", "auto"]`, because the door resolves an
        // authorized model itself — the alias IS its contract. Clamping such a
        // harness to its own alias list rejected every real decision, which was
        // measured 492 times in one ledger: each rejection quietly swapped the
        // chooser's pick for the alias and rewrote `reason`, so the run looked
        // routed while a fixed model was actually used.
        //
        // So a field only limits itself when it declares fields. An alias-only
        // list, or one containing "*", constrains nothing by definition and must
        // not be reported as if it did.
        let clamps = harness.models.iter().any(|m| crate::registry::is_model_field(m));
        // A harness that declares ONLY router aliases is telling us what it wants
        // to RECEIVE, not what it can serve: `route = "jev"` with
        // `models = ["jev/auto"]` reaches the chooser's door, and the door
        // resolves the real model itself. Handing it the chooser's concrete pick
        // instead bypasses the very indirection it was built for — measured
        // 2026-10-27, `omp` then received `muse-spark-1.3-contributor` and got
        // HTTP 403 "access has been restricted", while the alias answered fine.
        if !clamps && !d.model.is_empty() {
            if let Some(alias) = harness
                .models
                .iter()
                .find(|m| m.as_str() == "jev/auto" || m.as_str() == "auto")
            {
                if d.model != *alias {
                    d.reason = format!(
                        "{}; `{}` self-routes through the chooser door, handing it `{}`",
                        d.reason, harness.name, alias
                    );
                    d.model = alias.clone();
                }
            }
        }
        if clamps && !d.model.is_empty() && !harness.accepts(&d.model) {
            let fallback = harness
                .effective_model()
                .map(|s| s.to_string())
                .or_else(|| {
                    harness
                        .models
                        .iter()
                        .find(|m| crate::registry::is_model_field(m))
                        .cloned()
                })
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
        let mut c = self.cache.lock();
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
        let mut c = self.cache.lock();
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
        let mut args: Vec<String> = self
            .config
            .chooser
            .args
            .iter()
            .map(|a| {
                a.replace("{prompt}", &brief)
                    .replace("{harness}", harness)
                    .replace("{priority}", &format!("{:?}", self.config.chooser.priority_for(harness)).to_lowercase())
            })
            .collect();
        // The priority dial becomes chooser flags. Appended rather than baked
        // into `args` so a caller can change the priority without editing the
        // command line, and so an explicit flag in `args` still wins by coming
        // first for the chooser's own argument parser.
        let prio = self.config.chooser.priority_for(harness);
        args.extend(prio.chooser_args());
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
