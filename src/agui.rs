//! AG-UI harness transport — any AG-UI agent, run as a fleet harness.
//!
//! AG-UI (`ag-ui-protocol/ag-ui`, MIT, spec 1.0) is the open event protocol
//! between an agent backend and a user-facing app. Because it is a *protocol*
//! and not a vendor SDK, a harness of kind `agui` makes every AG-UI-compatible
//! backend — LangGraph, Mastra, PydanticAI, Claude Agent SDK, CrewAI, ADK, or
//! the fleet's own bridge — runnable through the fleet: the chooser decides
//! which of them runs, the ledger records the call, and the lessons loop sees
//! the outcome. Without this, those agents live outside the fleet and can only
//! be reached by whatever UI happens to own them.
//!
//! The request is one `RunAgentInput`; the response is a `text/event-stream`
//! where each event is `data: {json}` followed by a blank line. The final text
//! is the concatenation of `TEXT_MESSAGE_CONTENT` deltas, and `RUN_ERROR` is a
//! failure — not an empty answer.
//!
//! What this deliberately does NOT do: `TOOL_CALL_*` events are not routed back
//! as fleet tools. A remote AG-UI agent owns its own tools; pretending otherwise
//! would let a remote backend call into this machine's filesystem.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::registry::Harness;

/// What one AG-UI call produced.
#[derive(Debug)]
pub struct Outcome {
    pub text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    /// Steps the remote agent reported, in order. Surfaced as run progress.
    pub steps: Vec<String>,
}

fn event_text(ev: &serde_json::Value) -> Option<&str> {
    ev.get("type").and_then(|t| t.as_str())
}

/// Run one prompt against an AG-UI endpoint.
///
/// `on_line` receives a short human line per remote step, so `fleet run -v`
/// shows a remote agent working for the same reason it shows a local CLI's
/// output: a run that goes quiet for 40 seconds is indistinguishable from a
/// hung one.
pub fn call(
    harness: &Harness,
    prompt: &str,
    on_line: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let spec = harness
        .http
        .as_ref()
        .ok_or_else(|| anyhow!("AG-UI harness `{}` has no [http] base_url", harness.name))?;
    let path = if spec.chat_path.trim().is_empty() {
        "/agui"
    } else {
        spec.chat_path.as_str()
    };
    let url = crate::runner::join_url(&spec.base_url, path);

    let thread = format!("fleet-{}", crate::short_id());
    let run = format!("fleet-{}", crate::short_id());
    let body = json!({
        "threadId": thread,
        "runId": run,
        "messages": [{
            "id": format!("msg-{}", crate::short_id()),
            "role": "user",
            // A plain string is the one content shape every AG-UI producer
            // accepts; the parts[] form is optional in the spec.
            "content": prompt,
        }],
        "tools": [],
        "context": [],
        "state": {},
        // Namespaced, and NOT the harness name as a pin. A backend that answers
        // by calling this engine again (the fleet's own bridge does) would read a
        // top-level `harness` as "run me with this harness" and recurse: measured
        // 2026-10-28, one test run forked nine `fleet run` processes. The
        // identity goes under `fleet.name`, which no engine treats as an
        // instruction, and `fleet.pin_harness` stays an explicit opt-in.
        "forwardedProps": {"fleet": {"name": harness.name}},
    });

    let mut req = ureq::post(&url)
        .timeout(Duration::from_secs(harness.timeout()))
        .set("content-type", "application/json")
        .set("accept", "text/event-stream");
    if let Some(h) = &spec.auth_header {
        if let Some(key) = harness
            .key_env()
            .and_then(|e| std::env::var(e).ok())
            .filter(|k| !k.is_empty())
            .or_else(|| harness.literal_key().map(|s| s.to_string()))
        {
            let value = match spec.auth_scheme.as_deref() {
                Some(s) if !s.is_empty() => format!("{s} {key}"),
                _ => key,
            };
            req = req.set(h, &value);
        }
    }

    let resp = req.send_string(&body.to_string()).map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let detail = r.into_string().unwrap_or_default();
            anyhow!(
                "AG-UI harness `{}` answered HTTP {code}: {}",
                harness.name,
                detail.trim().chars().take(400).collect::<String>()
            )
        }
        other => anyhow!("AG-UI harness `{}` unreachable at {url}: {other}", harness.name),
    })?;

    let mut text = String::new();
    let mut steps: Vec<String> = Vec::new();
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut finished = false;
    let mut error: Option<String> = None;

    // Read the SSE stream. A producer that never sends RUN_FINISHED is a fault,
    // not an empty answer: the run may have died mid-stream and reporting ""
    // would look like a successful silent reply.
    let reader = BufReader::new(resp.into_reader());
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                if !finished && error.is_none() {
                    return Err(e).context("AG-UI stream broke before RUN_FINISHED");
                }
                break;
            }
        };
        let payload = match line.strip_prefix("data:") {
            Some(p) => p.trim(),
            None => continue, // `event:`/`id:`/comments are optional framing
        };
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let ev: serde_json::Value = match serde_json::from_str(payload) {
            Ok(v) => v,
            Err(_) => continue, // a keep-alive or a producer extension
        };
        match event_text(&ev).unwrap_or("") {
            "TEXT_MESSAGE_CONTENT" => {
                if let Some(d) = ev.get("delta").and_then(|d| d.as_str()) {
                    text.push_str(d);
                }
            }
            "STEP_STARTED" => {
                if let Some(name) = ev.get("stepName").and_then(|s| s.as_str()) {
                    on_line(&format!("→ {name}"));
                }
            }
            "STEP_FINISHED" => {
                if let Some(name) = ev.get("stepName").and_then(|s| s.as_str()) {
                    if !steps.iter().any(|s| s == name) {
                        steps.push(name.to_string());
                    }
                }
            }
            "RUN_ERROR" => {
                let msg = ev
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unspecified AG-UI error");
                error = Some(msg.to_string());
            }
            "RUN_FINISHED" => {
                finished = true;
                if let Some(usage) = ev.get("usage").and_then(|u| u.as_array()) {
                    for u in usage {
                        tokens_in += u.get("inputTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                        tokens_out += u.get("outputTokens").and_then(|x| x.as_u64()).unwrap_or(0);
                    }
                }
                break;
            }
            _ => {}
        }
    }

    if let Some(msg) = error {
        bail!("AG-UI harness `{}` reported an error: {msg}", harness.name);
    }
    if !finished {
        bail!(
            "AG-UI harness `{}` closed the stream without RUN_FINISHED ({} bytes of text)",
            harness.name,
            text.len()
        );
    }
    Ok(Outcome {
        text: strip_ansi(&text),
        tokens_in,
        tokens_out,
        steps,
    })
}
/// Strip terminal escape sequences from protocol text.
///
/// A backend that wraps a CLI inherits that CLI's colouring, and a UI renders
/// the bytes as literal garbage. This is defence at the consumer as well as at
/// the producer (the fleet's own bridge strips too): whichever side is upgraded
/// first, the protocol text is plain.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x1b {
            let start = i;
            while i < b.len() && b[i] != 0x1b {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        i += 1;
        match b.get(i) {
            Some(b'[') => {
                // CSI: parameters then a final byte in @..~
                i += 1;
                while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                    i += 1;
                }
                i += 1;
            }
            Some(b']') => {
                // OSC: ends with BEL or ST (ESC \)
                i += 1;
                while i < b.len() {
                    if b[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => {} // a lone ESC: drop it
        }
    }
    out
}
