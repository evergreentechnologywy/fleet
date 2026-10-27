//! The doctor: find drift, and fix what is safe to fix.
//!
//! Drift is the fleet's real failure mode, and it is never a crash — it is a
//! silent disagreement between three things that are all individually fine:
//!
//! * what the CATALOG declares the fleet should have,
//! * what is INSTALLED in the fleet home, and
//! * what is actually PRESENT on the machine.
//!
//! Any pair can disagree. A descriptor for a CLI that was uninstalled still
//! loads and fails at call time. A CLI on PATH with no descriptor is capacity
//! the planner cannot see. A catalog entry neither installed nor present is
//! simply work not yet done. Only the first two are drift, and only they are
//! reported as problems.
//!
//! Every check returns EVIDENCE, and every repair is either idempotent or
//! explicitly dry-runnable, so running the doctor twice is never worse than
//! running it once.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The fleet is wrong right now: a call will fail, or capacity is invisible.
    Problem,
    /// Worth doing, nothing is broken.
    Suggestion,
    /// Recorded for completeness.
    Info,
}

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub severity: Severity,
    /// Short id, stable across runs so it can be filtered and counted.
    pub code: String,
    /// What is wrong, in one line.
    pub summary: String,
    /// The evidence: paths, names, counts. Never a secret.
    pub evidence: String,
    /// What to run to fix it, when that is a command.
    pub fix: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub problems: usize,
    pub suggestions: usize,
    pub ok: bool,
}

impl Report {
    fn new() -> Self {
        Self {
            findings: Vec::new(),
            problems: 0,
            suggestions: 0,
            ok: true,
        }
    }

    fn push(&mut self, f: Finding) {
        match f.severity {
            Severity::Problem => self.problems += 1,
            Severity::Suggestion => self.suggestions += 1,
            Severity::Info => {}
        }
        self.findings.push(f);
    }

    fn finish(mut self) -> Self {
        self.ok = self.problems == 0;
        self
    }
}

/// Run every check. Read-only: this never changes anything.
pub fn check() -> Report {
    let mut r = Report::new();
    check_harnesses(&mut r);
    check_catalog(&mut r);
    check_memory(&mut r);
    check_keys(&mut r);
    check_chooser(&mut r);
    check_capability_contract(&mut r);
    check_planner_capacity(&mut r);
    r.finish()
}

/// A descriptor whose CLI is gone is drift: it loads, it is offered, and it
/// fails at call time with a message about the CLI rather than about the
/// descriptor that should be removed.
fn check_harnesses(r: &mut Report) {
    let reg = crate::registry::Registry::default_dir();
    let (harnesses, errors) = reg.all();
    for (path, err) in errors {
        r.push(Finding {
            severity: Severity::Problem,
            code: "harness.invalid".into(),
            summary: format!("{} is not a valid descriptor", path.display()),
            evidence: err,
            fix: Some(format!("fleet harness rm {}", path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default())),
        });
    }
    for h in &harnesses {
        if !h.enabled {
            continue;
        }
        let (ok, why) = crate::runner::probe(h);
        if !ok {
            r.push(Finding {
                severity: Severity::Problem,
                code: "harness.unavailable".into(),
                summary: format!("harness `{}` is enabled but not runnable", h.name),
                evidence: why,
                fix: Some(format!(
                    "fleet harness disable {}   # or install the CLI it names",
                    h.name
                )),
            });
        }
    }
    if harnesses.iter().filter(|h| h.enabled).count() < 2 {
        r.push(Finding {
            severity: Severity::Suggestion,
            code: "harness.few".into(),
            summary: "fewer than two enabled harnesses: the planner has nothing to choose between".into(),
            evidence: format!("{} enabled", harnesses.iter().filter(|h| h.enabled).count()),
            fix: Some("fleet catalog list --present   # install what is already on this machine".into()),
        });
    }
}

/// The three-way comparison. This is the check that actually prevents drift.
fn check_catalog(r: &mut Report) {
    let cat = crate::catalog::Catalog::default_dir();
    let (found, errors) = cat.discover();
    for (path, err) in errors {
        r.push(Finding {
            severity: Severity::Problem,
            code: "catalog.invalid".into(),
            summary: format!("{} is not a valid catalog entry", path.display()),
            evidence: err,
            fix: None,
        });
    }
    for d in &found {
        let name = &d.entry.name;
        match (d.present, d.installed) {
            // Present but not wired: free capacity the planner cannot see.
            (true, false) if d.entry.kind != crate::catalog::Kind::Integration => {
                r.push(Finding {
                    severity: Severity::Suggestion,
                    code: "catalog.installable".into(),
                    summary: format!("`{name}` is on this machine but not installed into the fleet"),
                    evidence: d.evidence.clone(),
                    fix: Some(format!("fleet catalog install {name}")),
                });
            }
            // Wired but absent: the drift that breaks calls.
            (false, true) => {
                r.push(Finding {
                    severity: Severity::Problem,
                    code: "catalog.dangling".into(),
                    summary: format!("`{name}` is installed into the fleet but not present on this machine"),
                    evidence: d.evidence.clone(),
                    fix: Some(format!(
                        "fleet catalog install {name}   # or remove the artifact it left behind"
                    )),
                });
            }
            _ => {}
        }
        // Keys the entry needs and the fleet does not have.
        if !d.keys_missing.is_empty() && d.present {
            r.push(Finding {
                severity: Severity::Suggestion,
                code: "catalog.keys".into(),
                summary: format!("`{name}` is present but missing {} key(s)", d.keys_missing.len()),
                evidence: d.keys_missing.join(", "),
                fix: Some(format!("fleet keys show   # then set: {}", d.keys_missing.join(", "))),
            });
        }
    }
}

fn check_memory(r: &mut Report) {
    let (name, cfg) = crate::memory_backend::active();
    if !crate::memory_backend::is_known(&name) {
        r.push(Finding {
            severity: Severity::Problem,
            code: "memory.unknown".into(),
            summary: format!("memory backend `{name}` is not one the engine knows"),
            evidence: crate::memory_backend::backends()
                .iter()
                .map(|b| b.name.clone())
                .collect::<Vec<_>>()
                .join(", "),
            fix: Some("fleet memory backend sqlite".into()),
        });
        return;
    }
    let path = crate::memory_backend::active_path();
    let exists = std::path::Path::new(&path).exists();
    if name == "sqlite" && !exists {
        r.push(Finding {
            severity: Severity::Suggestion,
            code: "memory.absent".into(),
            summary: "the memory database does not exist yet; it is created on first write".into(),
            evidence: path.clone(),
            fix: None,
        });
    }
    if cfg.memory.backend.is_none() {
        r.push(Finding {
            severity: Severity::Info,
            code: "memory.default".into(),
            summary: "memory backend is the default (`sqlite`) — instant, local, single-writer".into(),
            evidence: path.clone(),
            fix: Some("fleet memory backend   # see the alternatives and their trade-offs".into()),
        });
    }
}

fn check_keys(r: &mut Report) {
    let inv = crate::keys::inventory();
    let missing: Vec<String> = inv
        .iter()
        .filter(|k| k["present"] == serde_json::json!(false))
        .filter_map(|k| k["name"].as_str().map(|s| s.to_string()))
        .collect();
    let present = inv.len() - missing.len();
    if present == 0 {
        r.push(Finding {
            severity: Severity::Problem,
            code: "keys.none".into(),
            summary: "no known keys are set: nothing can be called".into(),
            evidence: format!("{} expected", inv.len()),
            fix: Some("fleet keys show".into()),
        });
    } else {
        r.push(Finding {
            severity: Severity::Info,
            code: "keys.summary".into(),
            summary: format!("{present} of {} known keys are set", inv.len()),
            evidence: missing.iter().take(8).cloned().collect::<Vec<_>>().join(", "),
            fix: Some("fleet keys show".into()),
        });
    }
    // The env file must never be world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = crate::paths::home().join("env");
        if let Ok(md) = std::fs::metadata(&f) {
            let mode = md.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                r.push(Finding {
                    severity: Severity::Problem,
                    code: "keys.permissions".into(),
                    summary: "the fleet env file is readable by other users".into(),
                    evidence: format!("{} mode {:o}", f.display(), mode),
                    fix: Some(format!("chmod 600 {}", f.display())),
                });
            }
        }
    }
}

fn check_chooser(r: &mut Report) {
    let cfg = crate::config::Config::load();
    let kind = format!("{:?}", cfg.chooser.kind).to_lowercase();
    if kind == "static" {
        r.push(Finding {
            severity: Severity::Suggestion,
            code: "chooser.static".into(),
            summary: "no chooser is configured; every call uses the default model".into(),
            evidence: cfg.defaults.model.clone().unwrap_or_else(|| "(none)".into()),
            fix: Some("fleet config set-chooser cli   # or: fleet config set-default <model>".into()),
        });
    }
    // A CLI chooser whose program is missing fails every call.
    if kind == "cli" {
        if let Some(cmd) = cfg.chooser.command.clone() {
            let p = std::path::Path::new(&cmd);
            let ok = if p.is_absolute() { p.exists() } else { crate::runner::which(&cmd).is_some() };
            if !ok {
                r.push(Finding {
                    severity: Severity::Problem,
                    code: "chooser.missing".into(),
                    summary: "the configured chooser program does not exist".into(),
                    evidence: cmd,
                    fix: Some("fleet config set-chooser static".into()),
                });
            }
        }
    }
}


/// The consultation's #1 finding, made checkable: nothing validated the pair
/// (chooser's answer, harness's declared ceiling) at load, so a mismatch only
/// appeared as a confusing error INSIDE the CLI — "model X is not served by any
/// door lane" (404), or "getaddrinfo ENOTFOUND" for a fabricated base URL.
///
/// The check is cheap and offline: for every enabled harness that routes through
/// a provider, confirm the provider's base URL is reachable enough to be real,
/// and that the harness's declared ceiling is not empty. A harness whose ceiling
/// is `["*"]` cannot clamp anything, which is how a chooser's answer for one
/// model reached a harness that only spoke another.
fn check_capability_contract(r: &mut Report) {
    let reg = crate::registry::Registry::default_dir();
    let (harnesses, _) = reg.all();
    for h in harnesses.iter().filter(|h| h.enabled) {
        // A ceiling of `*` means "accept anything", which defeats the clamp the
        // router is supposed to perform. That is only correct for a harness that
        // really can serve anything, and those are rare.
        if h.models.iter().any(|m| m == "*") && h.kind == crate::registry::Kind::Cli {
            r.push(Finding {
                severity: Severity::Suggestion,
                code: "capability.unclamped".into(),
                summary: format!("harness `{}` accepts any model (`models = [\"*\"]`)", h.name),
                evidence: "the router clamps the chooser's answer to this list; `*` clamps nothing"
                    .into(),
                fix: Some(format!(
                    "declare what `{}` can actually serve in {}/harnesses/{}.toml",
                    h.name,
                    crate::paths::home().display(),
                    h.name
                )),
            });
        }
        // A CLI harness whose command is a bare name and is not on PATH is
        // already reported; here we catch the config-shaped faults.
        if h.route == crate::registry::Route::Jev {
            if let Some(env) = h.env.get("ANTHROPIC_BASE_URL") {
                if !reachable(env) {
                    r.push(Finding {
                        severity: Severity::Problem,
                        code: "capability.endpoint".into(),
                        summary: format!("harness `{}` points at an endpoint that does not answer", h.name),
                        evidence: env.clone(),
                        fix: Some(format!("start the endpoint, or fix ANTHROPIC_BASE_URL in {}.toml", h.name)),
                    });
                }
            }
        }
    }

    // A provider whose base URL does not resolve is the "fabricated URL" fault:
    // it reads as a network error inside the CLI, never as a config error.
    let providers = crate::providers::Providers::default_file();
    for p in providers.all().into_iter().filter(|p| p.enabled) {
        if !p.base_url.starts_with("http") {
            continue;
        }
        if !reachable(&p.base_url) {
            r.push(Finding {
                severity: Severity::Problem,
                code: "provider.unreachable".into(),
                summary: format!("provider `{}` base URL does not answer", p.name),
                evidence: p.base_url.clone(),
                fix: Some(format!("fleet providers list   # then fix or remove `{}`", p.name)),
            });
        }
    }
}

/// A cheap reachability test: a TCP connect to host:port, so a fabricated
/// hostname or a dead port is caught without spending an HTTP request.
fn reachable(url: &str) -> bool {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse::<u16>().unwrap_or(443)),
        None => (
            hostport.to_string(),
            if url.starts_with("https") { 443 } else { 80 },
        ),
    };
    use std::net::{TcpStream, ToSocketAddrs};
    let Ok(mut addrs) = (host.as_str(), port).to_socket_addrs() else {
        return false;
    };
    match addrs.next() {
        Some(addr) => TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(2)).is_ok(),
        None => false,
    }
}

/// Capacity that exists but is invisible to the planner is the quietest drift:
/// nothing is broken, the fleet is just smaller than it should be.
fn check_planner_capacity(r: &mut Report) {
    let reg = crate::registry::Registry::default_dir();
    let (harnesses, _) = reg.all();
    let declared: Vec<String> = harnesses.iter().map(|h| h.name.clone()).collect();
    // Known CLIs the fleet could drive but has no descriptor for.
    const CANDIDATES: [(&str, &str); 8] = [
        ("omp", "omp"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("claude", "claude"),
        ("gemini", "gemini"),
        ("qwen", "qwen"),
        ("crush", "crush"),
        ("cline", "cline"),
    ];
    let missing: Vec<&str> = CANDIDATES
        .iter()
        .filter(|(cmd, _)| crate::runner::which(cmd).is_some())
        .filter(|(_, name)| !declared.iter().any(|d| d == name))
        .map(|(_, name)| *name)
        .collect();
    if !missing.is_empty() {
        r.push(Finding {
            severity: Severity::Suggestion,
            code: "capacity.unwired".into(),
            summary: format!("{} harness CLI(s) on PATH have no descriptor", missing.len()),
            evidence: missing.join(", "),
            fix: Some("fleet catalog list --present   # then: fleet catalog install <name>".into()),
        });
    }
}

/// Repair what is safe to repair, and say what it did. Never destructive: it
/// installs artifacts the catalog declares and the machine already has, and
/// nothing else.
pub fn heal(dry_run: bool) -> anyhow::Result<Vec<String>> {
    let mut log = Vec::new();
    let cat = crate::catalog::Catalog::default_dir();
    let (found, _) = cat.discover();
    for d in found {
        // Only wire something the machine can actually run.
        if !d.present || d.installed {
            continue;
        }
        if d.entry.kind == crate::catalog::Kind::Integration {
            continue;
        }
        let action = format!("install {} ({})", d.entry.name, d.evidence);
        if dry_run {
            log.push(format!("would {action}"));
            continue;
        }
        match crate::catalog::install_artifact(&d.entry, &d.dir) {
            Ok(paths) => log.push(format!(
                "{action} -> {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Err(e) => log.push(format!("{action} FAILED: {e}")),
        }
    }
    Ok(log)
}
