//! Providers: one shared definition, usable by every harness.
//!
//! A provider is a credential plus a base URL. It is declared **once**, in
//! `$FLEET_HOME/providers.toml`, and then every harness may use it — a CLI
//! harness through the environment the engine injects, an HTTP harness through
//! `$FLEET_PROVIDER_<NAME>_URL` / `_KEY`, and a self-routing harness through
//! `OPENAI_BASE_URL`. Nothing about a provider is specific to a harness.
//!
//! Two deliberate asymmetries:
//!
//! * **Sharing is the default.** A new harness inherits the whole provider set
//!   the moment its file lands, because that is what "a harness for harnesses"
//!   means: the harness brings its skill, the fleet brings the plumbing.
//! * **Blocking is explicit and per-harness.** `block` on a provider names
//!   harnesses that must not receive it; `deny_providers` on a harness names
//!   providers it must never be offered. Both directions exist because a fleet
//!   policy (`PROVIDER_POLICY`) and a local preference (`harness.toml`) are
//!   different authorities and neither should have to know the other's file.
//!
//! Nothing here ever prints a key. A provider's key is resolved at call time and
//! injected into the child process environment only.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    /// Lower-case id used in `$FLEET_PROVIDER_<NAME>_*` and in block rules.
    pub name: String,
    /// OpenAI-compatible base URL, e.g. `https://api.example.ai/v1`.
    pub base_url: String,
    /// Env var holding the key. Never the key itself.
    pub key_env: String,
    /// Optional second credential this provider needs (an org id, a project).
    #[serde(default)]
    pub extra: HashMap<String, String>,
    /// Models this provider can serve. `["*"]` means unpinned.
    #[serde(default)]
    pub models: Vec<String>,
    /// Harnesses that must NEVER receive this provider. The fleet's policy says
    /// the adult/proxy lanes are not for a given harness, so it is expressed
    /// here and enforced by the engine, not by convention.
    #[serde(default)]
    pub block: Vec<String>,
    /// Set false to keep a provider declared but unreachable.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub notes: String,
}

fn yes() -> bool {
    true
}

impl Provider {
    /// Is this provider allowed for this harness?
    pub fn allows(&self, harness: &str, harness_denies: &[String]) -> bool {
        if !self.enabled {
            return false;
        }
        if self.block.iter().any(|b| b == harness || b == "*") {
            return false;
        }
        if harness_denies.iter().any(|d| d == &self.name || d == "*") {
            return false;
        }
        true
    }

    pub fn accepts(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == model || m == "*")
    }

    /// Resolve the key from the environment. Returns `None` when unset, which
    /// is a reported condition, not an error: a provider with no key is simply
    /// unavailable, like a harness with no binary.
    pub fn key(&self) -> Option<String> {
        std::env::var(&self.key_env).ok().filter(|k| !k.is_empty())
    }

    pub fn available(&self) -> bool {
        self.key().is_some()
    }

    /// Environment a CLI harness needs to reach this provider, for each of the
    /// naming conventions the common CLIs use.
    pub fn env(&self) -> Vec<(String, String)> {
        let Some(key) = self.key() else {
            return Vec::new();
        };
        let upper = self.name.to_ascii_uppercase().replace('-', "_");
        let mut v = vec![
            (format!("FLEET_PROVIDER_{upper}_URL"), self.base_url.clone()),
            (format!("FLEET_PROVIDER_{upper}_KEY"), key.clone()),
            ("OPENAI_BASE_URL".into(), self.base_url.clone()),
            ("OPENAI_API_BASE".into(), self.base_url.clone()),
            ("OPENAI_API_KEY".into(), key.clone()),
            ("OPENAI_BASE".into(), self.base_url.clone()),
            // Anthropic-shaped CLIs read these two instead.
            ("ANTHROPIC_BASE_URL".into(), self.base_url.clone()),
            ("ANTHROPIC_API_KEY".into(), key.clone()),
            ("ANTHROPIC_AUTH_TOKEN".into(), key.clone()),
        ];
        for (k, val) in &self.extra {
            v_push_env(&mut v, k, val);
        }
        v
    }
}

fn v_push_env(v: &mut Vec<(String, String)>, k: &str, val: &str) {
    let resolved = match val.strip_prefix("env:") {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => val.to_string(),
    };
    if !resolved.is_empty() {
        v.push((k.to_string(), resolved));
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderFile {
    #[serde(default)]
    pub provider: Vec<Provider>,
}

/// The shared provider set, re-read per call so adding one takes effect at once.
pub struct Providers {
    file: std::path::PathBuf,
}

impl Providers {
    pub fn new(file: std::path::PathBuf) -> Self {
        Self { file }
    }

    pub fn default_file() -> Self {
        Self::new(crate::paths::home().join("providers.toml"))
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    /// Every declared provider. A malformed file is reported and treated as
    /// empty rather than crashing a run.
    pub fn all(&self) -> Vec<Provider> {
        let Ok(text) = std::fs::read_to_string(&self.file) else {
            return Vec::new();
        };
        match toml::from_str::<ProviderFile>(&text) {
            Ok(f) => f.provider,
            Err(e) => {
                eprintln!("fleet: {} invalid: {e}", self.file.display());
                Vec::new()
            }
        }
    }

    /// Providers this harness may be offered, in declaration order.
    pub fn for_harness(&self, harness: &str, denies: &[String]) -> Vec<Provider> {
        self.all()
            .into_iter()
            .filter(|p| p.allows(harness, denies))
            .collect()
    }

    /// The provider that should serve this model for this harness: the first
    /// allowed one that declares the model, else the first allowed at all.
    pub fn pick(&self, harness: &str, denies: &[String], model: &str) -> Option<Provider> {
        let allowed = self.for_harness(harness, denies);
        allowed
            .iter()
            .find(|p| !model.is_empty() && p.accepts(model) && p.available())
            .or_else(|| allowed.iter().find(|p| p.available()))
            .or_else(|| allowed.first())
            .cloned()
    }

    pub fn get(&self, name: &str) -> Option<Provider> {
        self.all().into_iter().find(|p| p.name == name)
    }

    /// Add or replace one provider and persist the file.
    pub fn put(&self, p: &Provider) -> Result<()> {
        let mut all = self.all();
        all.retain(|x| x.name != p.name);
        all.push(p.clone());
        self.write_all(&all)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let mut all = self.all();
        let before = all.len();
        all.retain(|x| x.name != name);
        if all.len() == before {
            anyhow::bail!("no provider named `{name}` in {}", self.file.display());
        }
        self.write_all(&all)
    }

    fn write_all(&self, all: &[Provider]) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = ProviderFile {
            provider: all.to_vec(),
        };
        let body = format!(
            "# Providers are fleet-wide: declare once, every harness can use it.\n\
             # A key is always named, never written, so this file stays publishable.\n\
             #\n\
             # [[provider]]\n\
             # name = \"myprovider\"\n\
             # base_url = \"https://api.myprovider.ai/v1\"\n\
             # key_env = \"MYPROVIDER_API_KEY\"\n\
             # models = [\"*\"]\n\
             # # Harnesses that must never receive it:\n\
             # block = []\n\n{}",
            toml::to_string_pretty(&f)?
        );
        // Written by rename so a concurrent reader never parses half a file.
        let tmp = self.file.with_extension("toml.tmp");
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file)
            .with_context(|| format!("installing {}", self.file.display()))?;
        Ok(())
    }

    /// Everything a CLI harness needs at call time: the allowed providers'
    /// env, plus a compact inventory it can read to discover what it has.
    pub fn env_for(&self, harness: &str, denies: &[String]) -> Vec<(String, String)> {
        let allowed = self.for_harness(harness, denies);
        let mut out = Vec::new();
        for p in &allowed {
            out.extend(p.env());
        }
        // The primary provider wins any conflict by being applied last, which
        // matters because each provider exports the generic OPENAI_* names.
        if let Some(primary) = allowed.last() {
            out.extend(primary.env());
        }
        let inventory: Vec<serde_json::Value> = allowed
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "base_url": p.base_url,
                    "key_env": p.key_env,
                    "models": p.models,
                    "available": p.available(),
                })
            })
            .collect();
        out.push((
            "FLEET_PROVIDERS".into(),
            serde_json::to_string(&inventory).unwrap_or_else(|_| "[]".into()),
        ));
        out
    }

    /// Block rules in both directions, for `fleet providers check`.
    pub fn audit(&self) -> Vec<serde_json::Value> {
        self.all()
            .into_iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "base_url": p.base_url,
                    "key_env": p.key_env,
                    "available": p.available(),
                    "blocked_for": p.block,
                    "models": p.models,
                    "enabled": p.enabled,
                })
            })
            .collect()
    }
}

/// Providers inferred from the environment on first run, so a machine that
/// already has a key working does not have to be told again.
pub fn detect_from_env() -> Vec<Provider> {
    let candidates: [(&str, &str, &str); 8] = [
        (
            "tokenbroker",
            "https://api.thetokenbroker.ai/v1",
            "TB_API_KEY",
        ),
        ("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
        (
            "anthropic",
            "https://api.anthropic.com/v1",
            "ANTHROPIC_API_KEY",
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
        ),
        (
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
        ),
        ("xai", "https://api.x.ai/v1", "XAI_API_KEY"),
        ("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
        (
            "together",
            "https://api.together.xyz/v1",
            "TOGETHER_API_KEY",
        ),
    ];
    candidates
        .iter()
        .filter(|(_, _, env)| std::env::var(env).map(|v| !v.is_empty()).unwrap_or(false))
        .map(|(name, url, env)| Provider {
            name: name.to_string(),
            base_url: url.to_string(),
            key_env: env.to_string(),
            extra: Default::default(),
            models: vec!["*".into()],
            block: Vec::new(),
            enabled: true,
            notes: "detected from the environment at onboarding".into(),
        })
        .collect()
}
