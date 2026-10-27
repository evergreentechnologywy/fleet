//! Texting, calling, and email — the fleet's own channels.
//!
//! Two rules shape this module:
//!
//! 1. **A new recipient needs confirmation.** Sending to a number the fleet has
//!    never used is the shape of a prompt-injection payout, so `--confirm` is
//!    required the first time. Once a number is in the ledger it is a known
//!    contact and sends freely, because making the tenth message to the same
//!    person need a flag is how a useful tool becomes an unused one.
//! 2. **Every send is recorded.** A message is written to shared memory, so the
//!    transcript of what the fleet said to whom exists on every node and cannot
//!    be lost with one process.
//!
//! Providers are chosen by what has a key: Twilio and SignalWire for SMS, and
//! ElevenLabs / Bland / Vocaleo for calls. Nothing here re-implements a provider
//! SDK; it calls the same HTTP APIs the fleet's own scripts already use, so there
//! is one behaviour to reason about rather than two.

use anyhow::{Context, Result};
use serde::Serialize;
use std::time::Duration;

/// Which channel a message went out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Sms,
    Call,
    Email,
}

/// The result of a send, recorded whether it worked or not.
#[derive(Debug, Clone, Serialize)]
pub struct Sent {
    pub channel: Channel,
    pub provider: String,
    pub to: String,
    pub ok: bool,
    /// The provider's own id for the message, when it returns one.
    pub id: String,
    pub detail: String,
}

/// Is a provider usable right now? The plugin text and this must agree, or an
/// agent is told it can do something the command then refuses.
pub fn available(channel: Channel) -> Vec<&'static str> {
    let set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);
    match channel {
        Channel::Sms => {
            let mut v = Vec::new();
            if set("TWILIO_ACCOUNT_SID") && set("TWILIO_AUTH_TOKEN") {
                v.push("twilio");
            }
            if set("SIGNALWIRE_PROJECT_ID") && set("SIGNALWIRE_TOKEN") {
                v.push("signalwire");
            }
            v
        }
        Channel::Call => {
            let mut v = Vec::new();
            if set("ELEVENLABS_API_KEY") {
                v.push("elevenlabs");
            }
            if set("BLAND_API_KEY") {
                v.push("bland");
            }
            if set("VOCALEO_API_KEY") {
                v.push("vocaleo");
            }
            if set("TWILIO_ACCOUNT_SID") && set("TWILIO_AUTH_TOKEN") {
                v.push("twilio");
            }
            v
        }
        Channel::Email => {
            let mut v = Vec::new();
            if set("SMTP_HOST") || set("GMAIL_USER") {
                v.push("smtp");
            }
            if set("INKBOX_API_KEY") {
                v.push("inkbox");
            }
            v
        }
    }
}

/// Pick a provider for a channel: the named one if it is usable, else the first.
pub fn pick(channel: Channel, want: Option<&str>) -> Result<String> {
    let have = available(channel);
    if have.is_empty() {
        anyhow::bail!(
            "no provider configured for {channel:?}; set one of the keys in `fleet comms providers`"
        );
    }
    match want {
        Some(w) => {
            if have.contains(&w) {
                Ok(w.to_string())
            } else {
                anyhow::bail!("provider `{w}` is not configured for {channel:?}; available: {}", have.join(", "))
            }
        }
        None => Ok(have[0].to_string()),
    }
}

/// Normalise a phone number to E.164, or refuse.
///
/// A number that is not E.164 is how a message goes to the wrong country, and the
/// provider's error for it reads as a network fault. Refusing early is kinder.
pub fn normalise_number(raw: &str) -> Result<String> {
    let s: String = raw.chars().filter(|c| !c.is_whitespace() && *c != '-' && *c != '(' && *c != ')').collect();
    if s.starts_with('+') && s.len() >= 8 && s[1..].chars().all(|c| c.is_ascii_digit()) {
        return Ok(s);
    }
    anyhow::bail!(
        "`{raw}` is not E.164. Use +<country><number> with digits only, e.g. +14165550123"
    )
}

fn post_form(url: &str, user: &str, pass: &str, fields: &[(&str, &str)]) -> Result<(u16, String)> {
    let body: String = fields
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = ureq::post(url)
        .timeout(Duration::from_secs(30))
        .set("Content-Type", "application/x-www-form-urlencoded")
        .set("Authorization", &format!("Basic {}", base64(&format!("{user}:{pass}"))))
        .send_string(&body);
    match resp {
        Ok(r) => Ok((r.status(), r.into_string().unwrap_or_default())),
        Err(ureq::Error::Status(c, r)) => Ok((c, r.into_string().unwrap_or_default())),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

fn post_json(url: &str, bearer: &str, body: &serde_json::Value) -> Result<(u16, String)> {
    let resp = ureq::post(url)
        .timeout(Duration::from_secs(60))
        .set("Content-Type", "application/json")
        .set("Authorization", &format!("Bearer {bearer}"))
        .send_string(&body.to_string());
    match resp {
        Ok(r) => Ok((r.status(), r.into_string().unwrap_or_default())),
        Err(ureq::Error::Status(c, r)) => Ok((c, r.into_string().unwrap_or_default())),
        Err(e) => Err(anyhow::anyhow!("{e}")),
    }
}

/// Send an SMS.
pub fn sms(to: &str, text: &str, provider: Option<&str>) -> Result<Sent> {
    let to = normalise_number(to)?;
    let p = pick(Channel::Sms, provider)?;
    let (code, body) = match p.as_str() {
        "twilio" => {
            let sid = std::env::var("TWILIO_ACCOUNT_SID")?;
            let from = std::env::var("TWILIO_FROM").unwrap_or_default();
            if from.is_empty() {
                anyhow::bail!("TWILIO_FROM is not set, so there is no number to send from");
            }
            post_form(
                &format!("https://api.twilio.com/2010-04-01/Accounts/{sid}/Messages.json"),
                &sid,
                &std::env::var("TWILIO_AUTH_TOKEN")?,
                &[("To", &to), ("From", &from), ("Body", text)],
            )?
        }
        "signalwire" => {
            let space = std::env::var("SIGNALWIRE_SPACE_URL")?;
            let from = std::env::var("SIGNALWIRE_FROM").unwrap_or_default();
            let proj = std::env::var("SIGNALWIRE_PROJECT_ID")?;
            post_form(
                &format!("https://{space}/api/laml/2010-04-01/Accounts/{proj}/Messages.json"),
                &proj,
                &std::env::var("SIGNALWIRE_TOKEN")?,
                &[("To", &to), ("From", &from), ("Body", text)],
            )?
        }
        other => anyhow::bail!("unsupported sms provider `{other}`"),
    };
    Ok(Sent {
        channel: Channel::Sms,
        provider: p,
        to,
        ok: (200..300).contains(&code),
        id: extract_id(&body),
        detail: if (200..300).contains(&code) {
            "sent".into()
        } else {
            truncate(&body, 300)
        },
    })
}

/// Place a call with a spoken script.
pub fn call(to: &str, say: &str, provider: Option<&str>) -> Result<Sent> {
    let to = normalise_number(to)?;
    let p = pick(Channel::Call, provider)?;
    let (code, body) = match p.as_str() {
        "bland" => {
            let key = std::env::var("BLAND_API_KEY")?;
            post_json(
                "https://api.bland.ai/v1/calls",
                &key,
                &serde_json::json!({
                    "phone_number": to,
                    "task": say,
                    // A transcript is the point of a call: without it the fleet
                    // learns nothing and the call is unverifiable.
                    "record": true,
                    "wait_for_completion": false,
                }),
            )?
        }
        "elevenlabs" => {
            // ElevenLabs Convai places the call; the agent id names the voice
            // agent, which is what makes the script speak rather than a TTS blob.
            let key = std::env::var("ELEVENLABS_API_KEY")?;
            let agent = std::env::var("ELEVENLABS_AGENT_ID").unwrap_or_default();
            if agent.is_empty() {
                anyhow::bail!("ELEVENLABS_AGENT_ID is not set, so there is no voice agent to call with");
            }
            let phone = std::env::var("ELEVENLABS_PHONE_ID").unwrap_or_default();
            post_json(
                "https://api.elevenlabs.io/v1/convai/twilio/outbound-call",
                &key,
                &serde_json::json!({
                    "agent_id": agent,
                    "agent_phone_number_id": phone,
                    "to_number": to,
                    "conversation_initiation_client_data": {
                        "dynamic_variables": { "script": say }
                    }
                }),
            )?
        }
        "twilio" => {
            let sid = std::env::var("TWILIO_ACCOUNT_SID")?;
            let from = std::env::var("TWILIO_FROM").unwrap_or_default();
            let twiml = format!(
                "<Response><Say>{}</Say></Response>",
                say.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
            );
            post_form(
                &format!("https://api.twilio.com/2010-04-01/Accounts/{sid}/Calls.json"),
                &sid,
                &std::env::var("TWILIO_AUTH_TOKEN")?,
                &[("To", &to), ("From", &from), ("Twiml", &twiml)],
            )?
        }
        "vocaleo" => {
            let key = std::env::var("VOCALEO_API_KEY")?;
            post_json(
                "https://api.vocaleo.ai/v1/calls",
                &key,
                &serde_json::json!({ "to": to, "script": say }),
            )?
        }
        other => anyhow::bail!("unsupported call provider `{other}`"),
    };
    Ok(Sent {
        channel: Channel::Call,
        provider: p,
        to,
        ok: (200..300).contains(&code),
        id: extract_id(&body),
        detail: if (200..300).contains(&code) {
            "placed".into()
        } else {
            truncate(&body, 300)
        },
    })
}

/// Send email over SMTP.
pub fn email(to: &str, subject: &str, body: &str) -> Result<Sent> {
    let host = std::env::var("SMTP_HOST").context("SMTP_HOST is not set")?;
    let port: u16 = std::env::var("SMTP_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(587);
    let user = std::env::var("SMTP_USER").unwrap_or_default();
    let pass = std::env::var("SMTP_PASS").unwrap_or_default();
    let from = std::env::var("SMTP_FROM").unwrap_or_else(|_| user.clone());

    // A single TLS connection via the system's `swaks`/`sendmail` is avoided on
    // purpose: those may not exist, and a missing MTA is a confusing failure.
    // The fleet's own SMTP client is the only dependency.
    let mut cmd = std::process::Command::new("python3");
    cmd.arg("-c").arg(SMTP_PY);
    cmd.env("SMTP_HOST", &host)
        .env("SMTP_PORT", port.to_string())
        .env("SMTP_USER", &user)
        .env("SMTP_PASS", &pass)
        .env("SMTP_FROM", &from)
        .env("MAIL_TO", to)
        .env("MAIL_SUBJECT", subject)
        .env("MAIL_BODY", body);
    let out = cmd.output().context("running the smtp client")?;
    let ok = out.status.success();
    Ok(Sent {
        channel: Channel::Email,
        provider: "smtp".into(),
        to: to.to_string(),
        ok,
        id: String::new(),
        detail: if ok {
            "sent".into()
        } else {
            truncate(&String::from_utf8_lossy(&out.stderr), 300)
        },
    })
}

/// A tiny SMTP client, inline so the module has one dependency (python3, which
/// every fleet node has) rather than a Rust TLS stack for one function.
const SMTP_PY: &str = r#"
import os, smtplib, sys
from email.message import EmailMessage
m = EmailMessage()
m["From"] = os.environ["SMTP_FROM"] or os.environ["SMTP_USER"]
m["To"] = os.environ["MAIL_TO"]
m["Subject"] = os.environ["MAIL_SUBJECT"]
m.set_content(os.environ["MAIL_BODY"])
h = os.environ["SMTP_HOST"]; p = int(os.environ["SMTP_PORT"])
u = os.environ.get("SMTP_USER"); pw = os.environ.get("SMTP_PASS")
try:
    s = smtplib.SMTP(h, p, timeout=30)
    s.starttls(); s.login(u, pw) if u else None
    s.send_message(m); s.quit()
except Exception as e:
    print(f"smtp failed: {e}", file=sys.stderr); sys.exit(1)
"#;

fn extract_id(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            for k in ["sid", "id", "call_id", "message_id", "callId"] {
                if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                    return Some(s.to_string());
                }
            }
            None
        })
        .unwrap_or_default()
}

fn truncate(s: &str, n: usize) -> String {
    let t: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        format!("{t}…")
    } else {
        t
    }
}

fn urlencode(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => o.push(b as char),
            b' ' => o.push('+'),
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

fn base64(s: &str) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let b = s.as_bytes();
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        o.push(T[(n >> 18 & 63) as usize] as char);
        o.push(T[(n >> 12 & 63) as usize] as char);
        o.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        o.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    o
}

/// Record a send in shared memory, so what the fleet said is never lost with one
/// process and is visible from every node.
pub fn record(s: &Sent) {
    if let Ok(mem) = crate::memory::Memory::open_default() {
        let key = format!("{:?}:{}", s.channel, s.to).to_lowercase();
        let value = format!(
            "{} via {} at {} — {}",
            if s.ok { "sent" } else { "FAILED" },
            s.provider,
            crate::now(),
            s.detail
        );
        let _ = mem.put("comms", &key, &value, "fleet");
    }
}
