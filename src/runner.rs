//! Process and HTTP execution: one call to one harness.
//!
//! Everything a run does to the outside world goes through here, so there is a
//! single place where timeouts, environment, working directory, output capture
//! and cost accounting are decided.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Capture {
    pub ok: bool,
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub ms: u128,
}

/// Result of one harness call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub harness: String,
    pub model: String,
    pub effort: String,
    pub ok: bool,
    pub output: String,
    pub error: String,
    pub ms: u128,
    /// The chooser's reasoning, kept so a run can explain itself afterwards.
    pub route_reason: String,
    /// Tokens the provider reported for this call, when it reported any.
    ///
    /// 0 means "not reported", which is the honest value for a CLI harness: the
    /// usage lives inside the child's own output format and varies per CLI. A
    /// direct or HTTP call gets real numbers from the response body.
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// The harness that was ASKED for, when a fallback answered instead.
    ///
    /// Without this the result is actively misleading: `--json` showed only the
    /// substitute, so a caller checking `calls[].harness == "kimi"` found nothing
    /// and a forced-harness test that only looked at the ANSWER TEXT concluded
    /// kimi worked. It never ran. Measured 2026-10-27: nine harnesses were
    /// briefly "enabled" on exactly that mistake.
    pub fell_back_from: String,
}

/// Resolve a command into something `spawn` can actually run.
///
/// Three cases, in order:
///
/// 1. an absolute or relative path to an executable file — run it directly;
/// 2. a path to a file that is NOT executable but starts with `#!` — run it
///    through the interpreter named in the shebang, so a 0644 script works;
/// 3. anything else — a bare name, left to `PATH` lookup.
pub fn command_for(cmd: &str, args: &[String]) -> (String, Vec<String>) {
    let path = std::path::Path::new(cmd);
    let is_file = path.is_file();
    if is_file {
        let executable = executable_bit(path);
        if !executable {
            if let Some(interp) = shebang(path) {
                let mut v: Vec<String> = interp;
                v.push(cmd.to_string());
                v.extend(args.iter().cloned());
                return (v.remove(0), v);
            }
        }
    }
    (cmd.to_string(), args.to_vec())
}

/// The interpreter named by a `#!` line, split on whitespace.
pub fn shebang(path: &std::path::Path) -> Option<Vec<String>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 200];
    let n = f.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    let first = head.lines().next()?;
    let rest = first.strip_prefix("#!")?.trim();
    // A `#!/usr/bin/env python3` form resolves through env, which is what a
    // portable script wants and what a spawn can run directly.
    let parts: Vec<String> = rest.split_whitespace().map(|s| s.to_string()).collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

#[cfg(unix)]
fn executable_bit(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn executable_bit(path: &std::path::Path) -> bool {
    path.is_file()
}

pub fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

/// Run a command to completion, capturing both streams, with a hard deadline.
pub fn run_capture(
    cmd: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Capture> {
    let start = Instant::now();
    // A script is only directly executable when its mode says so. A 0644 script
    // with a shebang fails with EACCES, which reads as "the chooser is broken"
    // rather than "the file lacks +x" — and, unlike a binary, it is trivially
    // runnable through its own interpreter. So resolve the interpreter first.
    let (program, full_args) = command_for(cmd, args);
    let mut c = Command::new(&program);
    c.args(&full_args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let mut child = c.spawn().with_context(|| format!("spawning `{program}`"))?;

    // Poll for completion so a hung harness cannot stall the engine forever.
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!("`{cmd}` exceeded {}s", timeout.as_secs());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child.wait()?;
    Ok(Capture {
        ok: status.success(),
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
        ms: start.elapsed().as_millis(),
    })
}

/// Run a command, feeding `input` on stdin, and hand each stdout line to
/// `on_line` as it arrives. Used for CLI harnesses so a long run is visible
/// while it happens instead of only at the end.
pub fn run_streaming(
    cmd: &str,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
    input: Option<&str>,
    timeout: Duration,
    mut on_line: impl FnMut(&str),
) -> Result<Capture> {
    let start = Instant::now();
    let mut c = Command::new(cmd);
    c.args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    let mut child = c.spawn().with_context(|| format!("spawning `{cmd}`"))?;

    if let Some(text) = input {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(text.as_bytes());
            let _ = si.flush();
            drop(si);
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("no stdout pipe"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("no stderr pipe"))?;

    let err_buf = std::sync::Arc::new(parking_lot::Mutex::new(String::new()));
    let err_thread = {
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            let mut r = BufReader::new(stderr);
            let _ = r.read_to_string(&mut s);
            *buf.lock() = s;
        })
    };

    let mut out_buf = String::new();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + timeout;
    loop {
        line.clear();
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = err_thread.join();
            bail!("`{cmd}` exceeded {}s", timeout.as_secs());
        }
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                on_line(line.trim_end_matches('\n'));
                out_buf.push_str(&line);
            }
            Err(e) => return Err(e).context("reading harness stdout"),
        }
    }
    let status = child.wait()?;
    let _ = err_thread.join();
    let stderr = err_buf.lock().clone();
    Ok(Capture {
        ok: status.success(),
        code: status.code().unwrap_or(-1),
        stdout: out_buf,
        stderr,
        ms: start.elapsed().as_millis(),
    })
}

/// Env vars every harness receives, so a harness that wants shared memory or a
/// routable model endpoint can use them without extra configuration.
pub fn base_env(
    session: &str,
    run_id: &str,
    memory_db: &Path,
    chooser_base: Option<&str>,
) -> Vec<(String, String)> {
    let mut v = vec![
        ("FLEET_SESSION".to_string(), session.to_string()),
        ("FLEET_RUN".to_string(), run_id.to_string()),
        (
            "FLEET_MEMORY_DB".to_string(),
            memory_db.to_string_lossy().to_string(),
        ),
        (
            "FLEET_HOME".to_string(),
            crate::paths::home().to_string_lossy().to_string(),
        ),
        // The delegation depth, handed to a subagent through the environment
        // because a subagent is a separate `fleet run` PROCESS — nothing in
        // memory links it to its parent. This process's own depth came in the
        // same way, so the child's is one more. Read back by the engine to bound
        // recursion; without it `subagent_depth` was dead configuration.
        (
            "FLEET_DEPTH".to_string(),
            (crate::engine::depth_from_env() + 1).to_string(),
        ),
    ];
    if let Some(b) = chooser_base {
        v.push(("FLEET_CHOOSER_BASE_URL".to_string(), b.to_string()));
        v.push(("FLEET_CHOOSER_BASE".to_string(), b.to_string()));
        v.push(("OPENAI_BASE_URL".to_string(), b.to_string()));
        v.push(("OPENAI_API_BASE".to_string(), b.to_string()));
    }
    v
}

/// Call one harness with one prompt. This is the single hinge of the engine:
/// the chooser has already decided the model, and here that decision is turned
/// into either an argv, or an HTTP request body.
pub fn call(
    harness: &crate::registry::Harness,
    prompt: &str,
    model: &str,
    effort: &str,
    cwd: &Path,
    env: &[(String, String)],
    on_line: &mut dyn FnMut(&str),
) -> Result<Called> {
    use crate::registry::Kind;
    match harness.kind {
        Kind::Cli => {
            let cmd = harness.command.clone().unwrap_or_default();
            let args = harness.argv(prompt, Some(model), Some(effort), cwd);
            let mut all = env.to_vec();
            for (k, v) in &harness.env {
                all.push((k.clone(), expand_env_value(v)));
            }
            let work_dir = harness
                .cwd
                .as_deref()
                .map(crate::paths::expand)
                .unwrap_or_else(|| cwd.to_path_buf());
            std::fs::create_dir_all(&work_dir).ok();
            let input = if harness.prompt_on_stdin {
                Some(prompt)
            } else {
                None
            };
            let cap = run_streaming(
                &cmd,
                &args,
                &all,
                Some(&work_dir),
                input,
                Duration::from_secs(harness.timeout()),
                |l| on_line(l),
            )?;
            if !cap.ok {
                bail!(
                    "`{}` exited {}: {}",
                    harness.name,
                    cap.code,
                    truncate(cap.stderr.trim(), 800)
                );
            }
            // Some CLIs print progress to stderr and the answer to stdout; if
            // stdout is empty the stderr tail is the only thing there is.
            if cap.stdout.trim().is_empty() && !cap.stderr.trim().is_empty() {
                return Ok(Called::plain(cap.stderr));
            }
            Ok(Called::plain(cap.stdout))
        }
        Kind::Http | Kind::OpenaiCompat => {
            let spec = harness
                .http
                .as_ref()
                .ok_or_else(|| anyhow!("harness `{}` has no [http]", harness.name))?;
            let url = join_url(&spec.base_url, &spec.chat_path);
            let mut body = serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
                "stream": false,
            });
            if !effort.is_empty() {
                if let Some(o) = body.as_object_mut() {
                    o.insert("reasoning_effort".into(), serde_json::json!(effort));
                }
            }
            let mut req = ureq::post(&url).timeout(Duration::from_secs(harness.timeout()));
            if let Some(h) = &spec.auth_header {
                // Precedence: env var first (so a rotation needs no file edit),
                // then a literal key, each accepted from the top level OR from
                // the `[http]` table.
                let key = harness
                    .key_env()
                    .and_then(|e| std::env::var(e).ok())
                    .filter(|k| !k.is_empty())
                    .or_else(|| harness.literal_key().map(|s| s.to_string()))
                    .ok_or_else(|| {
                        anyhow!(
                            "harness `{}` needs a key: set {}",
                            harness.name,
                            harness.key_env().unwrap_or("(no api_key_env declared)")
                        )
                    })?;
                let value = match spec.auth_scheme.as_deref() {
                    Some(s) if !s.is_empty() => format!("{s} {key}"),
                    _ => key,
                };
                req = req.set(h, &value);
            }
            let resp = req.send_json(body);
            match resp {
                Ok(r) => {
                    let text = r.into_string()?;
                    let msg = extract_message(&text)?;
                    let (tin, tout) = extract_usage(&text);
                    Ok(Called {
                        text: msg,
                        tokens_in: tin,
                        tokens_out: tout,
                    })
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    bail!(
                        "harness `{}` HTTP {}: {}",
                        harness.name,
                        code,
                        truncate(&body, 600)
                    )
                }
                Err(e) => Err(anyhow!("harness `{}`: {e}", harness.name)),
            }
        }
        Kind::Agui => {
            let out = crate::agui::call(harness, prompt, on_line)?;
            Ok(Called {
                text: out.text,
                tokens_in: out.tokens_in,
                tokens_out: out.tokens_out,
            })
        }
    }
}

/// The result of one harness call: the text, plus whatever usage the transport
/// could see.
///
/// A CLI harness reports no usage, so 0 is not "free" — it is "unknown", and the
/// ledger says so rather than inventing a number.
pub struct Called {
    pub text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl Called {
    fn plain(text: String) -> Called {
        Called {
            text,
            tokens_in: 0,
            tokens_out: 0,
        }
    }
}

/// Token counts from an [OI]-compatible response body, as (prompt, completion).
///
/// Providers disagree on where this lives (`prompt_tokens` vs `input_tokens`,
/// `completion_tokens` vs `output_tokens`) and a missing block is normal for
/// streaming and for some gateways. Returning (0,0) rather than failing keeps
/// cost accounting a best-effort extra instead of a new way for a good call to
/// fail.
pub fn extract_usage(body: &str) -> (u64, u64) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return (0, 0);
    };
    let num = |ptr: &str| v.pointer(ptr).and_then(|x| x.as_u64()).unwrap_or(0);
    (
        num("/usage/prompt_tokens").max(num("/usage/input_tokens")),
        num("/usage/completion_tokens").max(num("/usage/output_tokens")),
    )
}
/// Pull the assistant text out of an OpenAI-compatible response without
/// insisting on the exact shape: `choices[0].message.content`, then any
/// `content` string, then `text`.
pub fn extract_message(body: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(body).context("provider returned non-JSON")?;
    if let Some(s) = v
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_str())
    {
        return Ok(s.to_string());
    }
    if let Some(arr) = v
        .pointer("/choices/0/message/content")
        .and_then(|x| x.as_array())
    {
        let joined: String = arr
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("");
        if !joined.is_empty() {
            return Ok(joined);
        }
    }
    if let Some(s) = v.pointer("/choices/0/text").and_then(|x| x.as_str()) {
        return Ok(s.to_string());
    }
    if let Some(s) = v.get("output_text").and_then(|x| x.as_str()) {
        return Ok(s.to_string());
    }
    bail!("no message in provider response: {}", truncate(body, 400))
}

/// A key may be written as `env:NAME` to keep descriptors commit-safe.
fn expand_env_value(v: &str) -> String {
    if let Some(name) = v.strip_prefix("env:") {
        std::env::var(name).unwrap_or_default()
    } else {
        v.to_string()
    }
}

/// Probe whether a harness can run right now: binary present for a CLI, socket
/// answering for an HTTP harness. Reported by `fleet harness list`.
pub fn probe(h: &crate::registry::Harness) -> (bool, String) {
    use crate::registry::Kind;
    match h.kind {
        Kind::Cli => {
            let cmd = h.command.clone().unwrap_or_default();
            if cmd.is_empty() {
                return (false, "no command".into());
            }
            let path = PathBuf::from(&cmd);
            let found = if path.is_absolute() {
                path.exists()
            } else {
                which(&cmd).is_some()
            };
            if found {
                (true, "on PATH".into())
            } else {
                (false, format!("`{cmd}` not on PATH"))
            }
        }
        Kind::Http | Kind::OpenaiCompat => {
            let url = h
                .http
                .as_ref()
                .map(|s| s.base_url.clone())
                .unwrap_or_default();
            match ureq::get(&url).timeout(Duration::from_secs(3)).call() {
                Ok(_) => (true, "listening".into()),
                Err(ureq::Error::Status(code, _)) => (true, format!("listening (HTTP {code})")),
                Err(e) => (false, format!("unreachable: {e}")),
            }
        }
        Kind::Agui => {
            // AG-UI backends are asked for /health, the same liveness probe this
            // engine exposes. A backend that does not implement it still answers
            // something, and any HTTP answer proves the socket is alive.
            let base = h
                .http
                .as_ref()
                .map(|s| s.base_url.clone())
                .unwrap_or_default();
            let url = join_url(&base, "/health");
            match ureq::get(&url).timeout(Duration::from_secs(3)).call() {
                Ok(_) => (true, "AG-UI listening".into()),
                Err(ureq::Error::Status(code, _)) => (true, format!("AG-UI listening (HTTP {code})")),
                Err(e) => (false, format!("unreachable: {e}")),
            }
        }
    }
}

/// Minimal `which`, so the engine does not depend on a shell being present.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Join a base URL and a path without doubling a shared prefix.
///
/// The common shape is a base that already ends in `/v1` and a path that starts
/// with `/v1/chat/completions`. Naive concatenation produces
/// `/v1/v1/chat/completions`, which every provider rejects with a 404 that reads
/// like a wrong host instead of a wrong join.
pub fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if let Some((_, tail)) = base.split_once("/v1") {
        let _ = tail;
        if let Some(rest) = path.strip_prefix("v1/") {
            return format!("{base}/{rest}");
        }
    }
    format!("{base}/{path}")
}

#[cfg(test)]
mod tests {
    use super::join_url;

    #[test]
    fn url_join_does_not_double_v1() {
        assert_eq!(
            join_url("https://api.example.ai/v1", "/v1/chat/completions"),
            "https://api.example.ai/v1/chat/completions"
        );
        assert_eq!(
            join_url("https://api.example.ai/v1/", "v1/chat/completions"),
            "https://api.example.ai/v1/chat/completions"
        );
        // A base with no /v1 keeps the path exactly as written.
        assert_eq!(
            join_url("http://127.0.0.1:8792", "/v1/chat/completions"),
            "http://127.0.0.1:8792/v1/chat/completions"
        );
        // A non-default path still lands under the base.
        assert_eq!(
            join_url("https://api.example.ai/v1", "/chat"),
            "https://api.example.ai/v1/chat"
        );
    }

    #[test]
    fn message_extraction_accepts_common_shapes() {
        let a = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
        assert_eq!(super::extract_message(a).unwrap(), "hi");
        let b = r#"{"choices":[{"text":"second"}]}"#;
        assert_eq!(super::extract_message(b).unwrap(), "second");
        let c = r#"{"choices":[{"message":{"content":[{"text":"a"},{"text":"b"}]}}]}"#;
        assert_eq!(super::extract_message(c).unwrap(), "ab");
    }
}
