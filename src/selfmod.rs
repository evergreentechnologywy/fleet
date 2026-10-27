//! Verifier-gated self-modification — the Octopus contracts, ported.
//!
//! The rule: the engine may change itself (a harness descriptor, a config, a
//! router default), but a change becomes a *mark* only after a deterministic
//! verifier passes over the evidence. Nothing self-reports success.
//!
//! * **ceiling vs grant.** `policy.toml` is a static frame: which outcomes, which
//!   verifiers, which paths this engine may ever touch. A concrete request is
//!   checked against the frame *before* any verifier runs — fail closed, never
//!   "grant it and then check".
//! * **verifier packages.** A verifier is a directory with `verifier.toml` and a
//!   script. It reads one request JSON on stdin and writes one verdict JSON on
//!   stdout. Non-JSON output, a non-zero exit, a missing `passed`, or a timeout
//!   all count as failed.
//! * **the ledger is truth.** A failed verdict is a durable row.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::ledger::{digest_file, Entry, Ledger};

/// The static frame. Empty means a closed engine: nothing is self-modifiable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ceiling {
    /// Paths the engine may write, as globs relative to the fleet home.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Verifier names this engine will accept a pass from.
    #[serde(default)]
    pub verifiers: Vec<String>,
    /// Self-modification is off unless explicitly enabled.
    #[serde(default)]
    pub enabled: bool,
}

impl Ceiling {
    pub fn load() -> Self {
        let p = crate::paths::home().join("policy.toml");
        match std::fs::read_to_string(&p) {
            Ok(t) => toml::from_str(&t).unwrap_or_default(),
            Err(_) => Ceiling::default(),
        }
    }

    pub fn permits(&self, rel_path: &str) -> bool {
        self.paths.iter().any(|g| glob_match(g, rel_path))
    }
}

/// `*` matches within a segment, `**` matches across segments. Small enough to
/// read in one sitting, which matters more here than speed.
/// Match one path segment against one pattern segment, where `*` stands for any
/// run of characters. Everything else is literal.
fn seg_matches(pattern: &str, seg: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == seg;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = seg;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // The first chunk must be a prefix.
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            // The last chunk must be a suffix of what is left.
            return rest.ends_with(part) && rest.len() >= part.len();
        } else {
            match rest.find(part) {
                Some(pos) => rest = &rest[pos + part.len()..],
                None => return false,
            }
        }
    }
    true
}

pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = path.split('/').collect();
    fn go(p: &[&str], t: &[&str]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(&"**") => {
                for i in 0..=t.len() {
                    if go(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            Some(seg) => {
                if t.is_empty() {
                    return false;
                }
                // `*` matches exactly one segment; `**` (handled above) matches
                // any number. A segment may also embed `*`, so `*.toml` works —
                // without that, a ceiling entry of `harnesses/*.toml` silently
                // permits nothing, which fails closed but looks like a bug.
                if *seg == "*" || seg_matches(seg, t[0]) {
                    go(&p[1..], &t[1..])
                } else {
                    false
                }
            }
        }
    }
    go(&p, &t)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifierSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Executable run per admission.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// What is being asked: one concrete change with its evidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    /// What is being changed, relative to the fleet home.
    pub target: String,
    /// Why. Recorded in the ledger so a later reader knows the intent.
    #[serde(default)]
    pub reason: String,
    /// The proposed new content, if the change is a whole-file replacement.
    #[serde(default)]
    pub content: Option<String>,
    /// Digest of the artifact the verifier should look at. Computed from
    /// `content` or from the existing file when omitted.
    #[serde(default)]
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub passed: bool,
    #[serde(default)]
    pub detail: String,
}

/// Run one verifier package over a grant.
pub fn verify(spec: &VerifierSpec, grant: &Grant) -> Verdict {
    let request = serde_json::json!({
        "protocol": "fleet/verifier/1",
        "target": grant.target,
        "reason": grant.reason,
        "digest": grant.digest,
        "content": grant.content,
    });
    let body = serde_json::to_string(&request).unwrap_or_default();
    // The verifier reads stdin; `run_capture` cannot pipe input, so the body is
    // written to a temp file and passed by path as well. Verifiers may read
    // either, which keeps a shell-script verifier a two-liner.
    let tmp = crate::paths::home().join("runs").join(format!(
        "grant-{}.json",
        grant.digest.chars().take(12).collect::<String>()
    ));
    if let Some(p) = tmp.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let _ = std::fs::write(&tmp, &body);

    let mut args = spec.args.clone();
    args.push(tmp.to_string_lossy().to_string());
    let timeout = Duration::from_secs(spec.timeout_s.unwrap_or(60));
    match crate::runner::run_capture(&spec.command, &args, &[], None, timeout) {
        Ok(cap) if cap.ok => match serde_json::from_str::<Verdict>(cap.stdout.trim()) {
            Ok(v) => v,
            Err(e) => Verdict {
                passed: false,
                detail: format!("verifier output was not a verdict: {e}"),
            },
        },
        Ok(cap) => Verdict {
            passed: false,
            detail: format!(
                "verifier exited {}: {}",
                cap.code,
                crate::runner::truncate(cap.stderr.trim(), 300)
            ),
        },
        Err(e) => Verdict {
            passed: false,
            detail: format!("verifier failed to run: {e}"),
        },
    }
}

pub fn load_verifier(name: &str) -> Result<VerifierSpec> {
    let path = crate::paths::verifier_dir()
        .join(name)
        .join("verifier.toml");
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The admission path: frame check, then verifier, then mark-or-fail.
///
/// Returns the digest on admission. A rejected grant leaves a `verify` row with
/// `ok = false`; an admitted one leaves a `mark` row with `ok = true`.
pub fn propose(ceiling: &Ceiling, grant: &Grant, ledger: &Ledger, run: &str) -> Result<String> {
    if !ceiling.enabled {
        bail!("self-modification is disabled: set `enabled = true` in policy.toml");
    }
    // Frame check first, and it is a hard failure: the request must fit inside
    // the ceiling before any verifier is allowed to look at it.
    if !ceiling.permits(&grant.target) {
        ledger.append(&Entry {
            ts: crate::now(),
            kind: "verify".into(),
            run: run.to_string(),
            session: String::new(),
            harness: String::new(),
            subject: grant.target.clone(),
            detail: "refused: outside the ceiling".into(),
            ok: false,
            digest: String::new(),
        })?;
        bail!(
            "refused before verification: `{}` is outside the ceiling {:?}",
            grant.target,
            ceiling.paths
        );
    }
    let digest = if !grant.digest.is_empty() {
        grant.digest.clone()
    } else if let Some(c) = &grant.content {
        crate::ledger::digest_bytes(c.as_bytes())
    } else {
        let abs = crate::paths::home().join(&grant.target);
        digest_file(&abs).unwrap_or_default()
    };
    if digest.is_empty() {
        bail!("no artifact to verify for `{}`", grant.target);
    }

    let mut last = Verdict {
        passed: false,
        detail: "no verifier ran".into(),
    };
    for name in &ceiling.verifiers {
        let spec = match load_verifier(name) {
            Ok(s) => s,
            Err(e) => {
                // A verifier that cannot even be loaded is a failed
                // verification, and a failed verification is a durable row.
                // Dropping it here would make "no row" mean both "never asked"
                // and "asked and could not be answered".
                last = Verdict {
                    passed: false,
                    detail: format!("verifier `{name}` unavailable: {e}"),
                };
                ledger.append(&Entry {
                    ts: crate::now(),
                    kind: "verify".into(),
                    run: run.to_string(),
                    session: String::new(),
                    harness: name.clone(),
                    subject: grant.target.clone(),
                    detail: last.detail.clone(),
                    ok: false,
                    digest: digest.clone(),
                })?;
                break;
            }
        };
        let g = Grant {
            digest: digest.clone(),
            ..grant.clone()
        };
        let v = verify(&spec, &g);
        ledger.append(&Entry {
            ts: crate::now(),
            kind: "verify".into(),
            run: run.to_string(),
            session: String::new(),
            harness: name.clone(),
            subject: grant.target.clone(),
            detail: v.detail.clone(),
            ok: v.passed,
            digest: digest.clone(),
        })?;
        last = v;
        if !last.passed {
            break;
        }
    }
    if !last.passed {
        bail!(
            "verification failed for `{}`: {}",
            grant.target,
            last.detail
        );
    }
    ledger.append(&Entry {
        ts: crate::now(),
        kind: "mark".into(),
        run: run.to_string(),
        session: String::new(),
        harness: String::new(),
        subject: grant.target.clone(),
        detail: grant.reason.clone(),
        ok: true,
        digest: digest.clone(),
    })?;
    Ok(digest)
}

/// The built-in verifier: does the candidate file parse as the format it
/// claims? Shipped as a script so it demonstrates the package shape and can be
/// replaced without touching the engine.
pub fn write_builtin_verifier() -> Result<PathBuf> {
    let dir = crate::paths::verifier_dir().join("toml-parses");
    std::fs::create_dir_all(&dir)?;
    let spec = dir.join("verifier.toml");
    if !spec.exists() {
        std::fs::write(
            &spec,
            "name = \"toml-parses\"\n\
             description = \"a .toml target must parse; anything else passes\"\n\
             command = \"python3\"\n\
             args = [\"-c\", \"\"\"\n\
             import json,sys,tomllib\n\
             g=json.load(open(sys.argv[1]))\n\
             t=g.get('target','')\n\
             d=g.get('detail') or g.get('content')\n\
             if t.endswith('.toml') and d:\n\
             \x20   try: tomllib.loads(d)\n\
             \x20   except Exception as e:\n\
             \x20       print(json.dumps({'passed':False,'detail':'toml parse: %s'%e})); sys.exit(2)\n\
             print(json.dumps({'passed':True,'detail':'ok'}))\n\
             \"\"\"]\n\
             timeout_s = 30\n",
        )?;
    }
    Ok(dir)
}

/// Convenience wrapper used by `fleet selfmod check`.
pub fn check(target: &str) -> Result<String> {
    let ceiling = Ceiling::load();
    let ledger = Ledger::default_path();
    let home = crate::paths::home();
    let abs = home.join(target);
    if !abs.exists() && target.ends_with(".toml") {
        // Descriptors are written by `harness add`, so a missing file is only
        // an error when nothing at all is known about it.
        if !ceiling.permits(target) {
            bail!("`{target}` is outside the ceiling {:?}", ceiling.paths);
        }
        return Ok(format!(
            "path `{target}` is inside the ceiling; no artifact to digest yet"
        ));
    }
    let digest = digest_file(&abs)?;
    let _ = ledger;
    Ok(format!("{target} sha256={digest}"))
}

/// A one-line summary of what the engine may modify, for `fleet selfmod status`.
pub fn status(ledger: &Ledger) -> Result<serde_json::Value> {
    let ceiling = Ceiling::load();
    let marks = ledger.tail(Some("mark"), 50)?;
    let fails = ledger.tail(Some("verify"), 200)?;
    Ok(serde_json::json!({
        "enabled": ceiling.enabled,
        "paths": ceiling.paths,
        "verifiers": ceiling.verifiers,
        "marks": marks.len(),
        "failed_verifications": fails.iter().filter(|e| !e.ok).count(),
        "last_marks": marks.into_iter().take(10).map(|m| serde_json::json!({
            "ts": m.ts, "target": m.subject, "digest": m.digest, "reason": m.detail
        })).collect::<Vec<_>>(),
    }))
}

/// Guard used by the engine before it writes any file on the model's behalf.
pub fn guard_write(rel_path: &str) -> Result<()> {
    let ceiling = Ceiling::load();
    if !ceiling.enabled || ceiling.paths.is_empty() {
        return Err(anyhow!(
            "self-modification is off; `{rel_path}` must be changed by a human"
        ));
    }
    if !ceiling.permits(rel_path) {
        bail!("`{rel_path}` is outside the ceiling");
    }
    Ok(())
}

/// Resolve `target` under the fleet home, refusing to escape it.
pub fn resolve_inside_home(target: &str) -> Result<PathBuf> {
    let home = crate::paths::home();
    let abs = home.join(target);
    let norm = abs.components().fold(PathBuf::new(), |mut acc, c| {
        match c {
            std::path::Component::ParentDir => {
                acc.pop();
            }
            std::path::Component::CurDir => {}
            other => acc.push(other),
        };
        acc
    });
    if !norm.starts_with(&home) {
        bail!("`{target}` escapes {}", home.display());
    }
    Ok(norm)
}

/// Read a verifier directory listing, for `fleet selfmod verifiers`.
pub fn list_verifiers() -> Vec<(String, String)> {
    let dir = crate::paths::verifier_dir();
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Ok(spec) = load_verifier(&name) else {
                continue;
            };
            out.push((name, spec.description));
        }
    }
    out.sort();
    out
}

/// Path helper used by tests and the CLI.
pub fn policy_path() -> PathBuf {
    crate::paths::home().join("policy.toml")
}

/// Write a starter policy with the engine's own descriptors inside the ceiling.
pub fn write_starter_policy() -> Result<()> {
    let p = policy_path();
    if p.exists() {
        return Ok(());
    }
    std::fs::write(
        &p,
        "# The ceiling: what this engine may ever modify on its own behalf.\n\
         # Nothing here is self-modifiable until this file says so.\n\
         enabled = false\n\
         paths = [\"harnesses/*.toml\", \"mods/**\", \"memory/**\"]\n\
         verifiers = [\"toml-parses\"]\n",
    )
    .with_context(|| format!("writing {}", p.display()))?;
    Ok(())
}
