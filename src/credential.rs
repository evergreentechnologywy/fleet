//! Credentials: where a key comes from, including OAuth.
//!
//! A provider used to hold one thing: the *name* of an env var. That is the
//! simplest case and stays the default, but it is not the only one in a real
//! fleet:
//!
//! * an **OAuth** provider (Anthropic, Copilot, Gemini CLI, an internal SSO
//!   gateway) has no long-lived key at all — it has an access token that expires
//!   and a refresh token that rotates;
//! * a key may live in a **vault** and be fetched by a command;
//! * a key may be obtained from the harness tooling itself, which is exactly
//!   what `omp token <provider>` does.
//!
//! So a provider names a **source**, not just a variable:
//!
//! ```toml
//! [[provider]]
//! name = "muse"
//! base_url = "https://api.muse.example/v1"
//! [provider.credential]
//! kind = "oauth"
//! broker = "omp"          # any command that prints a token
//! provider = "muse-code"  # the broker's own id for it
//! ```
//!
//! Three properties matter, and each is a bug avoided:
//!
//! 1. **The token is never written to disk.** A resolved value lives in the
//!    process and is injected into a child's environment. A descriptor names a
//!    source, so every file stays publishable.
//! 2. **Resolution is cached until it is nearly stale**, because a broker call
//!    is a subprocess. An expired or rejected token is re-resolved on demand —
//!    which is what makes a rotated OAuth token work without a restart.
//! 3. **A broker that fails is reported as such**, not as "no credential":
//!    "the broker returned nothing" and "the provider is logged out" send an
//!    operator to two different places.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use parking_lot::Mutex;
use std::time::{Duration, Instant};

/// Where a provider's credential comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Source {
    /// An environment variable. The default: simplest, no subprocess.
    Env {
        #[serde(default)]
        var: String,
    },
    /// Run a command and read the token from its stdout. Covers a vault CLI, a
    /// secret manager, and `omp token <provider>`.
    Command {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment for the command.
        #[serde(default)]
        env: HashMap<String, String>,
        /// How long a resolved value is reused. Short by default, because a
        /// command may return a rotating token.
        #[serde(default)]
        ttl_s: Option<u64>,
        /// For a nested credential payload, the field holding the token. For an
        /// OAuth JSON body this is `oauthAccessToken`; for a plain key the whole
        /// stdout is used.
        #[serde(default)]
        json_field: Option<String>,
    },
    /// An OAuth login managed by a broker. Resolved through the broker's own
    /// token command, and re-resolved when the token is rejected.
    Oauth {
        /// `omp` by default: the fleet's own credential vault.
        #[serde(default = "default_broker")]
        broker: String,
        /// Extra argv before the provider id. A broker is not always a bare
        /// binary: `python3 /opt/vault/broker.py` needs two arguments, and
        /// without this the provider id would land where the script should.
        #[serde(default)]
        args: Vec<String>,
        /// The broker's id for this provider, e.g. `muse-code`, `anthropic`.
        provider: String,
        /// Which OAuth account when several are stored (1-based).
        #[serde(default)]
        account: Option<u32>,
        /// Ask the broker to refresh rather than returning a cached token.
        #[serde(default)]
        force_refresh: bool,
        /// How long a resolved token is reused before it is re-read.
        #[serde(default)]
        ttl_s: Option<u64>,
        /// Field holding the token inside a nested credential JSON body.
        #[serde(default = "default_oauth_field")]
        json_field: String,
    },
    /// Nothing: the harness authenticates itself.
    None,
}

fn default_broker() -> String {
    "omp".to_string()
}

fn default_oauth_field() -> String {
    "oauthAccessToken".to_string()
}

impl Default for Source {
    fn default() -> Self {
        Source::Env {
            var: String::new(),
        }
    }
}

impl Source {
    /// A short human label for listings. Never includes a value.
    pub fn label(&self) -> String {
        match self {
            Source::Env { var } if var.is_empty() => "env:(unset)".into(),
            Source::Env { var } => format!("env:{var}"),
            Source::Command { command, .. } => format!("command:{command}"),
            Source::Oauth { broker, provider, .. } => format!("oauth:{broker}/{provider}"),
            Source::None => "none".into(),
        }
    }

    /// Does this source need a subprocess to resolve?
    pub fn is_dynamic(&self) -> bool {
        matches!(self, Source::Command { .. } | Source::Oauth { .. })
    }
}

struct CacheEntry {
    value: String,
    at: Instant,
}

/// Resolves credentials, with a short-lived cache and a forced re-read.
pub struct Credentials {
    cache: Mutex<HashMap<String, CacheEntry>>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self::new()
    }
}

impl Credentials {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve a source to a value, or `None` when it is legitimately empty.
    ///
    /// `force` bypasses the cache: used after a call is rejected, because a
    /// rotating OAuth token is *the* case where a cached value is wrong and the
    /// only correct response is to ask the broker again.
    pub fn resolve(&self, _key: &str, src: &Source, force: bool) -> Result<Option<String>> {
        match src {
            Source::None => Ok(None),
            Source::Env { var } => {
                if var.is_empty() {
                    return Ok(None);
                }
                Ok(std::env::var(var).ok().filter(|v| !v.is_empty()))
            }
            Source::Command {
                command,
                args,
                env,
                ttl_s,
                json_field,
            } => {
                let ttl = Duration::from_secs(ttl_s.unwrap_or(60));
                let cache_key = format!("cmd:{command}:{}", args.join(" "));
                self.cached(&cache_key, ttl, force, || {
                    let pairs: Vec<(String, String)> = env
                        .iter()
                        .map(|(k, v)| (k.clone(), crate::credential::expand(v)))
                        .collect();
                    let out = crate::runner::run_capture(
                        command,
                        args,
                        &pairs,
                        None,
                        Duration::from_secs(45),
                    )
                    .with_context(|| format!("running credential command `{command}`"))?;
                    if !out.ok {
                        bail!(
                            "credential command `{command}` exited {}: {}",
                            out.code,
                            crate::runner::truncate(out.stderr.trim(), 300)
                        );
                    }
                    let text = out.stdout.trim().to_string();
                    if text.is_empty() {
                        bail!("credential command `{command}` printed nothing");
                    }
                    Ok(extract(&text, json_field.as_deref()))
                })
            }
            Source::Oauth {
                broker,
                args,
                provider,
                account,
                force_refresh,
                ttl_s,
                json_field,
            } => {
                let ttl = Duration::from_secs(ttl_s.unwrap_or(300));
                let cache_key = format!("oauth:{broker}:{provider}:{account:?}");
                let args = broker_args(broker, args, provider, *account, *force_refresh);
                self.cached(&cache_key, ttl, force || *force_refresh, || {
                    let out = crate::runner::run_capture(
                        broker,
                        &args,
                        &[],
                        None,
                        Duration::from_secs(45),
                    )
                    .with_context(|| {
                        format!(
                            "the OAuth broker `{broker}` is not runnable; install it or point \
                             the provider at a different credential source"
                        )
                    })?;
                    if !out.ok {
                        let err = out.stderr.trim();
                        // The broker distinguishes "you are not logged in" from
                        // its own failure, and so should this message.
                        bail!(
                            "OAuth broker `{broker}` could not produce a token for `{provider}` \
                             (exit {}): {}. Sign in with `{broker} login` if that is the cause.",
                            out.code,
                            crate::runner::truncate(err, 300)
                        );
                    }
                    let text = out.stdout.trim();
                    if text.is_empty() {
                        bail!("OAuth broker `{broker}` returned an empty token for `{provider}`");
                    }
                    Ok(extract(text, Some(json_field)))
                })
            }
        }
    }

    /// Cache a resolved value for `ttl`, unless `force` demands a fresh one.
    fn cached(
        &self,
        key: &str,
        ttl: Duration,
        force: bool,
        produce: impl FnOnce() -> Result<Option<String>>,
    ) -> Result<Option<String>> {
        if !force {
            let c = self.cache.lock();
            if let Some(e) = c.get(key) {
                if e.at.elapsed() < ttl {
                    return Ok(Some(e.value.clone()));
                }
            }
        }
        let value = produce()?;
        if let Some(v) = &value {
            let mut c = self.cache.lock();
            if c.len() > 64 {
                c.clear();
            }
            c.insert(
                key.to_string(),
                CacheEntry {
                    value: v.clone(),
                    at: Instant::now(),
                },
            );
        }
        Ok(value)
    }

    /// Forget a cached value, so the next resolve asks the source again.
    pub fn invalidate(&self, src: &Source) {
        let prefix = match src {
            Source::Command { command, args, .. } => format!("cmd:{command}:{}", args.join(" ")),
            Source::Oauth {
                broker,
                provider,
                account,
                ..
            } => format!("oauth:{broker}:{provider}:{account:?}"),
            _ => return,
        };
        self.cache.lock().retain(|k, _| k != &prefix);
    }
}

/// The argv a broker needs to print one provider's token.
///
/// Kept in one place because the fleet has exactly one broker convention and a
/// second, slightly different spelling of it is how a working integration breaks.
fn broker_args(
    broker: &str,
    extra: &[String],
    provider: &str,
    account: Option<u32>,
    force_refresh: bool,
) -> Vec<String> {
    let mut a: Vec<String> = extra.to_vec();
    // `omp` is a subcommand CLI; a bare `token` would be read as a program name
    // by anything else, so the subcommand is included only for a known broker.
    if broker.ends_with("omp") || broker == "omp" {
        a.push("token".into());
    }
    a.push(provider.to_string());
    if force_refresh {
        a.push("--force-refresh".into());
    }
    if let Some(n) = account {
        a.push("--account".into());
        a.push(n.to_string());
    }
    a
}

/// Pull the token out of whatever the source printed.
///
/// An OAuth broker prints a nested JSON credential
/// (`{"oauthAccessToken":"…"}`); an API-key broker prints the key itself. Both
/// are handled, because both occur and telling them apart by the provider's name
/// would be a guess.
pub fn extract(text: &str, field: Option<&str>) -> Option<String> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    // A JSON body is ALWAYS unwrapped, whether or not a field was named. A
    // broker that prints `{"oauthAccessToken":"…"}` must never hand a harness the
    // literal JSON as its bearer token: every provider rejects that with a 401
    // that reads as an expired login.
    if t.starts_with('{') {
        if let Some(f) = field {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
                for key in [f, "oauthAccessToken", "access_token", "token", "apiKey", "key"] {
                    if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                        if !s.is_empty() {
                            return Some(s.to_string());
                        }
                    }
                }
                // A nested credential object: `{"credential":{"access":"{…}"}}`
                // appears when a broker exports a whole login rather than a token.
                if let Some(inner) = v
                    .get("credential")
                    .and_then(|c| c.get("access"))
                    .and_then(|a| a.as_str())
                {
                    if let Ok(iv) = serde_json::from_str::<serde_json::Value>(inner) {
                        for key in [f, "oauthAccessToken", "access_token", "token"] {
                            if let Some(s) = iv.get(key).and_then(|x| x.as_str()) {
                                if !s.is_empty() {
                                    return Some(s.to_string());
                                }
                            }
                        }
                    }
                    if !inner.is_empty() {
                        return Some(inner.to_string());
                    }
                }
            }
        }
        // No field named, or the named one was absent: the well-known keys still
        // identify a token.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
            for key in ["oauthAccessToken", "access_token", "token", "apiKey", "key"] {
                if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
            if let Some(inner) = v
                .get("credential")
                .and_then(|c| c.get("access"))
                .and_then(|a| a.as_str())
            {
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
        }
    }
    // Not JSON, or no field asked for: the whole output is the credential.
    // A single trailing newline is already trimmed; anything else is real.
    Some(t.to_string())
}

/// Expand `env:NAME` inside a descriptor value, so a file never holds a secret.
pub fn expand(v: &str) -> String {
    match v.strip_prefix("env:") {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_key_is_taken_whole() {
        assert_eq!(extract("sk-abc123\n", None).unwrap(), "sk-abc123");
        assert_eq!(extract("  sk-abc123  ", None).unwrap(), "sk-abc123");
        assert!(extract("   ", None).is_none());
    }

    #[test]
    fn an_oauth_body_yields_the_token_not_the_json() {
        // The bug this prevents: handing a harness the literal string
        // `{"oauthAccessToken":"…"}` as a bearer token, which every provider
        // rejects with a 401 that looks like an expired login.
        let body = r#"{"oauthAccessToken":"dca:abc.def","other":"x"}"#;
        assert_eq!(extract(body, Some("oauthAccessToken")).unwrap(), "dca:abc.def");
        // Field name omitted: the well-known fallbacks still find it.
        assert_eq!(extract(body, None).unwrap(), "dca:abc.def");
        assert_eq!(extract(body, Some("wrongField")).unwrap(), "dca:abc.def");
    }

    #[test]
    fn a_nested_exported_login_still_yields_a_token() {
        let body = r#"{"credential":{"type":"oauth","access":"{\"oauthAccessToken\":\"tok-1\"}"}}"#;
        assert_eq!(extract(body, Some("oauthAccessToken")).unwrap(), "tok-1");
    }

    #[test]
    fn env_expansion_reads_the_environment() {
        std::env::set_var("CRED_TEST_VALUE", "from-env");
        assert_eq!(expand("env:CRED_TEST_VALUE"), "from-env");
        assert_eq!(expand("literal"), "literal");
        assert_eq!(expand("env:DEFINITELY_UNSET_VAR"), "");
    }

    #[test]
    fn an_env_source_resolves_and_reports_absence() {
        let c = Credentials::new();
        std::env::set_var("CRED_SRC_TEST", "v1");
        let s = Source::Env {
            var: "CRED_SRC_TEST".into(),
        };
        assert_eq!(c.resolve("k", &s, false).unwrap().as_deref(), Some("v1"));
        let missing = Source::Env {
            var: "CRED_SRC_MISSING".into(),
        };
        assert!(c.resolve("k", &missing, false).unwrap().is_none());
        let none = Source::None;
        assert!(c.resolve("k", &none, false).unwrap().is_none());
    }

    #[test]
    fn a_command_source_is_cached_and_can_be_forced() {
        // A broker call is a subprocess, so the cache matters; but a rotating
        // token means the cache must be bypassable.
        let c = Credentials::new();
        let counter = std::env::temp_dir().join(format!("cred-count-{}", std::process::id()));
        let _ = std::fs::remove_file(&counter);
        let script = format!(
            "echo x >> {}; echo tok-1",
            counter.display()
        );
        let s = Source::Command {
            command: "sh".into(),
            args: vec!["-c".into(), script],
            env: Default::default(),
            ttl_s: Some(300),
            json_field: None,
        };
        assert_eq!(c.resolve("k", &s, false).unwrap().as_deref(), Some("tok-1"));
        assert_eq!(c.resolve("k", &s, false).unwrap().as_deref(), Some("tok-1"));
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 1, "the second resolve was served from the cache");

        // Forcing is what makes a rejected token recoverable.
        c.resolve("k", &s, true).unwrap();
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 2, "a forced resolve asks the source again");

        // Invalidating does the same.
        c.invalidate(&s);
        c.resolve("k", &s, false).unwrap();
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 3);
    }

    #[test]
    fn a_failing_command_says_so_rather_than_looking_unset() {
        let c = Credentials::new();
        let s = Source::Command {
            command: "sh".into(),
            args: vec!["-c".into(), "echo 'not logged in' >&2; exit 4".into()],
            env: Default::default(),
            ttl_s: None,
            json_field: None,
        };
        let err = c.resolve("k", &s, false).unwrap_err().to_string();
        assert!(err.contains("exited 4"), "the exit code is named: {err}");
        assert!(
            err.contains("not logged in"),
            "the broker's own message is kept: {err}"
        );

        // A command that prints nothing is a distinct failure from one that
        // prints an error, and both are distinct from "unset".
        let empty = Source::Command {
            command: "sh".into(),
            args: vec!["-c".into(), "true".into()],
            env: Default::default(),
            ttl_s: None,
            json_field: None,
        };
        let err = c.resolve("k", &empty, false).unwrap_err().to_string();
        assert!(err.contains("printed nothing"), "{err}");
    }

    #[test]
    fn the_broker_argv_has_exactly_one_spelling() {
        assert_eq!(
            broker_args("omp", &[], "muse-code", None, false),
            vec!["token", "muse-code"]
        );
        assert_eq!(
            broker_args("omp", &[], "anthropic", Some(2), true),
            vec!["token", "anthropic", "--force-refresh", "--account", "2"]
        );
        // A broker that is not omp is invoked plainly, with the provider id.
        assert_eq!(
            broker_args("/usr/local/bin/my-vault", &[], "openai", None, false),
            vec!["openai"]
        );
        // A broker with its own arguments keeps them BEFORE the provider id.
        assert_eq!(
            broker_args(
                "python3",
                &["/opt/vault/broker.py".to_string()],
                "openai",
                None,
                false
            ),
            vec!["/opt/vault/broker.py", "openai"]
        );
    }

    #[test]
    fn a_label_never_contains_a_value() {
        let s = Source::Oauth {
            broker: "omp".into(),
            args: vec![],
            provider: "muse-code".into(),
            account: None,
            force_refresh: false,
            ttl_s: None,
            json_field: "oauthAccessToken".into(),
        };
        let l = s.label();
        assert!(l.contains("muse-code"));
        assert!(!l.contains("dca:"));
        assert!(s.is_dynamic());
    }
}
