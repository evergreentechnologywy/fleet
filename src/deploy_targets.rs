//! Deploy targets as plugins.
//!
//! The built-in targets — ssh, tailscale, reason, docker, file — cover a fleet's
//! own machines. They cannot cover every platform a person might want to deploy
//! to: a PaaS with its own CLI, a Kubernetes context, a cloud VM API, a
//! hosting provider's one-liner. Those change constantly, and compiling each one
//! in would make this binary a directory of other people's SDKs.
//!
//! So a target is a **plugin**: a directory in `$FLEET_HOME/deploy-targets/`
//! with a `target.toml` and a script. `fleet deploy --to <scheme>://…` finds it,
//! calls it, and reports what it did. One line to any server becomes one line to
//! any server *that someone has written a target for* — and writing one is a
//! twenty-line script, not a release of this crate.
//!
//! The contract, deliberately small:
//!
//! * `probe` — is this target usable from here (is its CLI installed, is it
//!   authenticated)? Answers `{"ready": true|false, "detail": "…"}`.
//! * `plan`  — what would you run? Answers `{"commands": ["…"], "url": "…"}`.
//!   This is what `--dry-run` prints, so a dry run never executes anything.
//! * `deploy` — do it. Prints lines; the exit code decides success.
//!
//! A target gets the deploy request as JSON on stdin, and may use the binary
//! path and the descriptor bundle the engine staged for it, so a target does not
//! have to know how this engine packages itself.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A target's descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    /// The scheme it answers: `fly`, `k8s`, `hetzner`, `my-paas`.
    pub scheme: String,
    #[serde(default)]
    pub description: String,
    /// Executable implementing the three verbs.
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// A one-line hint printed by `fleet deploy --help` and onboarding.
    #[serde(default)]
    pub example: String,
    /// A URL template shown in the plan's `url` when the script does not give
    /// one: `{host}`, `{port}`, `{scheme}` are substituted.
    #[serde(default)]
    pub url_template: Option<String>,
    /// Documentation URL, printed when a probe fails.
    #[serde(default)]
    pub docs: Option<String>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

/// What a target is told. Everything the engine knows about the deploy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub scheme: String,
    /// Everything after `scheme://`.
    pub host: String,
    pub port: u16,
    pub prefix: String,
    /// Absolute path to the built binary, so a target can upload it.
    pub binary: String,
    /// A tar of the descriptors, or empty when there are none.
    pub bundle: String,
    /// Environment the target's own tooling may need, copied from the caller so
    /// a target never has to read this engine's config.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probe {
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Plan {
    /// The commands a dry run prints, in order.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Where the deployed thing will answer.
    #[serde(default)]
    pub url: String,
}

/// The installed target set. Re-read per call, like every other plugin surface.
pub struct Targets {
    dir: PathBuf,
}

impl Targets {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn default_dir() -> Self {
        Self::new(crate::paths::home().join("deploy-targets"))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Every valid target, sorted by scheme. A broken descriptor is reported and
    /// skipped, because one bad plugin must not disable deploys entirely.
    pub fn all(&self) -> Vec<TargetSpec> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return out;
        };
        let mut dirs: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            let spec_path = if d.is_dir() {
                d.join("target.toml")
            } else if d.extension().and_then(|e| e.to_str()) == Some("toml") {
                d.clone()
            } else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&spec_path) else {
                continue;
            };
            match toml::from_str::<TargetSpec>(&text) {
                Ok(s) if s.enabled => out.push(s),
                Ok(_) => {}
                Err(e) => eprintln!("fleet: deploy target {} invalid: {e}", spec_path.display()),
            }
        }
        out.sort_by(|a, b| a.scheme.cmp(&b.scheme));
        out
    }

    pub fn get(&self, scheme: &str) -> Option<TargetSpec> {
        self.all().into_iter().find(|t| t.scheme == scheme)
    }

    /// Is the scheme a plugin target rather than a built-in?
    pub fn has(&self, scheme: &str) -> bool {
        self.get(scheme).is_some()
    }

    /// Run one verb.
    fn invoke(&self, spec: &TargetSpec, verb: &str, req: &Request) -> Result<String> {
        let body = serde_json::to_string(req)?;
        let mut args = spec.args.clone();
        args.push(verb.to_string());
        let env: Vec<(String, String)> = req
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        // The script runs in its own directory so `args = ["deploy.sh"]` works,
        // exactly like an engine plugin.
        let cwd = self.dir.join(&spec.scheme);
        let cwd = if cwd.is_dir() { Some(cwd.as_path()) } else { None };
        let (code, out, err) = run_with_stdin(
            &spec.command,
            &args,
            &body,
            Duration::from_secs(spec.timeout_s.unwrap_or(1800)),
            cwd,
            &env,
        )
        .with_context(|| format!("running deploy target `{}`", spec.scheme))?;
        if code != 0 {
            // A probe explains itself on stdout (a `{"ready":false,…}` body) and
            // may also write to stderr. Preferring stderr alone threw away the
            // target's own reason and reported an empty message.
            let detail = {
                let e = err.trim();
                let o = out.trim();
                if !o.is_empty() && serde_json::from_str::<serde_json::Value>(o).is_ok() {
                    o.to_string()
                } else if !e.is_empty() {
                    format!("{e} {}", crate::runner::truncate(o, 200))
                } else {
                    crate::runner::truncate(o, 400)
                }
            };
            bail!(
                "deploy target `{}` ({verb}) exited {code}: {}",
                spec.scheme,
                crate::runner::truncate(detail.trim(), 600)
            );
        }
        Ok(out)
    }

    /// Ask whether the target can run here at all.
    pub fn probe(&self, spec: &TargetSpec, req: &Request) -> Probe {
        match self.invoke(spec, "probe", req) {
            Ok(out) => {
                let trimmed = out.trim();
                // A target that prints nothing is assumed ready: the verb is
                // optional, and requiring it would make every target longer.
                if trimmed.is_empty() {
                    return Probe {
                        ready: true,
                        detail: "ready (no probe output)".into(),
                    };
                }
                match serde_json::from_str::<Probe>(trimmed) {
                    Ok(p) => p,
                    Err(_) => Probe {
                        ready: true,
                        detail: crate::runner::truncate(trimmed, 200),
                    },
                }
            }
            Err(e) => Probe {
                ready: false,
                detail: e.to_string(),
            },
        }
    }

    /// What would run, without running it.
    pub fn plan(&self, spec: &TargetSpec, req: &Request) -> Plan {
        match self.invoke(spec, "plan", req) {
            Ok(out) => {
                let trimmed = out.trim();
                if trimmed.is_empty() {
                    return Plan {
                        commands: vec![format!(
                            "{} {} deploy   # {scheme}://{host}",
                            spec.command,
                            spec.args.join(" "),
                            scheme = spec.scheme,
                            host = req.host
                        )],
                        url: self.url_for(spec, req),
                    };
                }
                match serde_json::from_str::<Plan>(trimmed) {
                    Ok(mut p) => {
                        if p.url.is_empty() {
                            p.url = self.url_for(spec, req);
                        }
                        p
                    }
                    Err(_) => Plan {
                        commands: trimmed.lines().map(|l| l.to_string()).collect(),
                        url: self.url_for(spec, req),
                    },
                }
            }
            Err(e) => Plan {
                commands: vec![format!("# plan unavailable: {e}")],
                url: self.url_for(spec, req),
            },
        }
    }

    /// Do it.
    pub fn deploy(&self, spec: &TargetSpec, req: &Request) -> Result<Vec<String>> {
        let probe = self.probe(spec, req);
        if !probe.ready {
            let mut msg = format!(
                "deploy target `{}` is not ready: {}",
                spec.scheme, probe.detail
            );
            if let Some(d) = &spec.docs {
                msg.push_str(&format!("  (see {d})"));
            }
            bail!(msg);
        }
        let out = self.invoke(spec, "deploy", req)?;
        let lines: Vec<String> = out
            .lines()
            .map(|l| l.trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        Ok(lines)
    }

    fn url_for(&self, spec: &TargetSpec, req: &Request) -> String {
        match &spec.url_template {
            Some(t) => t
                .replace("{host}", &req.host)
                .replace("{port}", &req.port.to_string())
                .replace("{scheme}", &spec.scheme),
            None => String::new(),
        }
    }
}

/// Run a helper with a body on stdin, in a chosen directory, with a deadline.
fn run_with_stdin(
    cmd: &str,
    args: &[String],
    input: &str,
    timeout: Duration,
    cwd: Option<&Path>,
    env: &[(String, String)],
) -> Result<(i32, String, String)> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    // A script is only directly executable when its mode says so; a 0644 script
    // with a shebang fails with EACCES, which reads as "the target is broken".
    let (program, full_args) = crate::runner::command_for(cmd, args);
    let mut builder = Command::new(&program);
    builder
        .args(&full_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        builder.env(k, v);
    }
    if let Some(d) = cwd {
        builder.current_dir(d);
    }
    let mut child = builder
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;
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
                bail!("`{program}` exceeded {}s", timeout.as_secs());
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let out = child.wait_with_output()?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    ))
}

/// Install a target directory, preserving its script's mode.
pub fn install(src: &Path) -> Result<TargetSpec> {
    let spec_path = if src.is_dir() {
        src.join("target.toml")
    } else {
        src.to_path_buf()
    };
    let text = std::fs::read_to_string(&spec_path)
        .with_context(|| format!("reading {}", spec_path.display()))?;
    let spec: TargetSpec =
        toml::from_str(&text).with_context(|| format!("parsing {}", spec_path.display()))?;
    if spec.scheme.is_empty() {
        bail!("{}: `scheme` is required", spec_path.display());
    }
    let dest = Targets::default_dir().dir().join(&spec.scheme);
    std::fs::create_dir_all(&dest)?;
    std::fs::write(dest.join("target.toml"), &text)?;
    if let Some(parent) = spec_path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() || p.file_name().map(|f| f == "target.toml").unwrap_or(false) {
                    continue;
                }
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                let _ = std::fs::copy(&p, dest.join(&name));
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&p) {
                        let _ = std::fs::set_permissions(
                            dest.join(&name),
                            std::fs::Permissions::from_mode(meta.permissions().mode()),
                        );
                    }
                }
            }
        }
    }
    Ok(spec)
}

pub fn remove(scheme: &str) -> Result<PathBuf> {
    let dir = Targets::default_dir().dir().join(scheme);
    if !dir.exists() {
        bail!(
            "no deploy target named `{scheme}` in {}",
            Targets::default_dir().dir().display()
        );
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(dir)
}

/// A worked example, written by `fleet targets example`: a target that
/// deploys over ssh by streaming the binary — the same thing the built-in ssh
/// target does, shown as a plugin so the contract is copyable.
pub const EXAMPLE_SPEC: &str = r#"# A deploy target: one line to any server, defined by a script.
#
#   fleet deploy --to myssh://root@box --run
#
# Install it with `fleet targets add ./myssh`, then it is live on the
# next deploy. Nothing is compiled in.
scheme = "myssh"
description = "stream the binary over ssh and start it with systemd"
command = "sh"
args = ["deploy.sh"]
example = "fleet deploy --to myssh://root@your-host --run"
url_template = "http://{host}:{port}/health"
docs = "https://github.com/evergreentechnologywy/fleet/blob/main/docs/DEPLOY.md"
timeout_s = 900
enabled = true
"#;

/// The example script. Reads the request on stdin, answers per verb.
pub const EXAMPLE_SCRIPT: &str = r#"#!/bin/sh
# Verbs: probe | plan | deploy. The request arrives as JSON on stdin:
#   {scheme, host, port, prefix, binary, bundle, env}
#
# `probe` may print {"ready":bool,"detail":"…"} and should exit non-zero when it
# is not usable. `plan` prints the commands a dry run shows, and is NEVER allowed
# to have side effects. `deploy` does the work; a non-zero exit means failure.
verb="$1"
req=$(cat)

json() { # json <field>  -- read a field without requiring jq, string or number
  v=$(printf '%s' "$req" | sed -n "s/.*\"$1\":\"\([^\"]*\)\".*/\1/p" | head -1)
  [ -n "$v" ] || v=$(printf '%s' "$req" | sed -n "s/.*\"$1\":\([0-9][0-9]*\).*/\1/p" | head -1)
  printf '%s' "$v"
}

host=$(json host)
port=$(json port)
binary=$(json binary)
bundle=$(json bundle)
prefix=$(json prefix)
[ -n "$prefix" ] || prefix=/usr/local

case "$verb" in
  probe)
    if ! command -v ssh >/dev/null 2>&1; then
      echo '{"ready":false,"detail":"ssh is not installed"}'
      exit 1
    fi
    if ! ssh -o BatchMode=yes -o ConnectTimeout=8 "$host" 'echo ok' >/dev/null 2>&1; then
      echo "{\"ready\":false,\"detail\":\"cannot reach $host over ssh\"}"
      exit 1
    fi
    echo '{"ready":true,"detail":"ssh reachable"}'
    ;;

  plan)
    # No side effects here: this is what --dry-run prints.
    cat <<EOF
{"commands":[
  "scp $binary $host:/tmp/fleet-deploy/fleet",
  "scp $bundle $host:/tmp/fleet-deploy/",
  "ssh $host 'sh /tmp/fleet-deploy/install.sh --prefix $prefix --port $port'",
  "ssh $host 'systemctl enable --now fleetd'"
],
"url":"http://$host:$port/health"}
EOF
    ;;

  deploy)
    ssh "$host" 'mkdir -p /tmp/fleet-deploy'
    scp -q "$binary" "$host:/tmp/fleet-deploy/fleet" || { echo "upload failed" >&2; exit 1; }
    [ -f "$bundle" ] && scp -q "$bundle" "$host:/tmp/fleet-deploy/" 2>/dev/null
    ssh "$host" "install -m 0755 /tmp/fleet-deploy/fleet $prefix/bin/fleet && $prefix/bin/fleet --version" || exit 1
    echo "deployed to $host:$port"
    ;;
esac
"#;
