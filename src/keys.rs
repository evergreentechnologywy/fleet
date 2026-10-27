//! Key inventory: what the fleet already has, where it came from, and what is
//! still missing — without ever printing a secret.
//!
//! A machine that already has keys should not be asked for them again. This
//! scans the places a fleet keeps them (the fleet home, the standard agent env
//! files, the process environment), reports NAMES and SOURCES, and can import
//! the ones that are missing into `$FLEET_HOME/env` so every harness sees them.
//!
//! The rule everywhere: a value is never logged, never returned by the API, and
//! never written into a descriptor. Only the name, the source, and a fingerprint
//! are. That is what makes the inventory safe to show and safe to commit.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A key the fleet knows about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Known {
    /// The environment variable name.
    pub name: String,
    /// Which integration it belongs to, for grouping in the UI.
    pub group: String,
    /// One line on what it is for.
    pub purpose: String,
    /// Where to get one, when that is knowable.
    pub obtain: Option<String>,
}

/// The keys the fleet expects, grouped. This is the "what should be here" half;
/// the scan is the "what is here" half, and the difference is the work.
pub fn expected() -> Vec<Known> {
    let mut v = Vec::new();
    let mut push = |name: &str, group: &str, purpose: &str, obtain: Option<&str>| {
        v.push(Known {
            name: name.to_string(),
            group: group.to_string(),
            purpose: purpose.to_string(),
            obtain: obtain.map(|s| s.to_string()),
        });
    };

    // --- model providers
    push("TB_API_KEY", "provider", "tokenbroker: the fleet's main model lane", Some("https://thetokenbroker.ai"));
    push("STEP_API_KEY", "provider", "Step Plan: subscription models and media", None);
    push("LIQUID_API_KEY", "provider", "Liquid: same models, priced per call", None);
    push("OPENAI_API_KEY", "provider", "OpenAI-compatible endpoint", Some("https://platform.openai.com"));
    push("ANTHROPIC_API_KEY", "provider", "Anthropic (or the fleet's Anthropic shim)", None);
    push("GEMINI_API_KEY", "provider", "Google Gemini", Some("https://aistudio.google.com"));
    push("GOOGLE_API_KEY", "provider", "Google (same key, different name)", None);
    push("DEEPSEEK_API_KEY", "provider", "DeepSeek", Some("https://platform.deepseek.com"));
    push("XAI_API_KEY", "provider", "xAI / Grok", Some("https://console.x.ai"));
    push("OPENROUTER_API_KEY", "provider", "OpenRouter: many models, one key", Some("https://openrouter.ai/keys"));
    push("GROQ_API_KEY", "provider", "Groq", Some("https://console.groq.com"));
    push("TOGETHER_API_KEY", "provider", "Together", Some("https://api.together.xyz"));
    push("MISTRAL_API_KEY", "provider", "Mistral", Some("https://console.mistral.ai"));
    push("COHERE_API_KEY", "provider", "Cohere", Some("https://dashboard.cohere.com"));
    push("FIREWORKS_API_KEY", "provider", "Fireworks", Some("https://fireworks.ai"));
    push("PERPLEXITY_API_KEY", "provider", "Perplexity", Some("https://perplexity.ai/settings/api"));
    push("CEREBRAS_API_KEY", "provider", "Cerebras", Some("https://cloud.cerebras.ai"));
    push("DASHSCOPE_API_KEY", "provider", "Alibaba DashScope / Qwen", Some("https://dashscope.console.aliyun.com"));

    // --- agent tooling
    push("COMPOSIO_API_KEY", "tooling", "Composio: one API for hundreds of SaaS actions", Some("https://app.composio.dev"));
    push("GITHUB_TOKEN", "tooling", "GitHub: repos, issues, PRs", Some("https://github.com/settings/tokens"));
    push("GITLAB_TOKEN", "tooling", "GitLab", None);
    push("VERCEL_TOKEN", "tooling", "Vercel deploys", None);
    push("CLOUDFLARE_API_TOKEN", "tooling", "Cloudflare DNS and tunnels", Some("https://dash.cloudflare.com/profile/api-tokens"));
    push("DIGITALOCEAN_TOKEN", "tooling", "DigitalOcean", None);
    push("HCLOUD_TOKEN", "tooling", "Hetzner Cloud", None);
    push("LINODE_TOKEN", "tooling", "Linode / Akamai", None);
    push("VULTR_API_KEY", "tooling", "Vultr", None);
    push("HOSTINGER_API_TOKEN", "tooling", "Hostinger domains and DNS", None);
    push("AWS_ACCESS_KEY_ID", "tooling", "AWS", None);
    push("AWS_SECRET_ACCESS_KEY", "tooling", "AWS secret", None);
    push("CLOUDROOM_TOKEN", "tooling", "Cloudroom: run agents in a cloud VM", Some("https://www.cloudroom.dev"));
    push("BROWSERBASE_API_KEY", "tooling", "Browserbase: hosted browsers", Some("https://browserbase.com"));

    // --- messaging
    push("TELEGRAM_BOT_TOKEN", "messaging", "Telegram bot", Some("https://t.me/BotFather"));
    push("TELEGRAM_USER_API_ID", "messaging", "Telegram user account (api_id)", Some("https://my.telegram.org"));
    push("TELEGRAM_USER_API_HASH", "messaging", "Telegram user account (api_hash)", None);
    push("TELEGRAM_USER_SESSION", "messaging", "Telegram user session string", None);
    push("DISCORD_BOT_TOKEN", "messaging", "Discord bot", Some("https://discord.com/developers/applications"));
    push("SLACK_BOT_TOKEN", "messaging", "Slack bot", Some("https://api.slack.com/apps"));
    push("WHATSAPP_TOKEN", "messaging", "WhatsApp Cloud API token", Some("https://developers.facebook.com"));
    push("WHATSAPP_PHONE_ID", "messaging", "WhatsApp phone number id", None);
    push("INKBOX_API_KEY", "messaging", "Inkbox email identities", None);

    // --- voice and telephony
    push("TWILIO_ACCOUNT_SID", "voice", "Twilio account", Some("https://console.twilio.com"));
    push("TWILIO_AUTH_TOKEN", "voice", "Twilio auth token", None);
    push("TWILIO_API_KEY_SID", "voice", "Twilio API key", None);
    push("TWILIO_API_KEY_SECRET", "voice", "Twilio API key secret", None);
    push("ELEVENLABS_API_KEY", "voice", "ElevenLabs speech", Some("https://elevenlabs.io/app/settings"));
    push("SIGNALWIRE_PROJECT_ID", "voice", "SignalWire project", Some("https://signalwire.com"));
    push("SIGNALWIRE_TOKEN", "voice", "SignalWire token", None);
    push("SIGNALWIRE_SPACE_URL", "voice", "SignalWire space URL", None);
    push("BLAND_API_KEY", "voice", "Bland: outbound calling agents", Some("https://bland.ai"));
    push("DEEPGRAM_API_KEY", "voice", "Deepgram transcription", Some("https://deepgram.com"));
    push("ASSEMBLYAI_API_KEY", "voice", "AssemblyAI transcription", Some("https://assemblyai.com"));

    // --- money
    push("PLAID_CLIENT_ID", "money", "Plaid client id", Some("https://dashboard.plaid.com"));
    push("PLAID_SECRET", "money", "Plaid secret", None);
    push("PLAID_ENV", "money", "Plaid environment (sandbox|development|production)", None);
    push("STRIPE_SECRET_KEY", "money", "Stripe", Some("https://dashboard.stripe.com/apikeys"));
    push("COINBASE_API_KEY", "money", "Coinbase", None);
    push("NOWPAYMENTS_API_KEY", "money", "NOWPayments", None);

    // --- marketing and ads
    push("GOOGLE_ADS_DEVELOPER_TOKEN", "marketing", "Google Ads developer token", Some("https://ads.google.com"));
    push("GOOGLE_ADS_CLIENT_ID", "marketing", "Google Ads OAuth client", None);
    push("GOOGLE_ADS_CLIENT_SECRET", "marketing", "Google Ads OAuth secret", None);
    push("GOOGLE_ADS_REFRESH_TOKEN", "marketing", "Google Ads OAuth refresh token", None);
    push("META_AD_ACCOUNT_ID", "marketing", "Meta ads account", None);
    push("META_ACCESS_TOKEN", "marketing", "Meta ads access token", Some("https://developers.facebook.com"));
    push("X_API_KEY", "marketing", "X / Twitter API key", Some("https://developer.x.com"));
    push("X_API_SECRET", "marketing", "X / Twitter API secret", None);
    push("X_ACCESS_TOKEN", "marketing", "X / Twitter access token", None);
    push("X_ACCESS_SECRET", "marketing", "X / Twitter access secret", None);
    push("LINKEDIN_ACCESS_TOKEN", "marketing", "LinkedIn", None);
    push("TIKTOK_ACCESS_TOKEN", "marketing", "TikTok", None);
    push("REDDIT_CLIENT_ID", "marketing", "Reddit app", None);
    push("REDDIT_CLIENT_SECRET", "marketing", "Reddit app secret", None);

    v
}

/// Where a key was found.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    pub name: String,
    /// `env`, or the file it was read from.
    pub source: String,
    /// First 6 chars + length. Enough to tell two keys apart, never enough to
    /// use one.
    pub fingerprint: String,
}

/// Redact a secret to something safe to display.
pub fn fingerprint(v: &str) -> String {
    let head: String = v.chars().take(6).collect();
    format!("{head}…({} chars)", v.chars().count())
}

/// The env files the fleet reads, in precedence order. Existing variables win
/// over all of them, which is why this is only consulted for what is missing.
pub fn env_files() -> Vec<PathBuf> {
    let mut v = vec![crate::paths::home().join("env")];
    for p in [
        "/root/.omp/env",
        "/root/fleet/env",
        "/opt/fleet/.env",
        "/opt/data/.env",
    ] {
        v.push(PathBuf::from(p));
    }
    // The fleet keeps per-service secrets in their own files, one concern each
    // (`twilio.env`, `signalwire.env`, `netcup-hermes.env`, ...). Not scanning
    // them meant `keys import` reported "0 imported" for keys that were sitting
    // right there — the inventory said missing while the machine had them.
    if let Ok(entries) = std::fs::read_dir("/opt/fleet/secrets") {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("env")
                    || p.file_name().map(|f| f == ".env").unwrap_or(false)
            })
            .collect();
        extra.sort();
        v.extend(extra);
    }
    if let Ok(h) = std::env::var("HOME") {
        v.push(PathBuf::from(&h).join(".env"));
    }
    v
}

/// Parse `KEY=value` lines, ignoring comments and blanks.
pub fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if !v.is_empty() {
            m.insert(k.to_string(), v.to_string());
        }
    }
    m
}

/// Every key the fleet can see, from the process environment and the env files.
///
/// A file is only read if it exists; the process environment always wins, and
/// the first file to define a name wins over later ones, so precedence is
/// stated once and matches `paths::load_env_file`.
pub fn scan() -> Vec<Found> {
    let mut out: BTreeMap<String, Found> = BTreeMap::new();
    for (k, v) in std::env::vars() {
        if looks_like_a_secret_name(&k) {
            out.insert(
                k.clone(),
                Found {
                    name: k,
                    source: "env".into(),
                    fingerprint: fingerprint(&v),
                },
            );
        }
    }
    for file in env_files() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (k, v) in parse_env_file(&text) {
            if !looks_like_a_secret_name(&k) {
                continue;
            }
            // First file to define a name wins, and the process environment
            // already did — so an existing entry is left alone.
            out.entry(k.clone()).or_insert_with(|| Found {
                name: k.clone(),
                source: file.display().to_string(),
                fingerprint: fingerprint(&v),
            });
        }
    }
    out.into_values().collect()
}

/// Is this name a credential rather than configuration? Keeps the inventory
/// about keys instead of every variable a shell happens to export.
pub fn looks_like_a_secret_name(k: &str) -> bool {
    const SUFFIXES: [&str; 8] = [
        "_KEY", "_TOKEN", "_SECRET", "_SID", "_API_ID", "_API_HASH", "_SESSION", "_PASSWORD",
    ];
    // A few exact names that do not follow the suffix rule.
    const EXACT: [&str; 6] = [
        "PLAID_CLIENT_ID",
        "AWS_ACCESS_KEY_ID",
        "TELEGRAM_USER_API_ID",
        "TELEGRAM_USER_SESSION",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
    ];
    if EXACT.contains(&k) {
        return true;
    }
    if SUFFIXES.iter().any(|s| k.ends_with(s)) {
        // `..._SECRET_KEY` and friends are secrets; but `..._KEY_ID`-style
        // identifiers are not credentials on their own, so let them through only
        // when they also end in a credential suffix above.
        return !k.ends_with("_ID") || k.ends_with("_API_ID");
    }
    false
}

/// Copy the named keys from wherever they were found into `$FLEET_HOME/env`, so
/// every harness and every later process sees them. Returns the names written.
///
/// Existing values in the target file are preserved: importing is additive, and
/// re-importing the same key overwrites only that key.
pub fn import(names: &[String]) -> Result<Vec<String>> {
    let target = crate::paths::home().join("env");
    let existing = std::fs::read_to_string(&target).unwrap_or_default();
    let mut map = parse_env_file(&existing);

    // Resolve each name from the process environment first, then the files.
    //
    // A name defined in MORE THAN ONE file is a conflict, and silently taking the
    // first is how a mislabeled value wins: measured 2026-10-27, DEEPSEEK_API_KEY
    // held a tokenbroker key (`tb_...`) in /opt/fleet/.env while the real DeepSeek
    // key (`sk-...`) sat in /opt/fleet/secrets/netcup-hermes.env. The import took
    // the wrong one and every DeepSeek call returned 401 while the key "existed".
    //
    // So: collect EVERY definition, and when a name is defined more than once,
    // PREFER THE ONE THAT LOOKS RIGHT FOR THAT PROVIDER, and report the conflict
    // rather than resolving it invisibly.
    let mut sources: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for file in env_files() {
        if let Ok(text) = std::fs::read_to_string(&file) {
            for (k, v) in parse_env_file(&text) {
                sources
                    .entry(k)
                    .or_default()
                    .push((file.display().to_string(), v));
            }
        }
    }
    // Collapse each name to one value, preferring a well-formed one.
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for (k, defs) in &sources {
        if defs.len() > 1 {
            eprintln!(
                "fleet: `{k}` is defined in {} places; choosing the best-formed value",
                defs.len()
            );
            for (where_, _) in defs {
                eprintln!("        {where_}");
            }
        }
        let best = defs
            .iter()
            .max_by_key(|(_, v)| key_shape_score(k, v))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        chosen.insert(k.clone(), best);
    }
    let sources = chosen;

    let mut written = Vec::new();
    for name in names {
        let value = std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| sources.get(name).cloned());
        if let Some(v) = value {
            map.insert(name.clone(), v);
            written.push(name.clone());
        }
    }

    let mut text = String::from(
        "# Written by `fleet keys import`. Keys live here so every harness and\n\
         # every process start sees the same ones. Never commit this file.\n",
    );
    for (k, v) in &map {
        text.push_str(&format!("{k}={v}\n"));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, &text).with_context(|| format!("writing {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    }
    Ok(written)
}

/// The expected keys, annotated with whether they are present and where from.
pub fn inventory() -> Vec<serde_json::Value> {
    let found: BTreeMap<String, Found> = scan().into_iter().map(|f| (f.name.clone(), f)).collect();
    expected()
        .into_iter()
        .map(|k| {
            let hit = found.get(&k.name);
            serde_json::json!({
                "name": k.name,
                "group": k.group,
                "purpose": k.purpose,
                "obtain": k.obtain,
                "present": hit.is_some(),
                "source": hit.map(|h| h.source.clone()),
                "fingerprint": hit.map(|h| h.fingerprint.clone()),
            })
        })
        .collect()
}

/// Keys that are set but not in the expected list — worth showing, because a
/// fleet usually has more than it knows about, and that is where a new
/// integration comes from.
pub fn unexpected() -> Vec<Found> {
    let known: Vec<String> = expected().into_iter().map(|k| k.name).collect();
    scan()
        .into_iter()
        .filter(|f| !known.contains(&f.name))
        .collect()
}

/// How plausible is this value for this variable name?
///
/// A cheap, honest heuristic: several providers prefix their keys, so a value
/// carrying the WRONG provider's prefix is almost certainly mislabeled. Ranking
/// by this is what lets `import` pick the real key when two files disagree,
/// instead of trusting file order.
pub fn key_shape_score(name: &str, value: &str) -> i32 {
    let v = value.trim();
    if v.is_empty() {
        return -1000;
    }
    let lower = name.to_lowercase();

    // (provider fragment in the NAME, the prefix a REAL key for it carries)
    //
    // These must be checked before any generic length rule, and they must
    // DOMINATE it: the whole failure being fixed was a `tb_` tokenbroker key in
    // DEEPSEEK_API_KEY, where both candidates were the same length, so a
    // length-based tie-break picked the wrong one.
    const SHAPES: [(&str, &str); 10] = [
        ("deepseek", "sk-"),
        ("openai", "sk-"),
        ("anthropic", "sk-ant"),
        ("openrouter", "sk-or-"),
        ("groq", "gsk_"),
        ("xai", "xai-"),
        ("fireworks", "fw_"),
        ("together", ""),
        ("perplexity", "pplx-"),
        ("cerebras", "csk-"),
    ];
    // Any provider key that is clearly NOT this provider's.
    const FOREIGN: [&str; 5] = ["tb_", "mk_", "rmt_", "xoxb-", "ghp_"];

    for (frag, prefix) in SHAPES {
        if !lower.contains(frag) || prefix.is_empty() {
            continue;
        }
        if v.starts_with(prefix) {
            // A matching prefix is decisive, and beats every other candidate.
            return 1000;
        }
        if FOREIGN.iter().any(|f| v.starts_with(f)) {
            // Wrong provider's prefix: worse than anything else on offer.
            return -1000;
        }
    }
    // A token-broker key in a non-broker variable is wrong, whatever the name.
    if FOREIGN.iter().any(|f| v.starts_with(f)) && !lower.contains("broker") {
        return -500;
    }
    // Otherwise: a longer, structured value beats a short one.
    (v.len().min(64) as i32) - 20
}

/// Read one key's value from the same places `scan` reads, for a caller that
/// needs it (never for display).
pub fn value(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    for file in env_files() {
        if let Ok(text) = std::fs::read_to_string(&file) {
            if let Some(v) = parse_env_file(&text).get(name) {
                return Some(v.clone());
            }
        }
    }
    None
}

/// Is this path one the fleet should never commit? Used by the doctor.
pub fn is_ignored_path(p: &Path) -> bool {
    p.file_name().map(|f| f == "env" || f == ".env").unwrap_or(false)
}
