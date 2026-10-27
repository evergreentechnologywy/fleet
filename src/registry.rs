//! The harness registry — harnesses are FILES, not code.
//!
//! One harness is one `*.toml` file in the harness directory. The directory is
//! re-scanned on every read and the parse is cached by `(path, mtime, len)`.
//! That is the whole add/delete story:
//!
//! * add     — write `foo.toml` into the directory. Next call sees it.
//! * delete  — remove the file. Next call stops seeing it.
//! * edit    — change the file. Next call uses the new definition.
//!
//! No restart, no registration call, no daemon state, no recompile. A harness
//! that is mid-flight is unaffected: the run holds the definition it started
//! with as an owned value, so deleting a file never pulls a harness out from
//! under a running task.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use parking_lot::Mutex;
use std::time::SystemTime;

/// How a harness is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A process run once per task; the prompt arrives on argv or stdin.
    /// These are the coding CLIs: `omp -p`, `codex exec`, `opencode run`.
    Cli,
    /// A local HTTP server that is already listening and is started/stopped by
    /// whoever owns it (`fleet harness start|stop` manages the lifecycle).
    /// The `[http]` table says how to talk to it.
    Http,
    /// Any OpenAI-compatible chat-completions endpoint. Harnesses of this kind
    /// ARE a model — they are how `fleet` reaches a provider directly, with no
    /// CLI in between.
    OpenaiCompat,
}

/// How the model is chosen for this harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Route {
    /// `fleet` asks the chooser (Jev) for model + effort and injects the answer
    /// on the command line / in the body. One decision per call.
    Jev,
    /// The harness does its own choosing: it is pointed at the chooser's
    /// base URL and behaves like any other OpenAI client. `fleet` still records
    /// which harness ran, but the model id lives inside the harness.
    SelfRouting,
    /// No model abstraction: the harness runs with its own auth and defaults.
    /// `fleet` passes no model flags.
    Native,
    /// Pin one model, no chooser call. Cheapest possible path.
    Fixed,
}

/// The `[http]` table.
///
/// `api_key_env` and `api_key` are accepted here as well as at the top level,
/// because TOML scoping makes that placement the natural mistake: a key written
/// after an `[http]` header belongs to `[http]`, not to the harness. Accepting
/// both is strictly more forgiving than rejecting the common one, and the
/// top-level field still wins when both are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSpec {
    /// Where `fleet` reaches a harness it did not spawn.
    pub base_url: String,
    /// Path appended to `base_url`. Defaults to `/v1/chat/completions`.
    #[serde(default = "default_chat_path")]
    pub chat_path: String,
    /// Header carrying the key, e.g. `Authorization`.
    #[serde(default)]
    pub auth_header: Option<String>,
    /// Bearer scheme for `auth_header`.
    #[serde(default)]
    pub auth_scheme: Option<String>,
    /// Same as `Harness::api_key_env`; accepted here for convenience.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Same as `Harness::api_key`; accepted here for convenience.
    #[serde(default)]
    pub api_key: Option<String>,
}

fn default_chat_path() -> String {
    "/v1/chat/completions".to_string()
}

/// The harness descriptor. Everything here is data, so the reach of `fleet`
/// grows by editing files instead of shipping code.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    /// Stable id; must equal the file stem so `fleet` can point at one file.
    pub name: String,
    /// One line shown by `fleet harness list` and used by the planner.
    #[serde(default)]
    pub description: String,
    pub kind: Kind,

    // ---- cli ----
    /// Executable. Looked up on `PATH` unless absolute.
    #[serde(default)]
    pub command: Option<String>,
    /// Argument vector. Placeholders are substituted per run.
    #[serde(default)]
    pub args: Vec<String>,
    /// Deliver the prompt on stdin instead of `{prompt}`.
    #[serde(default)]
    pub prompt_on_stdin: bool,
    /// Named placeholders, filled by the router (`--model`, `--variant`, ...).
    /// `model_flag` is a two-element list: flag then value.
    #[serde(default)]
    pub model_flag: Vec<String>,
    #[serde(default)]
    pub effort_flag: Vec<String>,

    // ---- http / openai-compat ----
    #[serde(default)]
    pub http: Option<HttpSpec>,
    /// Credential source. An env var NAME, never a literal secret, so a
    /// descriptor stays safe to commit and to publish.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Literal key. Only for a local engine that needs no credential.
    #[serde(default)]
    pub api_key: Option<String>,

    // ---- routing ----
    #[serde(default = "default_route")]
    pub route: Route,
    /// Model used when `route = "fixed"`, or when a chooser call fails.
    #[serde(default)]
    pub model: Option<String>,
    /// Ceiling for the chooser: ids this harness can actually be served.
    /// Empty means no local restriction.
    #[serde(default)]
    pub models: Vec<String>,
    /// Reasoning effort to ask for when the chooser has no opinion.
    #[serde(default)]
    pub effort: Option<String>,

    // ---- selection ----
    /// Free-form capability tags the planner matches against: `code`, `long`,
    /// `vision`, `verify`, `chat`, `fast`, `cheap`, `search`.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Higher wins when several harnesses fit equally well.
    #[serde(default)]
    pub priority: i32,
    /// Estimated cost in USD per call, used as the tie-break after `priority`.
    #[serde(default)]
    pub cost_hint: f64,
    /// A harness that only reports on other harnesses' work.
    #[serde(default)]
    pub verifier: bool,

    // ---- lifecycle ----
    /// Explicit off switch that keeps the file in place. A harness you wrote is
    /// meant to run, so the default is on: writing a file and finding it
    /// silently inert is the worst possible first experience.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// GPS coordinates, for realism in the reasoning trace. Optional.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Extra environment for the child process.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Working directory, `~` allowed. Defaults to a per-run scratch dir.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Providers this harness must never be offered, by name. The other
    /// direction of the same rule lives on the provider as `block`.
    #[serde(default)]
    pub deny_providers: Vec<String>,
    /// Set false to keep this harness out of the plugin path entirely.
    #[serde(default = "default_enabled")]
    pub plugins: bool,

    /// How many subagents this harness may spawn, and what they may be.
    ///
    /// A subagent is the SAME engine run, launched by the harness itself through
    /// `fleet run` (the path is in `$FLEET_MEMORY_TOOL`). That matters: a
    /// subagent inherits the chooser, the provider rules, the plugin path, and
    /// shared memory, so delegating cannot escape fleet policy the way a
    /// harness's own private sub-agent mechanism can.
    ///
    /// `0` means no delegation, which is the default because a harness that
    /// spawns without a budget is how one task becomes twenty.
    #[serde(default)]
    pub subagents: usize,
    /// Harnesses a subagent may be routed to. Empty means "any enabled one".
    #[serde(default)]
    pub subagent_harnesses: Vec<String>,
    /// Hard ceiling on subagent depth, so delegation cannot recurse forever.
    #[serde(default = "default_depth")]
    pub subagent_depth: usize,

    /// Provenance: which file this came from. Filled by the registry.
    #[serde(skip)]
    pub source: Option<PathBuf>,
}

fn default_route() -> Route {
    Route::Jev
}

fn default_enabled() -> bool {
    true
}

/// Delegation defaults to one level: enough for a real split, not enough to
/// build a tree nobody can account for.
fn default_depth() -> usize {
    1
}

impl Harness {
    pub fn timeout(&self) -> u64 {
        self.timeout_s
            .unwrap_or(if self.verifier { 600 } else { 1800 })
    }

    /// The model this harness is pinned to, if any. An empty string is treated
    /// as "not pinned" so a descriptor can carry an empty default.
    pub fn effective_model(&self) -> Option<&str> {
        self.model.as_deref().filter(|m| !m.is_empty())
    }
    /// Resolve the credential for this harness, wherever it was written.
    ///
    /// The top-level field wins; the `[http]` table is the fallback, because a
    /// key written after an `[http]` header belongs to that table in TOML, and
    /// that mistake is worth forgiving rather than silently dropping — a
    /// dropped key reads as "this provider is logged out", not as a typo.
    pub fn key_env(&self) -> Option<&str> {
        self.api_key_env
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.http
                    .as_ref()
                    .and_then(|h| h.api_key_env.as_deref())
                    .filter(|s| !s.is_empty())
            })
    }

    /// The literal key, if one was written either place.
    pub fn literal_key(&self) -> Option<&str> {
        self.api_key
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.http
                    .as_ref()
                    .and_then(|h| h.api_key.as_deref())
                    .filter(|s| !s.is_empty())
            })
    }

    /// Does this harness accept `model`? Empty `models` means yes.
    pub fn accepts(&self, model: &str) -> bool {
        self.models.is_empty() || self.models.iter().any(|m| m == model || m == "*")
    }

    /// Validate a descriptor. Called on load so a bad file is reported against
    /// its path instead of failing later inside a run.
    pub fn validate(&self, path: &Path) -> Result<()> {
        if self.name.is_empty() {
            bail!("{}: `name` is required", path.display());
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            if stem != self.name {
                bail!(
                    "{}: `name = \"{}\"` must match the file stem `{}` — the file IS the harness",
                    path.display(),
                    self.name,
                    stem
                );
            }
        }
        match self.kind {
            Kind::Cli => {
                if self.command.as_deref().unwrap_or("").is_empty() {
                    bail!("{}: kind = \"cli\" needs `command`", path.display());
                }
            }
            Kind::Http | Kind::OpenaiCompat => {
                let have_url = self
                    .http
                    .as_ref()
                    .map(|h| !h.base_url.is_empty())
                    .unwrap_or(false);
                if !have_url {
                    bail!(
                        "{}: kind = \"{:?}\" needs `[http] base_url`",
                        path.display(),
                        self.kind
                    );
                }
            }
        }
        if self.route == Route::Fixed && self.effective_model().is_none() {
            bail!("{}: route = \"fixed\" needs `model`", path.display());
        }
        Ok(())
    }

    /// Build the argv for one call, substituting placeholders.
    pub fn argv(
        &self,
        prompt: &str,
        model: Option<&str>,
        effort: Option<&str>,
        cwd: &Path,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::with_capacity(self.args.len() + 4);
        let model = model.unwrap_or("");
        let effort = effort.unwrap_or("");
        for a in &self.args {
            out.push(
                a.replace("{prompt}", prompt)
                    .replace("{model}", model)
                    .replace("{effort}", effort)
                    .replace("{cwd}", &cwd.to_string_lossy()),
            );
        }
        // Model/effort flags are appended only when we have a value, so a
        // harness that takes neither is expressed with an empty flag list.
        if !model.is_empty() && !self.model_flag.is_empty() {
            push_flag(&mut out, &self.model_flag, model);
        }
        if !effort.is_empty() {
            push_flag(&mut out, &self.effort_flag, effort);
        }
        out
    }
}

/// Is this ceiling entry a concrete MODEL id, rather than a router alias or a
/// wildcard?
///
/// The distinction is not cosmetic. A harness that reaches the chooser's OpenAI
/// door declares the door's alias (`jev/auto`), because the door resolves the
/// authorized model itself; a harness that talks to a provider declares real ids.
/// Treating the two the same made the router "clamp" every decision an aliased
/// harness received — 492 times in one ledger — silently replacing the chooser's
/// pick and recording a reason that read like a normal route.
///
/// `*` and empty are not fields either: neither constrains anything, which is
/// exactly what the doctor reports as `capability.unclamped`.
pub fn is_model_field(m: &str) -> bool {
    let m = m.trim();
    !(m.is_empty() || m == "*" || m == "auto" || m == "jev/auto")
}

/// Append a model/effort flag, in whichever of the three shapes the descriptor
/// used. Getting this wrong is silent: a bare `-m` with no value is accepted by
/// the argument parser and only fails inside the child process, as a message
/// about the flag rather than about the harness.
///
/// * `["-m", "{model}"]` — flag then value, the common case;
/// * `["--model={model}"]` — one token carrying the value;
/// * `["--model", "auto"]`  — a fixed token pair, when the value is not ours to
///   choose (a harness that only accepts one of several spellings).
fn push_flag(out: &mut Vec<String>, spec: &[String], value: &str) {
    match spec {
        [] => {}
        [one] => out.push(one.replace("{model}", value).replace("{effort}", value)),
        [flag, val, ..] => {
            out.push(flag.clone());
            out.push(val.replace("{model}", value).replace("{effort}", value));
        }
    }
}

/// Cache entry: the parsed harness plus the file fingerprint it came from.
struct Cached {
    harness: Harness,
    mtime: Option<SystemTime>,
    len: u64,
}

/// A live view over the harness directory. Cheap to construct; holds only the
/// parse cache, which is keyed by file fingerprint.
pub struct Registry {
    dir: PathBuf,
    cache: Mutex<HashMap<PathBuf, Cached>>,
}

impl Registry {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn default_dir() -> Self {
        Self::new(crate::paths::harness_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every valid harness currently on disk, sorted by name. Invalid files are
    /// returned in `errors` rather than aborting the load: one broken harness
    /// must not take the engine down.
    pub fn all(&self) -> (Vec<Harness>, Vec<(PathBuf, String)>) {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) => {
                errors.push((
                    self.dir.clone(),
                    format!("harness directory unreadable: {e}"),
                ));
                return (out, errors);
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("toml")
                    && !p
                        .file_name()
                        .and_then(|f| f.to_str())
                        .map(|f| f.starts_with('.'))
                        .unwrap_or(false)
            })
            .collect();
        paths.sort();
        for path in paths {
            match self.load_file(&path) {
                Ok(h) => out.push(h),
                Err(e) => errors.push((path, e.to_string())),
            }
        }
        (out, errors)
    }

    /// Only enabled harnesses — the set the planner may choose from.
    pub fn enabled(&self) -> Vec<Harness> {
        self.all().0.into_iter().filter(|h| h.enabled).collect()
    }

    /// Look one up by name.
    pub fn get(&self, name: &str) -> Result<Harness> {
        let path = self.dir.join(format!("{name}.toml"));
        if !path.exists() {
            let known: Vec<String> = self.all().0.into_iter().map(|h| h.name).collect();
            return Err(anyhow!(
                "no harness named `{name}` in {}. known: {}",
                self.dir.display(),
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
        self.load_file(&path)
    }

    /// Parse one descriptor, reusing the cache when the fingerprint matches.
    fn load_file(&self, path: &Path) -> Result<Harness> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let mtime = meta.modified().ok();
        let len = meta.len();
        {
            let cache = self.cache.lock();
            if let Some(c) = cache.get(path) {
                if c.mtime == mtime && c.len == len {
                    return Ok(c.harness.clone());
                }
            }
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        // The serde detail is included verbatim: without it, "parsing failed"
        // is indistinguishable from a dozen different mistakes, and the most
        // common one — a key written after a `[table]` header, so TOML files it
        // under that table — is only visible in that detail.
        let mut h: Harness =
            toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        h.source = Some(path.to_path_buf());
        h.validate(path)?;
        let mut cache = self.cache.lock();
        cache.insert(
            path.to_path_buf(),
            Cached {
                harness: h.clone(),
                mtime,
                len,
            },
        );
        // Drop cache entries for files that no longer exist so a delete is
        // truly seamless and memory does not grow with churn.
        cache.retain(|p, _| p.exists());
        Ok(h)
    }

    /// Copy a descriptor into the directory in one atomic step. This is the
    /// `fleet harness add` path: a half-written file is never visible, because
    /// it lands under a temporary name and is renamed into place.
    pub fn install(&self, source: &Path) -> Result<Harness> {
        std::fs::create_dir_all(&self.dir)?;
        let text = std::fs::read_to_string(source)
            .with_context(|| format!("reading {}", source.display()))?;
        let h: Harness =
            toml::from_str(&text).with_context(|| format!("parsing {}", source.display()))?;
        if h.name.is_empty() {
            bail!("{}: `name` is required", source.display());
        }
        let dest = self.dir.join(format!("{}.toml", h.name));
        // Validate against the DESTINATION filename: the file stem is part of
        // the contract, and the temporary `.name.tmp` would fail that check.
        h.validate(&dest)?;
        let tmp = self.dir.join(format!(".{}.tmp", h.name));
        std::fs::write(&tmp, &text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &dest).with_context(|| format!("installing {}", dest.display()))?;
        self.cache.lock().remove(&dest);
        self.get(&h.name)
    }

    /// Remove a harness. Refuses to guess: a name that matches nothing is an
    /// error, so a typo cannot silently look like a successful delete.
    pub fn remove(&self, name: &str) -> Result<PathBuf> {
        let path = self.dir.join(format!("{name}.toml"));
        if !path.exists() {
            bail!("no harness named `{name}` in {}", self.dir.display());
        }
        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        self.cache.lock().remove(&path);
        Ok(path)
    }

    /// Reload the cache from disk. Only useful to force a re-read of a file
    /// whose mtime did not change; normal operation needs no call at all.
    pub fn invalidate(&self) {
        self.cache.lock().clear();
    }
}
