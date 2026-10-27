//! The catalog: everything the fleet can detect, install, and wire — as data.
//!
//! A catalog entry is a DIRECTORY under `$FLEET_HOME/catalog/<name>/` holding
//! `catalog.toml` (what it is, how to detect it, how to install it, which keys it
//! needs) plus the artifact it installs (`harness.toml`, `plugin/`, ...).
//!
//! The directory is re-read on every call, exactly like the harness registry, so
//! adding an entry is writing files — no recompile, no restart, no drift between
//! what the code knows and what the fleet has. That is the whole point: the
//! fleet's reach grows by editing data, and the doctor can compare what is
//! DECLARED against what is DETECTED to find the difference.
//!
//! ```text
//! $FLEET_HOME/catalog/
//!   grok/
//!     catalog.toml      kind, description, detect, install, keys, docs, tags
//!     harness.toml      copied into $FLEET_HOME/harnesses/ on install
//!   composio/
//!     catalog.toml
//!     plugin/           copied into $FLEET_HOME/plugins/ on install
//!       plugin.toml
//!       run.sh
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What an entry installs. Each kind lands somewhere different, and the doctor
/// checks each in the way that kind can actually be checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A harness descriptor -> `harnesses/`.
    Harness,
    /// An engine-level plugin directory -> `plugins/`.
    Plugin,
    /// A provider declaration -> `providers.toml`.
    Provider,
    /// A deploy target that teaches `deploy` a new platform -> `deploy-targets/`.
    Target,
    /// A cloud VM runner (a `deploy-target` plus the notes to use it).
    Runner,
    /// A memory backend the engine can be switched to.
    Memory,
    /// Something the fleet only needs to KNOW about (an integration it reaches
    /// over HTTP with a key). Nothing is copied; the key and the docs are the
    /// value.
    Integration,
}

/// How to tell whether this is already on the machine.
///
/// Ordered from cheapest to most expensive: a `command` is a PATH lookup, a
/// `path` is a stat, an `env` is a variable read, and an `http` is a live probe
/// (last, because it costs a request).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Detect {
    /// Executable on `PATH`.
    #[serde(default)]
    pub command: Option<String>,
    /// A file or directory that must exist. `~` is expanded.
    #[serde(default)]
    pub path: Option<String>,
    /// An environment variable that must be non-empty.
    #[serde(default)]
    pub env: Option<String>,
    /// An endpoint that must answer.
    #[serde(default)]
    pub http: Option<String>,
    /// Any of these alternatives counts. Each is a `command` name.
    #[serde(default)]
    pub any_command: Vec<String>,
}

/// How to install it. At most one is used, chosen by what the machine has.
///
/// Every field is the package/URL, never a shell pipeline: the engine builds the
/// command, so an entry cannot smuggle in something arbitrary, and the printed
/// one-liner is the same one that runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    #[serde(default)]
    pub npm: Option<String>,
    #[serde(default)]
    pub pip: Option<String>,
    #[serde(default)]
    pub cargo: Option<String>,
    #[serde(default)]
    pub brew: Option<String>,
    /// A shell one-liner, for the projects that only ship one. Run as written;
    /// the entry is local data, and this is the field that says so.
    #[serde(default)]
    pub script: Option<String>,
    /// Where a human goes to install it by hand, when there is no automation.
    #[serde(default)]
    pub url: Option<String>,
    /// Args the installer needs beyond the package name (e.g. a global flag).
    #[serde(default)]
    pub npm_global: bool,
}

/// One catalog entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Where the project lives, for a human who wants to read before installing.
    #[serde(default)]
    pub docs: Option<String>,
    /// Environment variables this entry needs. The catalog does not hold keys;
    /// it holds the NAMES, so `fleet keys import` can fill them and
    /// `fleet catalog list` can show what is still missing.
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub detect: Detect,
    #[serde(default)]
    pub install: Install,
    /// Only offered when one of these is present on `PATH` (e.g. `docker`).
    #[serde(default)]
    pub requires: Vec<String>,
    /// Set false to keep an entry in the catalog but out of `install --all`.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Free-form note shown by `fleet catalog show`.
    #[serde(default)]
    pub notes: String,
}

fn yes() -> bool {
    true
}

/// Every entry on disk, paired with its directory, plus the files that failed
/// to parse — a shape used by both `entries` and `discover`.
pub type Entries = (Vec<(Entry, PathBuf)>, Vec<(PathBuf, String)>);

/// The catalog directory, re-read per call.
pub struct Catalog {
    dir: PathBuf,
}

/// A catalog entry plus where it came from and whether it is present.
#[derive(Debug, Clone, Serialize)]
pub struct Discovered {
    pub entry: Entry,
    pub dir: PathBuf,
    /// Detected on this machine right now.
    pub present: bool,
    /// The evidence for `present` — the path, the variable, the URL.
    pub evidence: String,
    /// Which required keys are set, and which are missing.
    pub keys_present: Vec<String>,
    pub keys_missing: Vec<String>,
    /// True when this entry's artifact is already installed into the fleet home.
    pub installed: bool,
}

impl Catalog {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn default_dir() -> Self {
        Self::new(crate::paths::catalog_dir())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every entry on disk, sorted by kind then name. A malformed entry is
    /// reported and skipped, because one bad file must not hide the rest.
    pub fn entries(&self) -> Entries {
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return (out, errors);
        };
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let file = path.join("catalog.toml");
            let text = match std::fs::read_to_string(&file) {
                Ok(t) => t,
                Err(_) => continue, // a directory without a manifest is not an entry
            };
            match toml::from_str::<Entry>(&text) {
                Ok(entry) => out.push((entry, path)),
                Err(err) => errors.push((file, err.to_string())),
            }
        }
        out.sort_by(|a, b| {
            format!("{:?}", a.0.kind)
                .cmp(&format!("{:?}", b.0.kind))
                .then(a.0.name.cmp(&b.0.name))
        });
        (out, errors)
    }

    pub fn get(&self, name: &str) -> Option<(Entry, PathBuf)> {
        self.entries().0.into_iter().find(|(e, _)| e.name == name)
    }

    /// Everything, with its live state resolved.
    pub fn discover(&self) -> (Vec<Discovered>, Vec<(PathBuf, String)>) {
        let (entries, errors) = self.entries();
        let found = entries
            .into_iter()
            .map(|(entry, dir)| {
                let (present, evidence) = detect(&entry);
                // Partition by borrowing, then clone the names out: the entry is
                // moved into the result below, so the keys must not borrow it.
                let keys_present: Vec<String> = entry
                    .keys
                    .iter()
                    .filter(|k| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false))
                    .cloned()
                    .collect();
                let keys_missing: Vec<String> = entry
                    .keys
                    .iter()
                    .filter(|k| !std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false))
                    .cloned()
                    .collect();
                let installed = is_installed(&entry, &dir);
                Discovered {
                    entry,
                    dir,
                    present,
                    evidence,
                    keys_present,
                    keys_missing,
                    installed,
                }
            })
            .collect();
        (found, errors)
    }
}

/// Is this entry present on the machine? Returns the evidence either way, so a
/// "no" says what it looked for instead of being an unexplained silence.
pub fn detect(e: &Entry) -> (bool, String) {
    let d = &e.detect;
    if let Some(cmd) = &d.command {
        return match crate::runner::which(cmd) {
            Some(p) => (true, p.display().to_string()),
            None => (false, format!("`{cmd}` not on PATH")),
        };
    }
    if !d.any_command.is_empty() {
        for c in &d.any_command {
            if let Some(p) = crate::runner::which(c) {
                return (true, p.display().to_string());
            }
        }
        return (false, format!("none of {} on PATH", d.any_command.join(", ")));
    }
    if let Some(p) = &d.path {
        let path = crate::paths::expand(p);
        return if path.exists() {
            (true, path.display().to_string())
        } else {
            (false, format!("{} missing", path.display()))
        };
    }
    if let Some(v) = &d.env {
        return match std::env::var(v) {
            Ok(val) if !val.is_empty() => (true, format!("${v} set")),
            _ => (false, format!("${v} unset")),
        };
    }
    if let Some(u) = &d.http {
        return match ureq::get(u).timeout(std::time::Duration::from_secs(3)).call() {
            Ok(_) => (true, format!("{u} answered")),
            Err(ureq::Error::Status(c, _)) => (true, format!("{u} answered {c}")),
            Err(err) => (false, format!("{u} unreachable: {err}")),
        };
    }
    (false, "no detect rule".into())
}

/// Is the entry's artifact already installed into the fleet home?
///
/// This is a different question from `detect`: a CLI can be on `PATH` without a
/// descriptor, and a descriptor can exist for a CLI that is gone. The doctor
/// cares about both, and reporting one as the other is how drift hides.
pub fn is_installed(e: &Entry, dir: &Path) -> bool {
    match e.kind {
        Kind::Harness => crate::paths::harness_dir()
            .join(format!("{}.toml", e.name))
            .is_file(),
        Kind::Plugin => crate::paths::home().join("plugins").join(&e.name).is_dir(),
        Kind::Provider => {
            let p = crate::providers::Providers::default_file();
            p.all().iter().any(|x| x.name == e.name)
        }
        Kind::Target => {
            let _ = dir;
            crate::deploy_targets::Targets::default_dir()
                .get(&e.name)
                .is_some()
        }
        Kind::Memory => {
            // A backend is "installed" when it is the configured one.
            let cfg = crate::config::Config::load();
            cfg.memory.backend.as_deref() == Some(e.name.as_str())
        }
        Kind::Runner | Kind::Integration => false,
    }
}

/// What `install` would run, without running it. Empty when there is nothing to
/// automate, in which case the entry carries a `url` for a human instead.
pub fn install_command(e: &Entry) -> Option<Vec<String>> {
    let i = &e.install;
    if let Some(pkg) = &i.npm {
        let mut v = vec!["npm".to_string(), "install".to_string()];
        if i.npm_global {
            v.push("-g".into());
        }
        v.push(pkg.clone());
        return Some(v);
    }
    if let Some(pkg) = &i.pip {
        return Some(vec![
            "pip".into(),
            "install".into(),
            "--break-system-packages".into(),
            pkg.clone(),
        ]);
    }
    if let Some(pkg) = &i.cargo {
        return Some(vec!["cargo".into(), "install".into(), pkg.clone()]);
    }
    if let Some(pkg) = &i.brew {
        return Some(vec!["brew".into(), "install".into(), pkg.clone()]);
    }
    if let Some(s) = &i.script {
        return Some(vec!["sh".into(), "-c".into(), s.clone()]);
    }
    None
}

/// Install one entry's ARTIFACT into the fleet home (not the upstream tool —
/// that is `install_command`). Returns the paths written.
///
/// This is the "one-click integration" half: once the CLI exists, this is what
/// makes the fleet actually use it.
pub fn install_artifact(e: &Entry, dir: &Path) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    match e.kind {
        Kind::Harness => {
            let src = dir.join("harness.toml");
            if !src.is_file() {
                anyhow::bail!("{} has no harness.toml to install", e.name);
            }
            let dst = crate::paths::harness_dir().join(format!("{}.toml", e.name));
            std::fs::create_dir_all(crate::paths::harness_dir())?;
            std::fs::copy(&src, &dst)
                .with_context(|| format!("installing {}", dst.display()))?;
            written.push(dst);
        }
        Kind::Plugin => {
            let src = dir.join("plugin");
            if !src.is_dir() {
                anyhow::bail!("{} has no plugin/ directory to install", e.name);
            }
            let dst = crate::paths::home().join("plugins").join(&e.name);
            std::fs::create_dir_all(&dst)?;
            copy_dir(&src, &dst)?;
            written.push(dst);
        }
        Kind::Target => {
            let src = dir.join("target");
            if !src.is_dir() {
                anyhow::bail!("{} has no target/ directory to install", e.name);
            }
            let dst = crate::deploy_targets::Targets::default_dir()
                .dir()
                .join(&e.name);
            std::fs::create_dir_all(&dst)?;
            copy_dir(&src, &dst)?;
            written.push(dst);
        }
        Kind::Provider => {
            let src = dir.join("provider.toml");
            if !src.is_file() {
                anyhow::bail!("{} has no provider.toml to install", e.name);
            }
            let text = std::fs::read_to_string(&src)?;
            let p: crate::providers::Provider = toml::from_str(&text)
                .with_context(|| format!("parsing {}", src.display()))?;
            crate::providers::Providers::default_file().put(&p)?;
            written.push(crate::providers::Providers::default_file().path().to_path_buf());
        }
        Kind::Memory | Kind::Runner | Kind::Integration => {
            // Nothing is copied: a runner is used through its deploy target, a
            // memory backend through `fleet memory backend`, and an integration
            // through the plugin that wraps it.
        }
    }
    Ok(written)
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            std::fs::create_dir_all(&to)?;
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
            // Preserve the executable bit: a plugin script that lands as 0644
            // fails at spawn with EACCES, which reads as a broken plugin.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(md) = std::fs::metadata(&from) {
                    let mode = md.permissions().mode();
                    if mode & 0o111 != 0 {
                        let _ = std::fs::set_permissions(
                            &to,
                            std::fs::Permissions::from_mode(mode | 0o111),
                        );
                    }
                }
            }
        }
    }
    Ok(())
}
