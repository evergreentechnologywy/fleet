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

        let mut order = vec![Transport::Tailscale, Transport::Ssh, Transport::Reason];
        if let Some(r) = remembered {
            if let Some(pos) = order.iter().position(|t| t.as_str() == r) {
                order.swap(0, pos);
            }
        }
        for t in order {
            if self.try_transport(peer, t, "true") {
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

    /// Run one command over one transport.
    fn try_transport(&self, peer: &Peer, t: Transport, remote_cmd: &str) -> bool {
        let (cmd, args): (String, Vec<String>) = match t {
            Transport::Tailscale => (
                "ssh".into(),
                vec![
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "ConnectTimeout=6".into(),
                    peer.host.clone(),
                    remote_cmd.to_string(),
                ],
            ),
            Transport::Ssh => {
                let mut a = vec![
                    "-o".into(),
                    "BatchMode=yes".into(),
                    "-o".into(),
                    "StrictHostKeyChecking=accept-new".into(),
                    "-o".into(),
                    "ConnectTimeout=6".into(),
                ];
                a.extend(peer.ssh_args.clone());
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
        let remote = format!(
            "printf %s '{payload}' | base64 -d >> ~/.fleet/inbox.jsonl && printf '\\n' >> ~/.fleet/inbox.jsonl"
        );
        let (cmd, args): (String, Vec<String>) = match t {
            Transport::Tailscale | Transport::Ssh => {
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
        let t = self.reach(peer, mem);
        if t == Transport::None {
            bail!("peer `{}` is unreachable", peer.name);
        }
        let payload = base64_encode(task.as_bytes());
        let remote = format!(
            "echo '{payload}' | base64 -d > /tmp/fleet-task.$$ && fleet run --harness {harness} --no-brief \"$(cat /tmp/fleet-task.$$)\"; rm -f /tmp/fleet-task.$$"
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
    pub fn email(&self, to: &str, subject: &str, body: &str) -> Result<String> {
        let sender = crate::runner::which("send_message")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/opt/fleet/scripts/send_message.py".to_string());
        if !std::path::Path::new(&sender).exists() {
            bail!("no sender installed (looked for `send_message` and {sender})");
        }
        let args = vec![
            "--to".to_string(),
            to.to_string(),
            "--subject".to_string(),
            subject.to_string(),
            "--body".to_string(),
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
        let t = self.reach(peer, mem);
        if t == Transport::None {
            bail!("peer `{}` is unreachable", peer.name);
        }
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
        a.push("tail -n 50 ~/.fleet/inbox.jsonl 2>/dev/null || true".into());
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
                        let host = p
                            .get("DNSName")
                            .and_then(|d| d.as_str())
                            .map(|d| d.trim_end_matches('.').to_string())
                            .or_else(|| {
                                p.get("HostName")
                                    .and_then(|h| h.as_str())
                                    .map(|s| s.to_string())
                            });
                        let online = p.get("Online").and_then(|o| o.as_bool()).unwrap_or(false);
                        if let Some(h) = host {
                            out.push((
                                h.split('.').next().unwrap_or(&h).to_string(),
                                h,
                                if online {
                                    Transport::Tailscale
                                } else {
                                    Transport::None
                                },
                            ));
                        }
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
