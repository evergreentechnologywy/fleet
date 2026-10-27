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
    ///
    /// Kept as the simple case: a provider with `key_env` and no
    /// `[provider.credential]` behaves exactly as it always did.
    #[serde(default)]
    pub key_env: String,
    /// Where the credential actually comes from. Absent means `key_env`, which
    /// keeps every existing descriptor valid.
    #[serde(default)]
    pub credential: Option<crate::credential::Source>,
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

    /// The credential source, defaulting to the env var so an existing
    /// descriptor keeps working untouched.
    pub fn source(&self) -> crate::credential::Source {
        match &self.credential {
            Some(s) => s.clone(),
            None => crate::credential::Source::Env {
                var: self.key_env.clone(),
            },
        }
    }

    /// Resolve the key. Returns `None` when the source is legitimately empty,
    /// which is a reported condition, not an error: a provider with no key is
    /// simply unavailable, like a harness with no binary.
    ///
    /// `force` asks the source again rather than reusing a cached value. That is
    /// what makes a rejected OAuth token recoverable — the whole point of a
    /// broker is that a token can be refreshed.
    pub fn key_with(&self, creds: &crate::credential::Credentials, force: bool) -> Result<Option<String>> {
        creds.resolve(&self.name, &self.source(), force)
    }

    /// The simple form, for listings and probes.
    pub fn key(&self) -> Option<String> {
        crate::credential::Credentials::new()
            .resolve(&self.name, &self.source(), false)
            .ok()
            .flatten()
    }

    /// Does this provider have a credential that resolves at all? An OAuth
    /// provider counts when its broker answers; a key provider counts when the
    /// variable is set and non-empty.
    pub fn has_credential(&self) -> bool {
        match self.key_with(&crate::credential::Credentials::new(), false) {
            Ok(Some(k)) => !k.is_empty(),
            _ => false,
        }
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
        self.env_with_key(&key)
    }

    /// The same environment, for a key already resolved (possibly forced).
    pub fn env_with_key(&self, key: &str) -> Vec<(String, String)> {
        let key = key.to_string();
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
    ///
    /// A provider is only a candidate when it has a USABLE CREDENTIAL, because a
    /// provider whose key is unset or rejected produces a harness that cannot
    /// authenticate — and the failure surfaces inside the CLI as "missing API
    /// key", which reads as a fleet bug rather than a provider problem. Measured
    /// 2026-10-27: qwen picked a `dashscope` provider whose key returns 401, and
    /// every qwen call failed with "Missing API key for OpenAI-compatible auth".
    ///
    /// A provider with no credential at all is only used as a last resort, so a
    /// fleet with nothing configured still behaves as it did.
    pub fn pick(&self, harness: &str, denies: &[String], model: &str) -> Option<Provider> {
        let allowed = self.for_harness(harness, denies);
        let keyed: Vec<&Provider> = allowed.iter().filter(|p| p.has_credential()).collect();
        let pool: Vec<&Provider> = if keyed.is_empty() {
            allowed.iter().collect()
        } else {
            keyed
        };
        pool.iter()
            .find(|p| !model.is_empty() && p.accepts(model) && p.available())
            .or_else(|| pool.iter().find(|p| p.available()))
            .or_else(|| pool.first())
            .map(|p| (*p).clone())
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
    ///
    /// `force` re-resolves dynamic sources (an OAuth broker, a vault command),
    /// which is how a rotated token reaches a harness after a rejection.
    pub fn env_for_forced(
        &self,
        harness: &str,
        denies: &[String],
        creds: &crate::credential::Credentials,
        force: bool,
    ) -> Vec<(String, String)> {
        let allowed = self.for_harness(harness, denies);
        let mut out = Vec::new();
        for p in &allowed {
            match p.key_with(creds, force) {
                Ok(Some(k)) => out.extend(p.env_with_key(&k)),
                // A source that fails is reported once, here, rather than
                // silently producing a harness that cannot authenticate.
                Ok(None) => {}
                Err(e) => eprintln!("fleet: provider `{}` credential: {e}", p.name),
            }
        }
        // The primary provider wins any conflict by being applied last, which
        // matters because each provider exports the generic OPENAI_* names.
        if let Some(primary) = allowed.last() {
            if let Ok(Some(k)) = primary.key_with(creds, force) {
                out.extend(primary.env_with_key(&k));
            }
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

    /// Convenience for a caller with no shared resolver.
    pub fn env_for(&self, harness: &str, denies: &[String]) -> Vec<(String, String)> {
        self.env_for_forced(harness, denies, &crate::credential::Credentials::new(), false)
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
            credential: None,
            extra: Default::default(),
            models: vec!["*".into()],
            block: Vec::new(),
            enabled: true,
            notes: "detected from the environment at onboarding".into(),
        })
        .collect()
}
