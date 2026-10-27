//! Coordination between entities: peer fleet nodes and the harnesses on them.
//!
//! Three transports, tried in order, because a fleet is rarely reachable by
//! exactly one of them:
//!
//! 1. **Tailscale SSH** — `ssh <node>` where the tailnet ACL already authorises
//!    it. No key management by this engine: the tailnet is the credential.
//! 2. **Plain SSH** — `ssh -o BatchMode=yes <host>` where a host alias exists in
//!    `~/.ssh/config`. Key-based only; a password prompt is treated as
//!    unreachable, because this runs without a terminal.
//! 3. **Reason Network** — `reason send --peer <id>` when the fleet's iroh-based
//!    transport is installed, for a node that is a Reason peer but not a
//!    Tailscale node.
//!
//! Whatever works is used; what worked is remembered in shared memory under
//! `peers/<name>`, so the next call starts with the transport that succeeded and
//! the first two probes are skipped.
//!
//! **Inter-agent email** is the asynchronous channel: when no transport reaches a
//! peer, or when the message does not need an answer now, it is handed to the
//! local sender (`send_message.py`, inkbox) which the fleet already uses. The
//! message is still recorded in the ledger, so "sent" and "arrived" are
//! distinguishable rather than assumed.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    Tailscale,
    Ssh,
    Reason,
    None,
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Tailscale => "tailscale",
            Transport::Ssh => "ssh",
            Transport::Reason => "reason",
            Transport::None => "none",
        }
    }
}

/// Which shell family a peer runs, learned once and remembered.
///
/// A fleet is heterogeneous: a Windows desk answers `ssh` with `cmd.exe`, where
/// `command -v`, `printf`, `tail` and `base64 -d` do not exist, while a Linux or
/// macOS peer has all of them. Guessing produces "… is not recognized as an
/// internal or external command", which reads as a broken transport rather than
/// as the wrong shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Posix,
    Windows,
}

impl Shell {
    pub fn as_str(&self) -> &'static str {
        match self {
            Shell::Posix => "posix",
            Shell::Windows => "windows",
        }
    }
}

/// `echo %OS%` is the one discriminator both shells answer: cmd.exe expands it
/// to `Windows_NT`, and a POSIX shell leaves it as the literal `%OS%`.
fn detect_shell(peer: &Peer) -> Shell {
    let mut a = ssh_base(peer, 8);
    a.push(peer.host.clone());
    a.push("echo %OS%".into());
    match crate::runner::run_capture("ssh", &a, &[], None, Duration::from_secs(20)) {
        Ok(c) if c.stdout.contains("Windows_NT") => Shell::Windows,
        _ => Shell::Posix,
    }
}

/// Build the command that appends one base64 payload as a line of a file, for a
/// peer whose shell family is already known.
///
/// The two families have disjoint toolchains — `printf`, `base64 -d` and `$HOME`
/// on one; `certutil` and `%USERPROFILE%` on the other — and a fleet contains
/// both. Branching on a shell we detected once is the only version that works in
/// `cmd.exe`, where even `command -v` does not exist.
pub fn append_command(shell: Shell, payload: &str) -> String {
    match shell {
        Shell::Posix => format!(
            "mkdir -p \"$HOME/.fleet\" && echo '{payload}' | base64 -d >> \"$HOME/.fleet/inbox.jsonl\" && echo >> \"$HOME/.fleet/inbox.jsonl\""
        ),
        Shell::Windows => format!(
            "if not exist \"%USERPROFILE%\\.fleet\" mkdir \"%USERPROFILE%\\.fleet\" && echo {payload} > \"%TEMP%\\fleet.b64\" && certutil -decode \"%TEMP%\\fleet.b64\" \"%TEMP%\\fleet.txt\" >nul && type \"%TEMP%\\fleet.txt\" >> \"%USERPROFILE%\\.fleet\\inbox.jsonl\" && echo. >> \"%USERPROFILE%\\.fleet\\inbox.jsonl\""
        ),
    }
}

/// The identity this node uses for fleet ssh, if one is configured. Checked in
/// the environment first, then the fleet's own key path, so a node that
/// authenticates with an explicit key works without every peer descriptor
/// repeating it. Without this, `ssh` tries only the default `~/.ssh/id_*` names
/// and fails with "Permission denied (publickey)" — a message that reads as
/// "unreachable" while the host is perfectly healthy.
fn default_identity() -> Option<String> {
    if let Ok(k) = std::env::var("FLEET_SSH_KEY") {
        if !k.is_empty() {
            return Some(k);
        }
    }
    for c in ["/root/.ssh/fleet_ed25519", "/root/.ssh/id_ed25519"] {
        if std::path::Path::new(c).is_file() {
            return Some(c.to_string());
        }
    }
    if let Some(h) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(h).join(".ssh/id_ed25519");
        if p.is_file() {
            return Some(p.to_string_lossy().to_string());
        }
    }
    None
}

/// Does this host answer as `user` with the fleet key? A fleet's nodes do not
/// share one login name: a Windows desk or a Mac answers as its desktop user,
/// a Linux VPS as root, and `ssh <ip>` resolves to whatever `User` the ssh
/// config's wildcard happens to name. Guessing wrong produces
/// "Permission denied (publickey)", which reads as "the peer is down".
fn probe_user(host: &str, user: &str) -> bool {
    let mut args: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
    ];
    if let Some(key) = default_identity() {
        args.push("-i".into());
        args.push(key);
    }
    // An explicit user embedded in the host (`user@host`) wins.
    let target = if host.contains('@') {
        host.to_string()
    } else {
        format!("{user}@{host}")
    };
    args.push(target);
    // `echo` is the only no-op every shell a peer might run understands:
    // `true` is a Unix binary and a Windows peer's default shell is cmd.exe,
    // where `true` is not found — which makes a healthy peer look dead.
    args.push("echo ok".into());
    crate::runner::run_capture("ssh", &args, &[], None, Duration::from_secs(12))
        .map(|c| c.ok)
        .unwrap_or(false)
}

/// Candidate login names, in the order they are tried.
fn candidate_users() -> Vec<String> {
    if let Ok(u) = std::env::var("FLEET_SSH_USER") {
        if !u.is_empty() {
            return vec![u];
        }
    }
    let mut v: Vec<String> = Vec::new();
    if let Ok(u) = std::env::var("USER") {
        v.push(u);
    }
    for u in ["everg", "root", "hermes", "ubuntu", "admin"] {
        if !v.iter().any(|x| x == u) {
            v.push(u.to_string());
        }
    }
    v
}

/// The ssh option list shared by probing and delivery, so the two can never
/// disagree about how a peer is reached.
fn ssh_base(peer: &Peer, timeout_s: u32) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        format!("ConnectTimeout={timeout_s}"),
    ];
    if let Some(key) = default_identity() {
        a.push("-i".into());
        a.push(key);
    }
    a.extend(peer.ssh_args.clone());
    a
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    /// Host as reachable, e.g. a tailnet name, an ssh alias, or a Reason peer id.
    pub host: String,
    /// Extra ssh args, e.g. `["-p", "2222"]`.
    #[serde(default)]
    pub ssh_args: Vec<String>,
    /// Set to force one transport instead of probing.
    #[serde(default)]
    pub transport: Option<Transport>,
    /// Number of parallel harness calls this peer can take.
    #[serde(default)]
    pub slots: Option<usize>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerFile {
    #[serde(default)]
    pub peer: Vec<Peer>,
}

/// The coordination layer.
pub struct Mesh {
    file: std::path::PathBuf,
    ttl: Duration,
}

impl Mesh {
    pub fn new(file: std::path::PathBuf, ttl: Duration) -> Self {
        Self { file, ttl }
    }

    pub fn default_mesh() -> Self {
        Self::new(
            crate::paths::home().join("peers.toml"),
            Duration::from_secs(120),
        )
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    pub fn peers(&self) -> Vec<Peer> {
        let Ok(text) = std::fs::read_to_string(&self.file) else {
            return Vec::new();
        };
        match toml::from_str::<PeerFile>(&text) {
            Ok(f) => f.peer,
            Err(e) => {
                eprintln!("fleet: {} invalid: {e}", self.file.display());
                Vec::new()
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<Peer> {
        self.peers().into_iter().find(|p| p.name == name)
    }

    pub fn put(&self, p: &Peer) -> Result<()> {
        let mut all = self.peers();
        all.retain(|x| x.name != p.name);
        all.push(p.clone());
        self.write(&all)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        let mut all = self.peers();
        let before = all.len();
        all.retain(|x| x.name != name);
        if all.len() == before {
            bail!("no peer named `{name}`");
        }
        self.write(&all)
    }

    fn write(&self, all: &[Peer]) -> Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = PeerFile { peer: all.to_vec() };
        let body = format!(
            "# Peers this fleet coordinates with.\n\
             # The transport is PROBED, in the order tailscale -> ssh -> reason, and\n\
             # the one that worked is remembered. Pin one with `transport = \"ssh\"`.\n\
             #\n\
             # [[peer]]\n\
             # name = \"desk\"\n\
             # host = \"zenbook\"\n\
             # slots = 4\n\n{}",
            toml::to_string_pretty(&f)?
        );
        let tmp = self.file.with_extension("toml.tmp");
        std::fs::write(&tmp, &body)?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }

    /// Probe one peer and remember what worked.
    pub fn reach(&self, peer: &Peer, mem: &crate::memory::Memory) -> Transport {
        if let Some(t) = peer.transport {
            return t;
        }
        // A remembered transport is used first, then re-verified cheaply.
        let remembered = mem
            .get("peers", &peer.name)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .and_then(|v| {
                v.get("transport")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            });

        // A remembered `user@host` is used directly: the login name is a
        // property of the peer, learned once rather than re-guessed per call.
        let remembered_host = mem
            .get("peers", &peer.name)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .and_then(|v| {
                v.get("ssh_target")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            });

        if let Some(target) = remembered_host {
            let mut p = peer.clone();
            p.host = target.clone();
            if self.try_transport(&p, Transport::Ssh, "echo ok") {
                return Transport::Ssh;
            }
        }

        // Resolve the login name before declaring the peer unreachable. A node
        // that answers as `everg@` but not `root@` is reachable; the first
        // attempt simply had the wrong user.
        if !peer.host.contains('@') {
            for user in candidate_users() {
                if probe_user(&peer.host, &user) {
                    let target = format!("{user}@{}", peer.host);
                    let shell = {
                        let mut p2 = peer.clone();
                        p2.host = target.clone();
                        detect_shell(&p2)
                    };
                    let rec = serde_json::json!({
                        "transport": "ssh",
                        "host": peer.host,
                        "ssh_target": target,
                        "shell": shell.as_str(),
                        "checked": crate::now(),
                    });
                    let _ = mem.put("peers", &peer.name, &rec.to_string(), "fleet");
                    return Transport::Ssh;
                }
            }
        }

        let mut order = vec![Transport::Tailscale, Transport::Ssh, Transport::Reason];
        if let Some(r) = remembered {
            if let Some(pos) = order.iter().position(|t| t.as_str() == r) {
                order.swap(0, pos);
            }
        }
        for t in order {
            if self.try_transport(peer, t, "echo ok") {
                let rec = serde_json::json!({
                    "transport": t.as_str(),
                    "host": peer.host,
                    "checked": crate::now(),
                });
                let _ = mem.put("peers", &peer.name, &rec.to_string(), "fleet");
                return t;
            }
        }
        Transport::None
    }

    /// A peer with the remembered `user@host`, if one was learned. Cheap to
    /// call: it reads shared memory, not the network.
    /// The shell family a peer runs. Learned once and remembered, because the
    /// test costs an ssh round trip; a record written before the shell was
    /// tracked has no `shell` field, and is re-learned rather than assumed —
    /// assuming POSIX for a Windows peer is exactly the bug this exists to fix.
    pub fn shell_of(&self, peer: &Peer, mem: &crate::memory::Memory) -> Shell {
        let remembered = mem
            .get("peers", &peer.name)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .and_then(|v| {
                v.get("shell")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
            });
        if let Some(s) = remembered {
            return if s == "windows" {
                Shell::Windows
            } else {
                Shell::Posix
            };
        }
        let detected = detect_shell(peer);
        let mut rec = mem
            .get("peers", &peer.name)
            .ok()
            .flatten()
            .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
            .unwrap_or_else(|| serde_json::json!({}));
        if let Some(o) = rec.as_object_mut() {
            o.insert("shell".into(), serde_json::json!(detected.as_str()));
            o.insert("checked".into(), serde_json::json!(crate::now()));
        }
        let _ = mem.put("peers", &peer.name, &rec.to_string(), "fleet");
        detected
    }

    pub fn resolved(&self, peer: &Peer, mem: &crate::memory::Memory) -> Peer {
        let mut out = peer.clone();
        if out.host.contains('@') {
            return out;
        }
        if let Ok(Some(v)) = mem.get("peers", &peer.name) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&v) {
                if let Some(t) = j.get("ssh_target").and_then(|x| x.as_str()) {
                    out.host = t.to_string();
                }
            }
        }
        out
    }

    /// Run one command over one transport.
    fn try_transport(&self, peer: &Peer, t: Transport, remote_cmd: &str) -> bool {
        let (cmd, args): (String, Vec<String>) = match t {
            Transport::Tailscale | Transport::Ssh => {
                let mut a = ssh_base(peer, 6);
                a.push(peer.host.clone());
                a.push(remote_cmd.to_string());
                ("ssh".into(), a)
            }
            Transport::Reason => (
                "reason".into(),
                vec![
                    "exec".into(),
                    "--peer".into(),
                    peer.host.clone(),
                    "--".into(),
                    "sh".into(),
                    "-c".into(),
                    remote_cmd.to_string(),
                ],
            ),
            Transport::None => return false,
        };
        match crate::runner::run_capture(&cmd, &args, &[], None, Duration::from_secs(20)) {
            Ok(c) => c.ok,
            Err(_) => false,
        }
    }

    /// Send one message to a peer over whichever transport reaches it.
    pub fn send(
        &self,
        peer: &Peer,
        message: &str,
        mem: &crate::memory::Memory,
    ) -> Result<Transport> {
        let t = self.reach(peer, mem);
        if t == Transport::None {
            bail!(
                "no transport reaches `{}` ({}): tailscale ssh, plain ssh and reason all failed",
                peer.name,
                peer.host
            );
        }
        // The message is delivered as a base64 payload to avoid every quoting
        // problem a shell would otherwise introduce.
        let payload = base64_encode(message.as_bytes());
        // The peer's shell family was learned when the peer was first reached,
        // so the command is built for it instead of trying to detect it there.
        let shell = self.shell_of(peer, mem);
        let remote = append_command(shell, &payload);
        let (cmd, args): (String, Vec<String>) = match t {
            Transport::Tailscale | Transport::Ssh => {
                let mut a = ssh_base(peer, 8);
                a.push(peer.host.clone());
                a.push(remote);
                ("ssh".into(), a)
            }
            Transport::Reason => (
                "reason".into(),
                vec![
                    "exec".into(),
                    "--peer".into(),
                    peer.host.clone(),
                    "--".into(),
                    "sh".into(),
                    "-c".into(),
                    remote,
                ],
            ),
            Transport::None => unreachable!(),
        };
        let c = crate::runner::run_capture(&cmd, &args, &[], None, Duration::from_secs(30))?;
        if !c.ok {
            bail!(
                "delivery over {} to `{}` failed: {}",
                t.as_str(),
                peer.name,
                crate::runner::truncate(c.stderr.trim(), 300)
            );
        }
        Ok(t)
    }

    /// Run one harness on a peer and bring the answer back. This is how a task
    /// reaches a harness that only exists on another node.
    pub fn run_remote(
        &self,
        peer: &Peer,
        harness: &str,
        task: &str,
        mem: &crate::memory::Memory,
    ) -> Result<String> {
        let peer = &self.resolved(peer, mem);
        let t = self.reach(peer, mem);
        if t == Transport::None {
            bail!("peer `{}` is unreachable", peer.name);
        }
        let payload = base64_encode(task.as_bytes());
        // The task crosses as base64 and is decoded on the peer, again choosing
        // the toolchain that exists rather than assuming a POSIX shell.
        let remote = format!(
            "if command -v base64 >/dev/null 2>&1; then \
               echo '{payload}' | base64 -d > /tmp/fleet-task && fleet run --harness {harness} \"$(cat /tmp/fleet-task)\"; rm -f /tmp/fleet-task; \
             else \
               echo '{payload}' > \"%TEMP%\\fleet-task.b64\" && certutil -decode \"%TEMP%\\fleet-task.b64\" \"%TEMP%\\fleet-task.txt\" >/dev/null && fleet run --harness {harness} \"%TEMP%\\fleet-task.txt\"; \
             fi"
        );
        let mut a = vec![
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=8".into(),
        ];
        if t == Transport::Ssh {
            a.extend(peer.ssh_args.clone());
        }
        a.push(peer.host.clone());
        a.push(remote);
        let c = crate::runner::run_capture("ssh", &a, &[], None, Duration::from_secs(1800))?;
        if !c.ok {
            bail!(
                "remote harness `{harness}` on `{}` exited {}: {}",
                peer.name,
                c.code,
                crate::runner::truncate(c.stderr.trim(), 400)
            );
        }
        Ok(c.stdout)
    }

    /// The asynchronous channel: email the fleet's own agent address. Used when
    /// a peer is unreachable or when an answer is not needed now.
    ///
    /// The sender is the fleet's existing `send_message` tool, whose interface is
    /// a CHANNEL positional plus `--text`, not a `--body` flag. Getting that
    /// wrong is silent: the tool exits non-zero with a usage message, which
    /// reads as a delivery failure rather than as a wrong invocation.
    pub fn email(&self, to: &str, subject: &str, body: &str) -> Result<String> {
        let sender = crate::runner::which("send_message")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/opt/fleet/scripts/send_message.py".to_string());
        if !std::path::Path::new(&sender).exists() {
            bail!("no sender installed (looked for `send_message` and {sender})");
        }
        let args = vec![
            "email".to_string(),
            "--to".to_string(),
            to.to_string(),
            "--subject".to_string(),
            subject.to_string(),
            "--text".to_string(),
            body.to_string(),
        ];
        let c = crate::runner::run_capture(&sender, &args, &[], None, Duration::from_secs(60))?;
        if !c.ok {
            bail!(
                "email send failed ({}): {}",
                c.code,
                crate::runner::truncate(c.stderr.trim(), 300)
            );
        }
        Ok(c.stdout)
    }

    /// Read a peer's inbox file over ssh, for `fleet peers inbox`.
    pub fn inbox(&self, peer: &Peer, mem: &crate::memory::Memory) -> Result<String> {
        let peer = &self.resolved(peer, mem);
        let t = self.reach(peer, mem);
        if t == Transport::None {
            bail!("peer `{}` is unreachable", peer.name);
        }
        let mut a = ssh_base(peer, 8);
        a.push(peer.host.clone());
        // Same heterogeneity as delivery: built for the peer's own shell, not
        // guessed with a `||` chain, which is itself POSIX syntax.
        let remote = match self.shell_of(peer, mem) {
            Shell::Posix => {
                "tail -n 50 \"$HOME/.fleet/inbox.jsonl\" 2>/dev/null || true".to_string()
            }
            Shell::Windows => "type \"%USERPROFILE%\\.fleet\\inbox.jsonl\"".to_string(),
        };
        a.push(remote);
        let c = crate::runner::run_capture("ssh", &a, &[], None, Duration::from_secs(30))?;
        Ok(c.stdout)
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

/// Minimal base64 so the engine carries no extra dependency for one function.
pub fn base64_encode(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Can we actually open an ssh session to this address? A tailnet can have a
/// device online whose sshd is dark, and the ACL can deny ssh to a device that
/// would otherwise answer — so the tailnet's own "online" flag is not evidence
/// that a peer is usable.
fn probe_ssh(host: &str) -> bool {
    let args: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ConnectTimeout=5".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        host.to_string(),
        "echo ok".into(),
    ];
    crate::runner::run_capture("ssh", &args, &[], None, Duration::from_secs(12))
        .map(|c| c.ok)
        .unwrap_or(false)
}

/// Detect peer candidates from what is actually installed and configured, so
/// onboarding can offer them instead of asking the operator to know.
pub fn detect_candidates() -> Vec<(String, String, Transport)> {
    let mut out = Vec::new();
    if crate::runner::which("tailscale").is_some() {
        if let Ok(c) = crate::runner::run_capture(
            "tailscale",
            &["status".into(), "--json".into()],
            &[],
            None,
            Duration::from_secs(10),
        ) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&c.stdout) {
                if let Some(peers) = v.get("Peer").and_then(|p| p.as_object()) {
                    for (_k, p) in peers {
                        let name = p
                            .get("HostName")
                            .and_then(|h| h.as_str())
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            continue;
                        }
                        let online = p.get("Online").and_then(|o| o.as_bool()).unwrap_or(false);
                        // The IP, not the MagicDNS name. A tailnet's DNS name
                        // frequently does NOT resolve from a Linux node that is
                        // not using the tailnet resolver, and `ssh <name>` then
                        // fails with "Temporary failure in name resolution" —
                        // which reads as "the peer is down" while the peer is
                        // fine. The IP always works.
                        let ip = p
                            .get("TailscaleIPs")
                            .and_then(|v| v.as_array())
                            .and_then(|a| {
                                a.iter()
                                    .filter_map(|x| x.as_str())
                                    .find(|s| !s.contains(':'))
                            })
                            .map(|s| s.to_string());
                        let host = ip.clone().unwrap_or_else(|| name.clone());
                        // Reachability is not the tailnet's opinion: a device
                        // can be online with a dark sshd, and one marked
                        // offline can still answer. Probe.
                        let t = if probe_ssh(&host) {
                            Transport::Tailscale
                        } else if online {
                            Transport::Ssh
                        } else {
                            Transport::None
                        };
                        out.push((name, host, t));
                    }
                }
            }
        }
    }
    if crate::runner::which("reason").is_some() {
        if let Ok(c) = crate::runner::run_capture(
            "reason",
            &["peers".into(), "--json".into()],
            &[],
            None,
            Duration::from_secs(10),
        ) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&c.stdout) {
                if let Some(arr) = v.as_array() {
                    for p in arr {
                        if let Some(id) = p.get("id").and_then(|x| x.as_str()) {
                            let name = p
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or(id)
                                .to_string();
                            out.push((name, id.to_string(), Transport::Reason));
                        }
                    }
                }
            }
        }
    }
    out
}
