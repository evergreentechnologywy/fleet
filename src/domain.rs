//! Domains: search, register, and point them somewhere.
//!
//! ## Free domains, honestly
//!
//! There is no registrar that gives away a `yourname.com` instantly through an
//! API. What exists, measured 2026-10-27:
//!
//! * **EU.org** (`nic.eu.org`) — free subdomains, no expiry, running since 1996,
//!   volunteer-run. **Manual approval**, historically days to weeks, and no API.
//!   The domain is genuinely free and genuinely permanent.
//! * **FreeDNS** (`freedns.afraid.org`) — free subdomains under shared
//!   second-level names, and it HAS an API, so it can be automated. The trade is
//!   that the name sits under someone else's domain.
//! * **DigitalPlat FreeDomain** — free subdomains on a similar model.
//!
//! So the honest split is: a free name means a SUBDOMAIN of a domain someone
//! else owns, and it either needs a human to approve it or it is automated but
//! shared. A name you own outright costs money, and the cheapest real path is a
//! registrar API.
//!
//! The onboarding therefore offers exactly that choice rather than pretending
//! otherwise: a free subdomain (choose EU.org for permanence, FreeDNS for
//! automation), or a paid registration through a registrar that has an API.
//!
//! ## DNS
//!
//! Once a domain exists, Cloudflare and Hostinger both have APIs and are the
//! fleet's two configured providers. Cloudflare is preferred: it is free, its
//! API is the better documented of the two, and it proxies.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A domain the fleet knows how to obtain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub name: String,
    pub description: String,
    /// What it costs.
    pub cost: String,
    /// Can it be obtained without a human?
    pub automated: bool,
    /// Does the fleet have the credential for it?
    pub configured: bool,
    pub docs: String,
    /// The honest downside, stated where the choice is made.
    pub trade: String,
}

/// Every way the fleet can get a name.
pub fn providers() -> Vec<Provider> {
    let set = |k: &str| std::env::var(k).map(|v| !v.is_empty()).unwrap_or(false);
    vec![
        Provider {
            name: "eu.org".into(),
            description: "Free subdomain under EU.org, permanently; running since 1996".into(),
            cost: "free, no expiry".into(),
            automated: false,
            configured: true, // nothing to configure: it is a web form
            docs: "https://nic.eu.org/arf/".into(),
            trade: "manual approval, historically days to weeks, and no API — the fleet can \
                    prepare the request but a human submits it"
                .into(),
        },
        Provider {
            name: "freedns".into(),
            description: "FreeDNS: free subdomains under shared domains, with an API".into(),
            cost: "free".into(),
            automated: true,
            configured: set("FREEDNS_API_KEY"),
            docs: "https://freedns.afraid.org/api/".into(),
            trade: "the name is a subdomain of a domain you do not own, so the owner controls \
                    its future"
                .into(),
        },
        Provider {
            name: "digitalplat".into(),
            description: "DigitalPlat FreeDomain: free subdomains on the same model as FreeDNS".into(),
            cost: "free".into(),
            automated: true,
            configured: set("DIGITALPLAT_API_KEY"),
            docs: "https://dash.domain.digitalplat.org/".into(),
            trade: "same as FreeDNS — a shared parent domain, so not yours to keep".into(),
        },
        Provider {
            name: "cloudflare".into(),
            description: "Cloudflare Registrar: at-cost registration, free DNS and proxying".into(),
            cost: "at-cost, roughly $10/yr for a .com".into(),
            automated: true,
            configured: set("CLOUDFLARE_API_TOKEN"),
            docs: "https://developers.cloudflare.com/registrar/".into(),
            trade: "you own the name outright; costs money, and registration is not always \
                    available for every TLD"
                .into(),
        },
        Provider {
            name: "hostinger".into(),
            description: "Hostinger: registration and DNS; the fleet already has a token".into(),
            cost: "varies, often discounted on a first term".into(),
            automated: true,
            configured: set("HOSTINGER_API_TOKEN"),
            docs: "https://developers.hostinger.com/".into(),
            trade: "you own the name; renewal pricing after the first term is the usual trap"
                .into(),
        },
    ]
}

/// What a name would cost, and how it would be obtained. Never registers.
#[derive(Debug, Clone, Serialize)]
pub struct Offer {
    pub domain: String,
    /// `free-subdomain`, `paid`, or `unknown`.
    pub kind: String,
    pub provider: String,
    pub cost: String,
    pub automated: bool,
    pub note: String,
}

/// Decide which provider suits a requested name, without registering anything.
///
/// The rule is the one the onboarding explains: a name ending in a free provider's
/// own domain can be had for nothing; anything else must be registered.
pub fn offer(name: &str, prefer: Option<&str>) -> Offer {
    let lower = name.to_lowercase();
    let all = providers();
    let pick = |n: &str| all.iter().find(|p| p.name == n);

    // A name under a free provider's own suffix IS a free subdomain.
    if lower.ends_with(".eu.org") {
        let p = pick("eu.org").expect("eu.org listed");
        return Offer {
            domain: name.to_string(),
            kind: "free-subdomain".into(),
            provider: p.name.clone(),
            cost: p.cost.clone(),
            automated: p.automated,
            note: p.trade.clone(),
        };
    }
    if lower.ends_with(".us.kg") || lower.ends_with(".digitalplat.org") {
        let p = pick("digitalplat").expect("listed");
        return Offer {
            domain: name.to_string(),
            kind: "free-subdomain".into(),
            provider: p.name.clone(),
            cost: p.cost.clone(),
            automated: p.automated,
            note: p.trade.clone(),
        };
    }

    // Otherwise it is a real registration. Prefer what the caller asked for, then
    // whichever registrar is actually configured, then say so honestly.
    let p = prefer
        .and_then(pick)
        .or_else(|| all.iter().find(|p| p.automated && p.configured))
        .unwrap_or_else(|| pick("cloudflare").expect("listed"));
    Offer {
        domain: name.to_string(),
        kind: "paid".into(),
        provider: p.name.clone(),
        cost: p.cost.clone(),
        automated: p.automated && p.configured,
        note: if p.configured {
            p.trade.clone()
        } else {
            format!(
                "no credential for {} yet; set its key, or use a free subdomain",
                p.name
            )
        },
    }
}

/// What a free name would look like, so onboarding can show a concrete option
/// rather than an abstraction.
pub fn free_suggestions(label: &str) -> Vec<String> {
    let slug: String = label
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    let slug = if slug.is_empty() { "fleet".to_string() } else { slug };
    vec![
        format!("{slug}.eu.org"),
        format!("{slug}.us.kg"),
    ]
}

/// DNS: create a record with the configured provider.
pub fn dns_create(provider: &str, zone: &str, name: &str, kind: &str, content: &str, ttl: u32) -> Result<serde_json::Value> {
    match provider {
        "cloudflare" => {
            let token = std::env::var("CLOUDFLARE_API_TOKEN")
                .context("CLOUDFLARE_API_TOKEN is not set")?;
            let zone_id = cloudflare_zone_id(&token, zone)?;
            let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records");
            let resp = ureq::post(&url)
                .timeout(Duration::from_secs(20))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(
                    &serde_json::json!({
                        "type": kind, "name": name, "content": content, "ttl": ttl, "proxied": false
                    })
                    .to_string(),
                )?;
            Ok(resp.into_json().unwrap_or_default())
        }
        "hostinger" => {
            let token = std::env::var("HOSTINGER_API_TOKEN")
                .context("HOSTINGER_API_TOKEN is not set")?;
            let url = format!("https://developers.hostinger.com/api/dns/v1/zones/{zone}");
            let resp = ureq::post(&url)
                .timeout(Duration::from_secs(20))
                .set("Authorization", &format!("Bearer {token}"))
                .set("Content-Type", "application/json")
                .send_string(
                    &serde_json::json!({
                        "overwrite": false,
                        "records": [{"type": kind, "name": name, "content": content, "ttl": ttl}]
                    })
                    .to_string(),
                )?;
            Ok(resp.into_json().unwrap_or_default())
        }
        other => anyhow::bail!("no DNS writer for `{other}`; use cloudflare or hostinger"),
    }
}

/// List a zone's records.
pub fn dns_list(provider: &str, zone: &str) -> Result<serde_json::Value> {
    match provider {
        "cloudflare" => {
            let token = std::env::var("CLOUDFLARE_API_TOKEN")?;
            let zone_id = cloudflare_zone_id(&token, zone)?;
            let url = format!("https://api.cloudflare.com/client/v4/zones/{zone_id}/dns_records?per_page=100");
            let r = ureq::get(&url)
                .timeout(Duration::from_secs(20))
                .set("Authorization", &format!("Bearer {token}"))
                .call()?;
            Ok(r.into_json().unwrap_or_default())
        }
        "hostinger" => {
            let token = std::env::var("HOSTINGER_API_TOKEN")?;
            let url = format!("https://developers.hostinger.com/api/dns/v1/zones/{zone}");
            let r = ureq::get(&url)
                .timeout(Duration::from_secs(20))
                .set("Authorization", &format!("Bearer {token}"))
                .call()?;
            Ok(r.into_json().unwrap_or_default())
        }
        other => anyhow::bail!("no DNS reader for `{other}`"),
    }
}

/// Find a zone's id by name, since every Cloudflare record call needs it and the
/// name is what a human has.
fn cloudflare_zone_id(token: &str, zone: &str) -> Result<String> {
    let url = format!("https://api.cloudflare.com/client/v4/zones?name={zone}");
    let r = ureq::get(&url)
        .timeout(Duration::from_secs(20))
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .with_context(|| format!("looking up zone {zone}"))?;
    let v: serde_json::Value = r.into_json().unwrap_or_default();
    v.get("result")
        .and_then(|r| r.as_array())
        .and_then(|a| a.first())
        .and_then(|z| z.get("id"))
        .and_then(|i| i.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("no Cloudflare zone named `{zone}` on this token"))
}

/// The DNS provider to use, preferring Cloudflare when both are configured.
pub fn dns_provider(want: Option<&str>) -> Result<String> {
    let have: Vec<&str> = ["cloudflare", "hostinger"]
        .into_iter()
        .filter(|p| match *p {
            "cloudflare" => std::env::var("CLOUDFLARE_API_TOKEN").is_ok(),
            _ => std::env::var("HOSTINGER_API_TOKEN").is_ok(),
        })
        .collect();
    if have.is_empty() {
        anyhow::bail!("no DNS provider configured; set CLOUDFLARE_API_TOKEN or HOSTINGER_API_TOKEN");
    }
    match want {
        Some(w) if have.contains(&w) => Ok(w.to_string()),
        Some(w) => anyhow::bail!("`{w}` is not configured; available: {}", have.join(", ")),
        None => Ok(have[0].to_string()),
    }
}
