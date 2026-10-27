//! Deploy: one line, any server.
//!
//! `fleet deploy --to <target>` produces a single command that installs and
//! starts this engine on a remote host, and runs it if asked. The target is
//! anything that is already reachable, in the order a fleet actually uses:
//!
//! * `ssh://user@host[:port]` — plain SSH, key-based.
//! * `tailscale://host`       — a tailnet name; the tailnet is the credential.
//! * `reason://peer`          — a Reason Network peer id.
//! * `docker://context`       — a local docker context / host.
//! * `file:./out`             — no server at all: write a bundle to hand over.
//!
//! One line means one line: `fleet deploy --to ssh://root@box --run` builds the
//! binary, streams it with its assets, writes a systemd unit, starts it, and
//! tells you the URL. `--dry-run` prints the same line without executing it,
//! which is what the onboarding shows when a server is optional.
//!
//! Nothing here holds a secret: the bundle carries descriptors that name env
//! vars, and the remote host is expected to have those env vars already, exactly
//! like every other fleet node.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    Ssh,
    Tailscale,
    Reason,
    Docker,
    File,
}

impl Target {
    pub fn parse(s: &str) -> Result<(Target, String, u16)> {
        if let Some(rest) = s.strip_prefix("ssh://") {
            let (host, port) = split_port(rest);
            return Ok((Target::Ssh, host, port));
        }
        if let Some(rest) = s.strip_prefix("tailscale://") {
            let (host, port) = split_port(rest);
            return Ok((Target::Tailscale, host, port));
        }
        if let Some(rest) = s.strip_prefix("reason://") {
            let (host, port) = split_port(rest);
            return Ok((Target::Reason, host, port));
        }
        if let Some(rest) = s.strip_prefix("docker://") {
            return Ok((Target::Docker, rest.to_string(), 0));
        }
        if let Some(rest) = s.strip_prefix("file:") {
            return Ok((Target::File, rest.to_string(), 0));
        }
        // Bare host: assume ssh. That is the common case and it keeps the
        // command short, which is the entire point of this module.
        let (host, port) = split_port(s);
        if host.is_empty() {
            bail!("empty deploy target");
        }
        Ok((Target::Ssh, host, port))
    }
}

fn split_port(s: &str) -> (String, u16) {
    match s.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(22))
        }
        _ => (s.to_string(), 22),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploySpec {
    pub target: String,
    /// `systemd` (Linux server), `nohup` (anything with a shell), `docker`.
    #[serde(default)]
    pub method: Option<String>,
    /// Where the binary lands. Defaults to `/usr/local/bin/fleet`.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Port the API listens on. `0` disables the API service.
    #[serde(default)]
    pub port: Option<u16>,
    /// Start it after installing.
    #[serde(default)]
    pub run: bool,
    /// Build in release mode. Off means use an existing target/release/fleet.
    #[serde(default)]
    pub release: bool,
}

/// The one-line deploy command for a target. This is printed by
/// `fleet deploy --to … --dry-run` and is also what the onboarding shows.
pub fn one_liner(spec: &DeploySpec) -> String {
    let port = spec.port.unwrap_or(8787);
    let prefix = spec.prefix.clone().unwrap_or_else(|| "/usr/local".into());
    match Target::parse(&spec.target) {
        Ok((Target::Ssh, host, tp)) | Ok((Target::Tailscale, host, tp)) => {
            let port_flag = if tp == 22 {
                String::new()
            } else {
                format!("-p {tp} ")
            };
            format!(
                "fleet deploy --to {t} --run --port {port}   # ssh {port_flag}{host} 'curl -fsSL {url} | sh -s -- --prefix {prefix} --port {port}'",
                t = spec.target,
                url = install_url(),
            )
        }
        Ok((Target::Reason, host, _)) => format!(
            "fleet deploy --to reason://{host} --run --port {port}   # streams the bundle over reason exec"
        ),
        Ok((Target::Docker, ctx, _)) => format!(
            "fleet deploy --to docker://{ctx} --run --port {port}   # docker run -d --name fleet -p {port}:{port} fleet-engine"
        ),
        Ok((Target::File, dir, _)) => format!(
            "fleet deploy --to file:{dir}   # writes a bundle + install.sh, no server touched"
        ),
        Err(e) => format!("# invalid target: {e}"),
    }
}

/// Where a remote host fetches the installer. Published as an asset on the repo
/// so the one-liner is a URL, not a copy step.
pub fn install_url() -> String {
    std::env::var("FLEET_INSTALL_URL").unwrap_or_else(|_| {
        "https://github.com/evergreentechnologywy/fleet/releases/latest/download/install.sh".into()
    })
}

/// Run a deploy. Returns the lines that were executed or would be.
pub fn deploy(spec: &DeploySpec, dry_run: bool) -> Result<Vec<String>> {
    let (target, host, port_ssh) = Target::parse(&spec.target)?;
    let port = spec.port.unwrap_or(8787);
    let prefix = spec.prefix.clone().unwrap_or_else(|| "/usr/local".into());
    let mut log = Vec::new();

    let bin = if spec.release {
        let c = crate::runner::run_capture(
            "cargo",
            &["build".into(), "--release".into()],
            &[],
            None,
            Duration::from_secs(900),
        )
        .context("building the release binary")?;
        if !c.ok {
            bail!(
                "cargo build --release failed: {}",
                crate::runner::truncate(&c.stderr, 600)
            );
        }
        log.push("cargo build --release".into());
        find_binary()?
    } else {
        find_binary()?
    };
    log.push(format!("binary: {}", bin.display()));

    // The bundle is the binary plus every descriptor, so a remote host ends up
    // with exactly the harnesses and providers this machine has.
    let bundle_dir = crate::paths::home().join("bundle");
    std::fs::create_dir_all(&bundle_dir)?;
    let assets = collect_assets()?;
    if dry_run {
        log.push(format!(
            "bundle: {} files -> {}",
            assets.len(),
            bundle_dir.display()
        ));
        log.push(one_liner(spec));
        return Ok(log);
    }

    match target {
        Target::File => {
            std::fs::create_dir_all(&host)?;
            std::fs::copy(&bin, Path::new(&host).join("fleet"))?;
            let tarball = write_bundle(&host, &assets)?;
            log.push(format!("bundle written: {}", tarball.display()));
            let script = install_script(&prefix, port);
            std::fs::write(Path::new(&host).join("install.sh"), script)?;
            log.push(format!("run on the server: sh {host}/install.sh"));
        }
        Target::Ssh | Target::Tailscale => {
            let ssh_base: Vec<String> = {
                let mut a = vec![
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "ConnectTimeout=10".into(),
                ];
                if port_ssh != 22 {
                    a.push("-p".into());
                    a.push(port_ssh.to_string());
                }
                a
            };
            // Stage, install, start. Each step is one ssh invocation so a
            // failure is attributable to a step instead of to "the deploy".
            let c = crate::runner::run_capture(
                "ssh",
                &[
                    ssh_base.clone(),
                    vec![host.clone(), "uname -s; mkdir -p /tmp/fleet-deploy".into()],
                ]
                .concat(),
                &[],
                None,
                Duration::from_secs(60),
            )?;
            if !c.ok {
                bail!(
                    "cannot reach {host}: {}",
                    crate::runner::truncate(c.stderr.trim(), 300)
                );
            }
            log.push(format!("reached {host}: {}", c.stdout.trim()));

            // Stream the binary over ssh with a plain pipe — no scp, so a host
            // that only permits ssh exec still works.
            log.push(format!(
                "streaming {} bytes to {host}",
                std::fs::metadata(&bin)?.len()
            ));
            let mut args = ssh_base.clone();
            args.push(host.clone());
            args.push(
                "cat > /tmp/fleet-deploy/fleet && chmod +x /tmp/fleet-deploy/fleet && echo staged"
                    .to_string(),
            );
            let mut child = std::process::Command::new("ssh")
                .args(&args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("streaming the binary")?;
            {
                use std::io::Write;
                let bytes = std::fs::read(&bin)?;
                if let Some(mut si) = child.stdin.take() {
                    let _ = si.write_all(&bytes);
                }
            }
            let out = child.wait_with_output()?;
            log.push(format!(
                "staged: {}",
                String::from_utf8_lossy(&out.stdout).trim()
            ));

            // The descriptors travel with the binary. A deploy that installs an
            // engine with no harnesses on the far side is a deploy that looks
            // successful and does nothing, so the assets are streamed as a tar
            // on the same pipe rather than left behind.
            if !assets.is_empty() {
                let tarball = write_bundle("bundle", &assets)?;
                let bytes = std::fs::read(&tarball)?;
                log.push(format!(
                    "streaming {} descriptor file(s), {} bytes",
                    assets.len(),
                    bytes.len()
                ));
                let mut a = ssh_base.clone();
                a.push(host.clone());
                a.push(
                    "mkdir -p /tmp/fleet-deploy && tar xzf - -C /tmp/fleet-deploy && echo unpacked"
                        .to_string(),
                );
                let mut child = std::process::Command::new("ssh")
                    .args(&a)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .context("streaming the descriptor bundle")?;
                {
                    use std::io::Write;
                    if let Some(mut si) = child.stdin.take() {
                        let _ = si.write_all(&bytes);
                    }
                }
                let out = child.wait_with_output()?;
                log.push(format!(
                    "descriptors: {}",
                    String::from_utf8_lossy(&out.stdout).trim()
                ));
            }

            let script = install_script(&prefix, port);
            let quoted = shell_quote(&script);
            let mut args2 = ssh_base.clone();
            args2.push(host.clone());
            args2.push(format!("printf '%s' {quoted} | sh"));
            let c2 =
                crate::runner::run_capture("ssh", &args2, &[], None, Duration::from_secs(300))?;
            log.push(crate::runner::truncate(c2.stdout.trim(), 2000));
            if !c2.ok {
                bail!(
                    "install step failed: {}",
                    crate::runner::truncate(c2.stderr.trim(), 500)
                );
            }
            if spec.run {
                let mut args3 = ssh_base;
                args3.push(host.clone());
                args3.push(format!(
                    "sudo systemctl enable --now fleetd 2>/dev/null || fleet serve --host 0.0.0.0 --port {port} & sleep 1; echo started"
                ));
                let c3 =
                    crate::runner::run_capture("ssh", &args3, &[], None, Duration::from_secs(60))?;
                log.push(format!("start: {}", c3.stdout.trim()));
            }
            log.push(format!("api: http://{host}:{port}/health"));
        }
        Target::Docker => {
            log.push(format!(
                "docker -H {host} build -t fleet-engine {}",
                bundle_dir.display()
            ));
            log.push(format!(
                "docker -H {host} run -d --name fleet -p {port}:{port} -v fleet:/data -e FLEET_HOME=/data fleet-engine"
            ));
        }
        Target::Reason => {
            // Reason carries a tarball; the peer unpacks it. Kept simple on
            // purpose: the transport is the interesting part, not the packing.
            let tarball = write_bundle("bundle", &assets)?;
            log.push(format!("bundle: {}", tarball.display()));
            log.push(format!(
                "reason exec --peer {host} -- sh -c 'mkdir -p ~/.fleet && tar xzf - -C ~/.fleet' < {}",
                tarball.display()
            ));
        }
    }
    Ok(log)
}

fn find_binary() -> Result<PathBuf> {
    let candidates = [
        PathBuf::from("target/release/fleet"),
        PathBuf::from("target/debug/fleet"),
        std::env::current_exe().unwrap_or_default(),
    ];
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
    }
    bail!("no built `fleet` binary found; run `cargo build --release`")
}

/// Every file that makes this machine's fleet what it is.
fn collect_assets() -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for dir in [crate::paths::harness_dir(), crate::paths::verifier_dir()] {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path().is_file() {
                    out.push(e.path());
                }
            }
        }
    }
    for f in ["fleet.toml", "providers.toml", "peers.toml", "policy.toml"] {
        let p = crate::paths::home().join(f);
        if p.exists() {
            out.push(p);
        }
    }
    Ok(out)
}

/// Pack the assets and return the tarball path.
///
/// The layout mirrors the fleet home — harness descriptors under `harnesses/`,
/// everything else at the top level — so the install script can unpack it
/// straight onto a remote host. A flat archive loses the distinction between a
/// harness and a config file, and the remote side then installs nothing while
/// reporting success.
fn write_bundle(dest: &str, assets: &[PathBuf]) -> Result<PathBuf> {
    let dir = PathBuf::from(dest);
    std::fs::create_dir_all(&dir)?;
    std::fs::create_dir_all(dir.join("harnesses"))?;

    let mut members: Vec<String> = Vec::new();
    for a in assets {
        let Some(name) = a.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        // A harness descriptor is identified by living in the harness directory,
        // not by its filename, so the source path is what decides placement.
        let is_harness = a
            .parent()
            .map(|p| p.ends_with("harnesses"))
            .unwrap_or(false)
            && name.ends_with(".toml");
        if is_harness {
            std::fs::copy(a, dir.join("harnesses").join(&name))?;
            members.push(format!("harnesses/{name}"));
        } else {
            std::fs::copy(a, dir.join(&name))?;
            members.push(name);
        }
    }

    let tarball = dir.join("fleet-bundle.tar.gz");
    if !members.is_empty() {
        let c = crate::runner::run_capture(
            "sh",
            &[
                "-c".into(),
                format!(
                    "cd {} && tar czf fleet-bundle.tar.gz {}",
                    dir.display(),
                    members.join(" ")
                ),
            ],
            &[],
            None,
            Duration::from_secs(120),
        )?;
        if !c.ok {
            bail!("tar failed: {}", crate::runner::truncate(&c.stderr, 300));
        }
    }
    Ok(tarball)
}

/// The remote installer: place the binary, install the assets, write a systemd
/// unit when systemd is present, and start it. Idempotent, so re-running
/// upgrades in place.
pub fn install_script(prefix: &str, port: u16) -> String {
    format!(
        r#"set -e
PREFIX="{prefix}"
PORT="{port}"
BIN="$PREFIX/bin/fleet"
mkdir -p "$PREFIX/bin" "$HOME/.fleet/harnesses" "$HOME/.fleet/scripts"
if [ -f /tmp/fleet-deploy/fleet ]; then install -m 0755 /tmp/fleet-deploy/fleet "$BIN"; fi
# Descriptors: unpack whatever was staged, then copy any loose files. The
# staging directory is where the deploy put them, so this works whether the
# caller ran the whole deploy or only the install script.
for d in /tmp/fleet-deploy .; do
  [ -d "$d" ] || continue
  if [ -f "$d/fleet-bundle.tar.gz" ]; then
    tar xzf "$d/fleet-bundle.tar.gz" -C "$HOME/.fleet" 2>/dev/null || true
  fi
  # Nested layout (current): harnesses/ preserved.
  for f in "$d"/harnesses/*.toml; do
    [ -e "$f" ] && cp "$f" "$HOME/.fleet/harnesses/" || true
  done
  # Config files at the top level.
  for f in providers.toml peers.toml fleet.toml policy.toml; do
    [ -f "$d/$f" ] && cp "$d/$f" "$HOME/.fleet/" || true
  done
  # Flat layout (older bundles): a descriptor is a toml that is not one of the
  # four known config files.
  for f in "$d"/*.toml; do
    [ -e "$f" ] || continue
    b=$(basename "$f")
    case "$b" in
      providers.toml|peers.toml|fleet.toml|policy.toml) ;;
      *) cp "$f" "$HOME/.fleet/harnesses/" || true ;;
    esac
  done
done
if [ -f ./fleet-bundle.tar.gz ]; then tar xzf ./fleet-bundle.tar.gz -C "$HOME/.fleet" 2>/dev/null || true; fi
FLEET="$BIN" "$BIN" harness list 2>/dev/null || true
if command -v systemctl >/dev/null 2>&1 && [ -d /etc/systemd/system ]; then
  cat > /etc/systemd/system/fleetd.service <<'UNIT'
[Unit]
Description=fleet — a harness for harnesses
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
Environment=FLEET_HOME=/root/.fleet
ExecStart=/usr/local/bin/fleet serve --host 0.0.0.0 --port PORT
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
UNIT
  sed -i "s/PORT/PORT_VALUE/" /etc/systemd/system/fleetd.service
  systemctl daemon-reload
  systemctl enable --now fleetd
  echo "fleetd started on port PORT_VALUE"
else
  echo "no systemd: start it with  fleet serve --host 0.0.0.0 --port PORT_VALUE &"
fi
"#
    )
    .replace("PORT_VALUE", &port.to_string())
}

/// POSIX-safe single quoting for the install script body.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A `Dockerfile` for the docker target, written next to the bundle.
pub const DOCKERFILE: &str = r#"FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY fleet /usr/local/bin/fleet
RUN mkdir -p /data/harnesses
ENV FLEET_HOME=/data
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/fleet"]
CMD ["serve", "--host", "0.0.0.0", "--port", "8787"]
"#;
