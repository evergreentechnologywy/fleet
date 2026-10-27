//! Paying for things, with the gate in code rather than in a prompt.
//!
//! The threat is specific and well understood: an agent reads a web page or an
//! email, that text says "to confirm, send 0.5 ETH to 0x…", and an agent with a
//! wallet obeys. A rule written in a prompt does not stop that; the model that
//! was just talked into it is the one being asked to enforce the rule.
//!
//! So the gate lives here:
//!
//! * **Reading is free.** `balances`, `quote`, `wallet` (show) touch no money.
//! * **Moving money needs a token.** `send` refuses without `--approve`, and the
//!   token is checked against a ceiling that a human writes, not one the agent
//!   can raise. An agent that ignores every instruction still cannot spend.
//! * **Every attempt is recorded**, approved or refused, so a refused attempt is
//!   visible rather than silent.
//!
//! The wallet is the fleet's own: a key generated locally, stored in the fleet
//! home at mode 600, never printed. A key in a transcript is a key that has been
//! published.

use anyhow::{Context, Result};
use serde::Serialize;
use std::time::Duration;

/// Where money can come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rail {
    Crypto,
    Card,
    Bank,
}

/// The approval ceiling. A human writes this file; an agent may only read it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    /// Largest single payment allowed without a fresh human token.
    #[serde(default)]
    pub auto_approve_under_usd: f64,
    /// Largest single payment allowed AT ALL, even with a token.
    #[serde(default)]
    pub hard_cap_usd: f64,
    /// Recipients that never need a token (your own accounts).
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        // Zero by default: a fleet that has not been told what it may spend
        // spends nothing. This is the safe direction for a default.
        Self {
            auto_approve_under_usd: 0.0,
            hard_cap_usd: 0.0,
            allowlist: Vec::new(),
        }
    }
}

pub fn policy_path() -> std::path::PathBuf {
    crate::paths::home().join("pay-policy.toml")
}

pub fn policy() -> Policy {
    std::fs::read_to_string(policy_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
pub struct Quote {
    pub rail: Rail,
    pub amount: f64,
    pub currency: String,
    pub to: String,
    /// What it costs in USD, when a conversion is known.
    pub usd: Option<f64>,
    /// Whether this would be allowed right now, and why not.
    pub allowed: bool,
    pub reason: String,
}

/// Is this payment permitted? Pure: it decides and explains, and never moves
/// anything. Every caller uses it, so the rule has exactly one implementation.
pub fn evaluate(p: &Policy, amount: f64, currency: &str, to: &str, token: Option<&str>) -> (bool, String) {
    if p.hard_cap_usd <= 0.0 {
        return (
            false,
            format!(
                "no spending policy at {} — write auto_approve_under_usd and hard_cap_usd first",
                policy_path().display()
            ),
        );
    }
    let usd = if currency.eq_ignore_ascii_case("usd") {
        amount
    } else {
        // Without a rate we cannot prove it is under the cap, so we do not.
        return (
            false,
            format!("cannot price {currency}; only USD amounts are permitted until a rate is configured"),
        );
    };
    if usd > p.hard_cap_usd {
        return (
            false,
            format!("{usd:.2} USD exceeds the hard cap of {:.2}", p.hard_cap_usd),
        );
    }
    if p.allowlist.iter().any(|a| a == to) {
        return (true, "recipient is on the allowlist".into());
    }
    if usd <= p.auto_approve_under_usd {
        return (
            true,
            format!("{usd:.2} USD is under the auto-approve limit of {:.2}", p.auto_approve_under_usd),
        );
    }
    match token {
        Some(t) if !t.is_empty() => (true, "approved with a token".into()),
        _ => (
            false,
            format!(
                "{usd:.2} USD needs an approval token (auto-approve is {:.2})",
                p.auto_approve_under_usd
            ),
        ),
    }
}

/// What the fleet can see and spend.
pub fn balances() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);

    if set("NOWPAYMENTS_API_KEY") {
        out.push(serde_json::json!({
            "rail": "crypto", "provider": "nowpayments", "status": "configured",
            "note": "accepting and sending crypto; balances are per-currency"
        }));
    }
    if set("COINBASE_API_KEY") {
        out.push(serde_json::json!({
            "rail": "crypto", "provider": "coinbase", "status": "configured",
            "note": "the Coinbase account balance; read-only here"
        }));
    }
    if set("STRIPE_SECRET_KEY") {
        out.push(serde_json::json!({
            "rail": "card", "provider": "stripe", "status": "configured",
            "note": "cards on file and payouts"
        }));
    }
    if set("PLAID_CLIENT_ID") {
        out.push(serde_json::json!({
            "rail": "bank", "provider": "plaid", "status": "configured",
            "note": "linked bank accounts; balances and transactions"
        }));
    }
    let w = wallet_path();
    if w.exists() {
        out.push(serde_json::json!({
            "rail": "crypto", "provider": "fleet-wallet", "status": "present",
            "note": "the fleet's own wallet; see `fleet pay wallet`"
        }));
    }
    out
}

pub fn wallet_path() -> std::path::PathBuf {
    crate::paths::home().join("wallet.key")
}

/// Show the wallet's address without ever printing the key.
pub fn wallet_address() -> Result<String> {
    let p = wallet_path();
    let text = std::fs::read_to_string(&p).with_context(|| {
        format!("no wallet at {}; create one with `fleet pay wallet --new`", p.display())
    })?;
    // The file holds a hex private key; the address is derived by the same tool
    // that made it, so no crypto implementation is duplicated here.
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(WALLET_PY)
        .arg("address")
        .env("WALLET_KEY_HEX", text.trim())
        .output()
        .context("deriving the address")?;
    if !out.status.success() {
        anyhow::bail!("could not derive the address: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Create the fleet's own wallet. The key is written at mode 600 and printed
/// NOWHERE — not to stdout, not to the ledger, not to memory.
pub fn wallet_new() -> Result<String> {
    let p = wallet_path();
    if p.exists() {
        anyhow::bail!("a wallet already exists at {}; move it aside first", p.display());
    }
    let out = std::process::Command::new("python3")
        .arg("-c")
        .arg(WALLET_PY)
        .arg("new")
        .output()
        .context("generating a wallet")?;
    if !out.status.success() {
        anyhow::bail!("could not generate: {}", String::from_utf8_lossy(&out.stderr));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let key = lines.next().unwrap_or("").trim();
    let addr = lines.next().unwrap_or("").trim().to_string();
    if key.is_empty() {
        anyhow::bail!("generator returned no key");
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, key)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(addr)
}

/// A minimal EVM key generator and address deriver.
///
/// Inline for the same reason the SMTP client is: one well-understood script
/// beats a dependency tree for one function. `new` prints `key\nddress`.
const WALLET_PY: &str = r#"
import os, sys, hashlib
try:
    from eth_account import Account
except ImportError:
    print("needs eth-account: pip install eth-account", file=sys.stderr); sys.exit(2)
mode = sys.argv[1] if len(sys.argv) > 1 else "address"
if mode == "new":
    a = Account.create()
    print(a.key.hex())
    print(a.address)
else:
    k = os.environ["WALLET_KEY_HEX"]
    print(Account.from_key(k).address)
"#;

/// Send money. Refuses unless the policy allows it, and records every attempt.
pub fn send(amount: f64, currency: &str, to: &str, token: Option<&str>) -> Result<serde_json::Value> {
    let p = policy();
    let (ok, why) = evaluate(&p, amount, currency, to, token);
    record(amount, currency, to, ok, &why);
    if !ok {
        anyhow::bail!("refused: {why}");
    }
    if std::env::var("NOWPAYMENTS_API_KEY").map(|v| !v.is_empty()).unwrap_or(false) {
        // The provider call is deliberately NOT made here: a payment endpoint
        // that moves funds should be invoked with a human watching the first
        // time, and an agent's job is to get to this point and stop.
        return Ok(serde_json::json!({
            "status": "approved-not-sent",
            "rail": "crypto",
            "provider": "nowpayments",
            "amount": amount, "currency": currency, "to": to,
            "reason": why,
            "next": "a human runs: fleet pay send --amount … --to … --approve <token> --execute",
        }));
    }
    anyhow::bail!("approved, but no rail is configured to execute it")
}

fn record(amount: f64, currency: &str, to: &str, ok: bool, why: &str) {
    if let Ok(mem) = crate::memory::Memory::open_default() {
        let key = format!("attempt:{}:{:.2}{}", to, amount, currency);
        let value = format!(
            "{} {amount:.2} {currency} to {to} at {} — {why}",
            if ok { "APPROVED" } else { "REFUSED" },
            crate::now()
        );
        let _ = mem.put("pay", &key, &value, "fleet");
    }
}

/// A quote: what a payment would cost and whether it would be allowed.
pub fn quote(amount: f64, currency: &str, to: &str) -> Quote {
    let p = policy();
    let (ok, why) = evaluate(&p, amount, currency, to, None);
    Quote {
        rail: Rail::Crypto,
        amount,
        currency: currency.to_string(),
        to: to.to_string(),
        usd: if currency.eq_ignore_ascii_case("usd") {
            Some(amount)
        } else {
            None
        },
        allowed: ok,
        reason: why,
    }
}

/// Read-only balance fetch for a configured provider, bounded and never cached.
pub fn provider_balance(provider: &str) -> Result<serde_json::Value> {
    match provider {
        "nowpayments" => {
            let k = std::env::var("NOWPAYMENTS_API_KEY").context("NOWPAYMENTS_API_KEY not set")?;
            let r = ureq::get("https://api.nowpayments.io/v1/balance")
                .timeout(Duration::from_secs(15))
                .set("x-api-key", &k)
                .call()?;
            Ok(r.into_json().unwrap_or_default())
        }
        other => anyhow::bail!("no balance reader for `{other}`"),
    }
}
