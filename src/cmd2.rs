//! Command implementations — part 2: plugins, peers, sessions, memory,
//! selfmod, lessons, ledger, deploy, config, onboard.

use anyhow::{bail, Result};

use fleet::ledger::Ledger;
use fleet::memory::Memory;

use crate::{
    ConfigCmd, CredentialCmd, TargetsCmd, DeployArgs, LedgerArgs, LessonsCmd, MemoryCmd, OnboardArgs, PeersCmd, PluginsCmd,
    SelfmodCmd, SessionsCmd,
};

// ---------------------------------------------------------------- plugins

pub fn plugins(c: PluginsCmd, json: bool) -> Result<()> {
    let p = fleet::plugins::Plugins::default_dir();
    match c {
        PluginsCmd::List => {
            let all = p.all();
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
                return Ok(());
            }
            println!("plugins: {}  ({})", p.dir().display(), all.len());
            for s in &all {
                println!(
                    "  {:<16} prio {:<4} events [{}] harnesses [{}]{}",
                    s.name,
                    s.priority,
                    if s.events.is_empty() {
                        "*".into()
                    } else {
                        s.events.join(",")
                    },
                    if s.harnesses.is_empty() {
                        "*".into()
                    } else {
                        s.harnesses.join(",")
                    },
                    if s.description.is_empty() {
                        String::new()
                    } else {
                        format!("  — {}", s.description)
                    }
                );
            }
            if all.is_empty() {
                println!("  (none) — `fleet plugins example` writes a working one");
            }
            println!("\nA plugin lives in the engine's call path, so it applies to every harness — including ones added later.");
            Ok(())
        }
        PluginsCmd::Add { path } => {
            let s = fleet::plugins::install(&path)?;
            println!(
                "installed `{}` — it now runs on {} harness(es), events [{}]",
                s.name,
                if s.harnesses.is_empty() {
                    "every".to_string()
                } else {
                    s.harnesses.join(",")
                },
                if s.events.is_empty() {
                    "*".to_string()
                } else {
                    s.events.join(",")
                }
            );
            Ok(())
        }
        PluginsCmd::Rm { name } => {
            let d = fleet::plugins::remove(&name)?;
            println!("removed {}", d.display());
            Ok(())
        }
        PluginsCmd::Test { name, event, task } => {
            let all = p.all();
            let s = all
                .iter()
                .find(|s| s.name == name)
                .ok_or_else(|| anyhow::anyhow!("no plugin `{name}`"))?;
            let fx = p.fire(
                &event,
                &s.harnesses.first().cloned().unwrap_or_default(),
                "",
                &serde_json::json!({"task": task}),
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "plugin": name,
                    "event": event,
                    "effects": {
                        "env": fx.env,
                        "args_append": fx.args_append,
                        "prompt_prefix": fx.prompt_prefix,
                        "prompt_suffix": fx.prompt_suffix,
                        "skip": fx.skip,
                        "skip_reason": fx.skip_reason,
                        "memory": fx.memory,
                        "note": fx.note,
                    }
                }))?
            );
            Ok(())
        }
        PluginsCmd::Example => {
            let dir = p.dir().join("example");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("plugin.toml"), fleet::plugins::EXAMPLE_SPEC)?;
            std::fs::write(dir.join("plugin.sh"), fleet::plugins::EXAMPLE)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(
                    dir.join("plugin.sh"),
                    std::fs::Permissions::from_mode(0o755),
                );
            }
            println!(
                "wrote {} (disabled by default: set `enabled = true`)",
                dir.display()
            );
            Ok(())
        }
        PluginsCmd::Dir => {
            println!("{}", p.dir().display());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- peers

pub fn peers(c: PeersCmd, json: bool) -> Result<()> {
    let mesh = fleet::mesh::Mesh::default_mesh();
    let mem = Memory::open_default()?;
    match c {
        PeersCmd::List => {
            let all = mesh.peers();
            if json {
                let rows: Vec<serde_json::Value> = all
                    .iter()
                    .map(|p| {
                        let t = mesh.reach(p, &mem);
                        serde_json::json!({
                            "name": p.name, "host": p.host,
                            "transport": t.as_str(), "reachable": t != fleet::mesh::Transport::None,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            println!("peers: {}  ({})", mesh.path().display(), all.len());
            for p in &all {
                let t = mesh.reach(p, &mem);
                println!(
                    "  {:<14} {:<32} {:<10} {}",
                    p.name,
                    p.host,
                    t.as_str(),
                    if t == fleet::mesh::Transport::None {
                        "unreachable"
                    } else {
                        "ok"
                    }
                );
            }
            if all.is_empty() {
                println!("  (none) — `fleet peers discover` finds candidates on this machine");
            }
            Ok(())
        }
        PeersCmd::Add {
            name,
            host,
            transport,
            ssh_arg,
        } => {
            let t = match transport.as_deref() {
                None => None,
                Some("tailscale") => Some(fleet::mesh::Transport::Tailscale),
                Some("ssh") => Some(fleet::mesh::Transport::Ssh),
                Some("reason") => Some(fleet::mesh::Transport::Reason),
                Some(other) => bail!("unknown transport `{other}`: use tailscale, ssh, or reason"),
            };
            let p = fleet::mesh::Peer {
                name: name.clone(),
                host,
                ssh_args: ssh_arg,
                transport: t,
                slots: None,
                notes: String::new(),
            };
            mesh.put(&p)?;
            let got = match p.transport {
                Some(t) => t,
                None => mesh.reach(&p, &mem),
            };
            println!("peer `{name}` saved; transport: {}", got.as_str());
            Ok(())
        }
        PeersCmd::Rm { name } => {
            mesh.remove(&name)?;
            println!("peer `{name}` removed");
            Ok(())
        }
        PeersCmd::Discover => {
            let found = fleet::mesh::detect_candidates();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &found
                            .iter()
                            .map(|(n, h, t)| {
                                serde_json::json!({"name": n, "host": h, "transport": t.as_str()})
                            })
                            .collect::<Vec<_>>()
                    )?
                );
                return Ok(());
            }
            let known: Vec<String> = mesh.peers().iter().map(|p| p.name.clone()).collect();
            for (n, h, t) in &found {
                let mark = if known.contains(n) {
                    "configured"
                } else {
                    "new"
                };
                println!("  {n:<20} {h:<40} via {}  [{mark}]", t.as_str());
            }
            if found.is_empty() {
                println!("nothing discoverable: no tailscale peers and no reason peers found");
            }
            Ok(())
        }
        PeersCmd::Send {
            name,
            message,
            email,
        } => {
            let msg = message.join(" ");
            if let Some(to) = email {
                let out = mesh.email(&to, "fleet message", &msg)?;
                println!("emailed {to}\n{out}");
                return Ok(());
            }
            let p = mesh
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("no peer `{name}`"))?;
            let t = mesh.send(&p, &msg, &mem)?;
            println!("delivered to {name} over {}", t.as_str());
            Ok(())
        }
        PeersCmd::Inbox { name } => {
            let p = mesh
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("no peer `{name}`"))?;
            println!("{}", mesh.inbox(&p, &mem)?);
            Ok(())
        }
        PeersCmd::Run {
            name,
            harness,
            task,
        } => {
            let p = mesh
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("no peer `{name}`"))?;
            let out = mesh.run_remote(&p, &harness, &task.join(" "), &mem)?;
            println!("{out}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- sessions

pub fn sessions(c: SessionsCmd, json: bool) -> Result<()> {
    let mem = Memory::open_default()?;
    match c {
        SessionsCmd::List { limit } => {
            let rows = mem.sessions(limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            for s in rows {
                println!(
                    "  {:<28} {:>4} turns  {:<10} {}  {}",
                    s.id, s.turns, s.status, s.updated, s.title
                );
            }
            Ok(())
        }
        SessionsCmd::Show { id, limit } => {
            let turns = mem.turns(&id, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&turns)?);
                return Ok(());
            }
            for t in turns {
                let who = if t.harness.is_empty() {
                    t.role.clone()
                } else {
                    format!("{}·{}", t.role, t.harness)
                };
                println!("[{}] {}  {}", t.seq, t.ts, who);
                if !t.model.is_empty() {
                    println!("    model: {}", t.model);
                }
                println!("{}", t.text);
                println!();
            }
            Ok(())
        }
        SessionsCmd::Context { id, turns } => {
            println!("{}", mem.resume_context(&id, turns)?);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- memory

pub fn memory(c: MemoryCmd, json: bool) -> Result<()> {
    let mem = Memory::open_default()?;
    match c {
        MemoryCmd::List {
            scope,
            prefix,
            limit,
        } => {
            let rows = mem.list(&scope, &prefix, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &rows
                            .iter()
                            .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                            .collect::<Vec<_>>()
                    )?
                );
                return Ok(());
            }
            for (k, v) in rows {
                println!("  {k} = {}", fleet::runner::truncate(&v, 300));
            }
            Ok(())
        }
        MemoryCmd::Get { key, scope } => {
            match mem.get(&scope, &key)? {
                Some(v) => println!("{v}"),
                None => {
                    if json {
                        println!("null");
                    } else {
                        eprintln!("(no such key: {scope}/{key})");
                    }
                    std::process::exit(2);
                }
            }
            Ok(())
        }
        MemoryCmd::Put { key, value, scope } => {
            let v = value.join(" ");
            let version = mem.put(&scope, &key, &v, "cli")?;
            println!("{scope}/{key} v{version} — durable now");
            Ok(())
        }
        MemoryCmd::Search { needle, limit } => {
            let rows = mem.search(&needle, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &rows
                            .iter()
                            .map(|(s, k, v)| serde_json::json!({"scope": s, "key": k, "value": v}))
                            .collect::<Vec<_>>()
                    )?
                );
                return Ok(());
            }
            for (s, k, v) in rows {
                println!("  {s}/{k} = {}", fleet::runner::truncate(&v, 200));
            }
            Ok(())
        }
        MemoryCmd::History { key, scope } => {
            let rows = mem.history(&scope, &key, 50)?;
            if rows.is_empty() {
                println!("(no history for {scope}/{key})");
            }
            for (v, val, at) in rows {
                println!(
                    "  v{v} (replaced {at}) = {}",
                    fleet::runner::truncate(&val, 300)
                );
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- selfmod

pub fn selfmod(c: SelfmodCmd, json: bool) -> Result<()> {
    let ledger = Ledger::default_path();
    match c {
        SelfmodCmd::Status => {
            let s = fleet::selfmod::status(&ledger)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
                return Ok(());
            }
            println!("ceiling file: {}", fleet::selfmod::policy_path().display());
            println!("  enabled    {}", s["enabled"]);
            println!("  paths      {}", s["paths"]);
            println!("  verifiers  {}", s["verifiers"]);
            println!("  marks      {}", s["marks"]);
            println!("  failed verifications {}", s["failed_verifications"]);
            for m in s["last_marks"].as_array().cloned().unwrap_or_default() {
                println!("  {} {} {}", m["ts"], m["target"], m["digest"]);
            }
            Ok(())
        }
        SelfmodCmd::Init => {
            fleet::selfmod::write_starter_policy()?;
            let d = fleet::selfmod::write_builtin_verifier()?;
            println!("ceiling: {}", fleet::selfmod::policy_path().display());
            println!("verifier: {}", d.display());
            println!("Both are inert until `enabled = true` is set in the ceiling.");
            Ok(())
        }
        SelfmodCmd::Verifiers => {
            for (n, d) in fleet::selfmod::list_verifiers() {
                println!("  {n:<20} {d}");
            }
            Ok(())
        }
        SelfmodCmd::Check { target } => {
            println!("{}", fleet::selfmod::check(&target)?);
            Ok(())
        }
        SelfmodCmd::Propose {
            target,
            reason,
            content,
        } => {
            let body = match content {
                Some(c) if c.starts_with('@') => {
                    Some(std::fs::read_to_string(c.trim_start_matches('@'))?)
                }
                Some(c) => Some(c),
                None => None,
            };
            let grant = fleet::selfmod::Grant {
                target: target.clone(),
                reason: reason.clone(),
                content: body,
                digest: String::new(),
            };
            let ceiling = fleet::selfmod::Ceiling::load();
            match fleet::selfmod::propose(&ceiling, &grant, &ledger, "cli") {
                Ok(d) => {
                    println!("admitted `{target}` sha256={d}");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("refused: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

// ---------------------------------------------------------------- lessons

pub fn lessons(c: LessonsCmd, json: bool) -> Result<()> {
    let mem = Memory::open_default()?;
    match c {
        LessonsCmd::List { limit } => {
            let l = fleet::lessons::briefing(&mem, limit as usize)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&l)?);
                return Ok(());
            }
            if l.is_empty() {
                println!("(nothing learned yet)");
            }
            for x in l {
                println!("  [{}×{}] {}", x.kind, x.seen, x.text);
            }
            Ok(())
        }
        LessonsCmd::Add { text, kind } => {
            let t = text.join(" ");
            let l = fleet::lessons::record(&mem, &kind, &t, &t)?;
            println!("recorded {} ({})", l.key, l.seen);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- ledger

pub fn ledger(a: LedgerArgs, json: bool) -> Result<()> {
    let led = Ledger::default_path();
    let rows = led.tail(a.kind.as_deref(), a.limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    println!("ledger: {}", fleet::paths::ledger_path().display());
    for e in rows {
        println!(
            "  {} {:<8} {} {:<14} {}",
            e.ts,
            e.kind,
            if e.ok { "ok  " } else { "FAIL" },
            e.harness,
            fleet::runner::truncate(&format!("{} {}", e.subject, e.detail), 160)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- deploy

pub fn deploy(a: DeployArgs) -> Result<()> {
    let spec = fleet::deploy::DeploySpec {
        target: a.to.clone(),
        method: None,
        prefix: Some(a.prefix),
        port: Some(a.port),
        run: a.run,
        release: a.release,
    };
    if a.dry_run {
        println!("{}", fleet::deploy::one_liner(&spec));
        println!();
    }
    let lines = fleet::deploy::deploy(&spec, a.dry_run)?;
    for l in lines {
        println!("{l}");
    }
    Ok(())
}

// ---------------------------------------------------------------- config

pub fn config(c: ConfigCmd, json: bool) -> Result<()> {
    match c {
        ConfigCmd::Show => {
            let cfg = fleet::config::Config::load();
            if json {
                println!("{}", serde_json::to_string_pretty(&cfg)?);
                return Ok(());
            }
            println!("file        {}", fleet::paths::config_path().display());
            println!("chooser     {:?}", cfg.chooser.kind);
            if let Some(c) = &cfg.chooser.command {
                println!("  command   {c} {}", cfg.chooser.args.join(" "));
            }
            if let Some(u) = &cfg.chooser.url {
                println!("  url       {u}");
            }
            println!("  ttl       {}s", cfg.chooser.ttl().as_secs());
            println!(
                "defaults    model={:?} effort={:?}",
                cfg.defaults.model, cfg.defaults.effort
            );
            println!(
                "            base={:?} key_env={:?}",
                cfg.defaults.provider_base, cfg.defaults.provider_key_env
            );
            println!("parallel    {}", cfg.parallel());
            Ok(())
        }
        ConfigCmd::SetChooser { kind } => {
            let mut cfg = fleet::config::Config::load();
            cfg.chooser = match kind.as_str() {
                "jev" => fleet::config::ChooserConfig {
                    kind: fleet::config::ChooserKind::Cli,
                    command: Some("/opt/fleet/scripts/jev_text_route.py".into()),
                    args: vec!["--request".into(), "{prompt}".into(), "--json".into()],
                    ..Default::default()
                },
                "static" => fleet::config::ChooserConfig {
                    kind: fleet::config::ChooserKind::Static,
                    ..Default::default()
                },
                "http" => fleet::config::ChooserConfig {
                    kind: fleet::config::ChooserKind::Http,
                    url: Some("http://127.0.0.1:8792/v1/chat/completions".into()),
                    auth_header: Some("Authorization".into()),
                    api_key_env: Some("FLEET_CHOOSER_KEY".into()),
                    ..Default::default()
                },
                other => fleet::config::ChooserConfig {
                    kind: fleet::config::ChooserKind::Cli,
                    command: Some(other.to_string()),
                    args: vec!["{prompt}".into()],
                    ..Default::default()
                },
            };
            cfg.save()?;
            println!("chooser set to {kind}");
            Ok(())
        }
        ConfigCmd::SetDefault { model, effort } => {
            let mut cfg = fleet::config::Config::load();
            cfg.defaults.model = Some(model.clone());
            if let Some(e) = effort {
                cfg.defaults.effort = Some(e);
            }
            cfg.save()?;
            println!("default model set to {model}");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------- onboard

pub fn onboard(a: OnboardArgs, json: bool) -> Result<()> {
    fleet::paths::ensure_dirs()?;
    let mut did: Vec<String> = Vec::new();

    // 1. Chooser. Jev if it is here, otherwise nothing — both are valid.
    let mut cfg = fleet::config::Config::load();
    let chooser = a.chooser.clone().unwrap_or_else(|| {
        if std::path::Path::new("/opt/fleet/scripts/jev_text_route.py").exists() {
            "jev".into()
        } else {
            "static".into()
        }
    });
    cfg.chooser = match chooser.as_str() {
        "jev" => fleet::config::ChooserConfig {
            kind: fleet::config::ChooserKind::Cli,
            command: Some("/opt/fleet/scripts/jev_text_route.py".into()),
            args: vec!["--request".into(), "{prompt}".into(), "--json".into()],
            ttl_s: Some(300),
            ..Default::default()
        },
        "static" => fleet::config::ChooserConfig {
            kind: fleet::config::ChooserKind::Static,
            ..Default::default()
        },
        "http" => fleet::config::ChooserConfig {
            kind: fleet::config::ChooserKind::Http,
            url: Some("http://127.0.0.1:8792/v1/chat/completions".into()),
            auth_header: Some("Authorization".into()),
            api_key_env: Some("FLEET_CHOOSER_KEY".into()),
            ..Default::default()
        },
        path => fleet::config::ChooserConfig {
            kind: fleet::config::ChooserKind::Cli,
            command: Some(path.to_string()),
            args: vec!["{prompt}".into()],
            ..Default::default()
        },
    };
    did.push(format!("chooser: {chooser}"));

    // 2. Provider. The second key the user said is required before starting.
    let ps = fleet::providers::Providers::default_file();
    let detected = fleet::providers::detect_from_env();
    if let Some(base) = a.base_url.clone() {
        let key_env = a.key_env.clone().unwrap_or_else(|| "OPENAI_API_KEY".into());
        ps.put(&fleet::providers::Provider {
            name: "custom".into(),
            base_url: base.clone(),
            key_env: key_env.clone(),
            extra: Default::default(),
            models: vec!["*".into()],
            block: Vec::new(),
            credential: None,
            enabled: true,
            notes: "from onboarding".into(),
        })?;
        cfg.defaults.provider_base = Some(base.clone());
        cfg.defaults.provider_key_env = Some(key_env.clone());
        cfg.defaults.model = Some(fleet::templates::default_model_for(&base).into());
        did.push(format!("provider: {base} (key {key_env})"));
    } else if !detected.is_empty() {
        for p in &detected {
            ps.put(p)?;
        }
        let first = &detected[0];
        cfg.defaults.provider_base = Some(first.base_url.clone());
        cfg.defaults.provider_key_env = Some(first.key_env.clone());
        cfg.defaults.model = Some(fleet::templates::default_model_for(&first.base_url).into());
        did.push(format!(
            "providers detected from the environment: {}",
            detected
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        did.push(
            "no provider key found — add one with `fleet providers add …`, or set OPENAI_API_KEY"
                .into(),
        );
    }
    cfg.save()?;

    // 3. Harnesses: every CLI actually installed here.
    let dir = fleet::paths::harness_dir();
    let reg = fleet::registry::Registry::new(dir.clone());
    let existing: Vec<String> = reg.all().0.into_iter().map(|h| h.name).collect();
    let mut installed = Vec::new();
    for p in fleet::templates::presets_for_path() {
        if existing.contains(&p.name.to_string()) {
            continue;
        }
        match fleet::templates::install_preset(&dir, &p) {
            Ok(f) => installed.push(format!("{} ({})", p.name, f.display())),
            Err(e) => eprintln!("could not write {}: {e}", p.name),
        }
    }
    if !installed.is_empty() {
        did.push(format!("harnesses written: {}", installed.join(", ")));
    }

    // 4. Ceiling + verifier, inert by default.
    fleet::selfmod::write_starter_policy()?;
    let _ = fleet::selfmod::write_builtin_verifier();

    // 5. The home README, so the directory explains itself.
    let readme = fleet::paths::home().join("README.md");
    if !readme.exists() {
        std::fs::write(&readme, fleet::templates::HOME_README)?;
    }

    let (all, errors) = reg.all();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "home": fleet::paths::home().to_string_lossy(),
                "steps": did,
                "harnesses": all.iter().map(|h| {
                    let (ok, why) = fleet::runner::probe(h);
                    serde_json::json!({"name": h.name, "available": ok, "detail": why})
                }).collect::<Vec<_>>(),
                "errors": errors.iter().map(|(p, e)| serde_json::json!({
                    "path": p.to_string_lossy(), "error": e})).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!("fleet home: {}", fleet::paths::home().display());
    for d in &did {
        println!("  ✓ {d}");
    }
    println!();
    println!("harnesses live now:");
    for h in &all {
        let (ok, why) = fleet::runner::probe(h);
        println!("  {} {:<12} {}", if ok { "●" } else { "○" }, h.name, why);
    }
    if all.is_empty() {
        println!("  (none yet — `fleet harness new omp` if you have that CLI)");
    }
    for (p, e) in &errors {
        println!("  !! {}: {e}", p.display());
    }
    println!();
    println!("Try:");
    println!("  fleet run \"explain what a harness for harnesses is\"");
    println!("  fleet run \"compare two approaches to X\" --unison");
    println!("  fleet harness list");
    println!("  fleet serve            # local web API on :8787");
    println!("  fleet deploy --to ssh://you@host --run   # one line to a server");
    println!("  fleet targets list                       # deploy targets, incl. plugins");
    if std::env::var("FLEET_SKIP_SELFCHECK").is_err() {
        let _ = self_check(&reg);
    }
    Ok(())
}

/// A first-run check that says, plainly, whether a run can happen at all.
fn self_check(reg: &fleet::registry::Registry) -> Result<()> {
    let (all, _) = reg.all();
    let ready: Vec<&fleet::registry::Harness> = all
        .iter()
        .filter(|h| h.enabled && fleet::runner::probe(h).0)
        .collect();
    if ready.is_empty() {
        println!();
        println!(
            "! no harness is runnable yet. A harness is a file: write one with \
             `fleet harness new openai-compat --base-url … --key-env …` and it is live at once."
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- helpers

/// Print every path the engine uses, so a bug report is one paste.
pub fn print_paths() {
    println!("home        {}", fleet::paths::home().display());
    println!("harnesses   {}", fleet::paths::harness_dir().display());
    println!(
        "plugins     {}",
        fleet::paths::home().join("plugins").display()
    );
    println!("verifiers   {}", fleet::paths::verifier_dir().display());
    println!("database    {}", fleet::paths::db_path().display());
    println!("ledger      {}", fleet::paths::ledger_path().display());
    println!("runs        {}", fleet::paths::runs_dir().display());
    println!("config      {}", fleet::paths::config_path().display());
    println!(
        "providers   {}",
        fleet::paths::home().join("providers.toml").display()
    );
    println!(
        "peers       {}",
        fleet::paths::home().join("peers.toml").display()
    );
    println!("ceiling     {}", fleet::selfmod::policy_path().display());
}

/// The hint shown by a bare `fleet` on an unconfigured machine.
pub fn onboard_hint() -> String {
    format!(
        "fleet — a harness for harnesses.\n\n\
         Nothing is installed yet in {}.\n\n\
         Two things are needed to start:\n\
           1. a chooser  — Jev, or any alternative, or nothing (`static`)\n\
           2. a provider — any OpenAI-compatible base URL and its key env var\n\n\
         Run:  fleet onboard\n\n\
         Or skip onboarding entirely: a harness is a file.\n\
         Write one into {} and it is live on the next call.\n",
        fleet::paths::harness_dir().display(),
        fleet::paths::harness_dir().display()
    )
}

/// Help text shown when harnesses exist and no subcommand was given.
pub fn help_text(reg: &fleet::registry::Registry) -> String {
    let (all, _) = reg.all();
    let mut s = String::new();
    s.push_str("fleet — a harness for harnesses.\n\n");
    s.push_str(&format!(
        "{} harness(es) live in {}\n",
        all.len(),
        reg.dir().display()
    ));
    for h in &all {
        let (ok, _) = fleet::runner::probe(h);
        s.push_str(&format!(
            "  {} {:<14} {}\n",
            if ok { "●" } else { "○" },
            h.name,
            h.description
        ));
    }
    s.push_str(
        "\nRun a task:\n\
         \x20 fleet run \"your task\"                  # planner picks\n\
         \x20 fleet run \"your task\" -H codex        # force one\n\
         \x20 fleet run \"compare X and Y\" --unison  # several at once\n\
         \x20 fleet run \"do X then verify\" --serial # pipeline\n\
         \x20 fleet run --resume last               # continue a conversation\n\n\
         Manage:\n\
         \x20 fleet harness list | new | add | rm\n\
         \x20 fleet providers list | add | check\n\
         \x20 fleet credential check | refresh   # where a key comes from, incl. OAuth\n\
         \x20 fleet targets list | add | example # deploy targets as plugins\n\
         \x20 fleet plugins list | example\n\
         \x20 fleet peers list | discover | send\n\
         \x20 fleet sessions list | show\n\
         \x20 fleet memory search \"…\" | fleet lessons list | fleet ledger\n\
         \x20 fleet serve | fleet deploy --to ssh://host --run\n\n\
         `fleet --help` for everything.\n",
    );
    s
}

// ------------------------------------------------------- deploy targets

/// Deploy targets are plugins: the built-in schemes cover a fleet's own
/// machines, and anything else is a script someone can write in twenty lines.
pub fn deploy_targets(c: TargetsCmd, json: bool) -> Result<()> {
    use fleet::deploy_targets::{self, Targets};
    let t = Targets::default_dir();
    match c {
        TargetsCmd::List => {
            if json {
                println!("{}", serde_json::to_string_pretty(&t.all())?);
                return Ok(());
            }
            println!("built-in targets:");
            for (scheme, what) in [
                ("ssh", "stream the binary over ssh, install, start systemd"),
                ("tailscale", "same, over a tailnet name"),
                ("reason", "reason exec a tarball onto a Reason peer"),
                ("docker", "print the docker build/run pair for a context"),
                ("file", "write a bundle and install.sh, touch no server"),
            ] {
                println!("  {scheme:<12} {what}");
            }
            let all = t.all();
            println!("\nplugin targets: {}  ({})", all.len(), t.dir().display());
            for s in &all {
                println!(
                    "  {:<12} {}{}",
                    s.scheme,
                    s.description,
                    if s.example.is_empty() {
                        String::new()
                    } else {
                        format!("\n               e.g. {}", s.example)
                    }
                );
            }
            if all.is_empty() {
                println!("  (none) — `fleet targets example` writes one");
            }
            Ok(())
        }
        TargetsCmd::Add { path } => {
            let s = deploy_targets::install(&path)?;
            println!(
                "installed target `{}` — `fleet deploy --to {}://…` works now",
                s.scheme, s.scheme
            );
            Ok(())
        }
        TargetsCmd::Rm { scheme } => {
            let d = deploy_targets::remove(&scheme)?;
            println!("removed {}", d.display());
            Ok(())
        }
        TargetsCmd::Example => {
            let dir = t.dir().join("myssh");
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("target.toml"), deploy_targets::EXAMPLE_SPEC)?;
            let script = dir.join("deploy.sh");
            std::fs::write(&script, deploy_targets::EXAMPLE_SCRIPT)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755));
            }
            println!("wrote {} — edit it, then `fleet deploy --to myssh://host`", dir.display());
            Ok(())
        }
        TargetsCmd::Probe { target, port } => {
            let (scheme, dest) = target
                .split_once("://")
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .unwrap_or_else(|| {
                    let s = target.split("://").next().unwrap_or("").to_string();
                    (s, String::new())
                });
            let spec = t.get(&scheme).ok_or_else(|| {
                anyhow::anyhow!(
                    "no plugin target named `{scheme}`; built-in schemes are ssh, tailscale, \
                     reason, docker, file"
                )
            })?;
            let req = deploy_targets::Request {
                scheme: scheme.clone(),
                host: dest,
                port,
                prefix: "/usr/local".into(),
                binary: std::env::current_exe()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default(),
                bundle: String::new(),
                env: Default::default(),
            };
            let probe = t.probe(&spec, &req);
            let plan = t.plan(&spec, &req);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "scheme": scheme, "ready": probe.ready, "detail": probe.detail,
                        "commands": plan.commands, "url": plan.url,
                    }))?
                );
                return Ok(());
            }
            println!(
                "{scheme}: {} — {}",
                if probe.ready { "ready" } else { "not ready" },
                probe.detail
            );
            for c in &plan.commands {
                println!("  {c}");
            }
            if !plan.url.is_empty() {
                println!("  api: {}", plan.url);
            }
            Ok(())
        }
        TargetsCmd::Dir => {
            println!("{}", t.dir().display());
            Ok(())
        }
    }
}

// ------------------------------------------------------- credentials

/// Inspect and refresh credentials. A provider may be an env var, a vault
/// command, or an OAuth login; this is how an operator sees which, and how a
/// rotated token is picked up without restarting a service.
pub fn credential(c: CredentialCmd, json: bool) -> Result<()> {
    let ps = fleet::providers::Providers::default_file();
    let creds = fleet::credential::Credentials::new();
    let targets: Vec<fleet::providers::Provider> = match provider_filter(&c) {
        Some(name) => vec![ps
            .get(&name)
            .ok_or_else(|| anyhow::anyhow!("no provider named `{name}`"))?],
        None => ps.all(),
    };
    if targets.is_empty() {
        eprintln!("no providers declared in {}", ps.path().display());
        return Ok(());
    }
    match c {
        CredentialCmd::Check { .. } => {
            let mut rows = Vec::new();
            for p in &targets {
                let src = p.source();
                let (ok, detail) = match p.key_with(&creds, false) {
                    Ok(Some(_)) => (true, "resolved".to_string()),
                    Ok(None) => (false, "source is empty".to_string()),
                    Err(e) => (false, e.to_string()),
                };
                rows.push((p.name.clone(), src.label(), ok, detail));
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &rows
                            .iter()
                            .map(|(n, s, ok, d)| serde_json::json!({
                                "provider": n, "source": s, "ok": ok, "detail": d
                            }))
                            .collect::<Vec<_>>()
                    )?
                );
                return Ok(());
            }
            for (n, s, ok, d) in rows {
                println!(
                    "  {:<16} {:<26} {:<10} {}",
                    n,
                    s,
                    if ok { "ok" } else { "FAILED" },
                    d
                );
            }
            Ok(())
        }
        CredentialCmd::Refresh { .. } => {
            for p in &targets {
                let src = p.source();
                creds.invalidate(&src);
                match p.key_with(&creds, true) {
                    Ok(Some(_)) => println!("  {:<16} refreshed ({})", p.name, src.label()),
                    Ok(None) => println!("  {:<16} source is empty ({})", p.name, src.label()),
                    Err(e) => println!("  {:<16} FAILED: {e}", p.name),
                }
            }
            Ok(())
        }
        CredentialCmd::Show { raw, .. } => {
            let p = &targets[0];
            let src = p.source();
            match p.key_with(&creds, false)? {
                Some(v) => {
                    if raw {
                        println!("{v}");
                    } else {
                        println!("{}: {} ({})", p.name, v, src.label());
                    }
                    Ok(())
                }
                None => anyhow::bail!("no credential for `{}` ({})", p.name, src.label()),
            }
        }
    }
}

/// Which provider a credential command refers to, when one was named.
fn provider_filter(c: &CredentialCmd) -> Option<String> {
    match c {
        CredentialCmd::Check { provider }
        | CredentialCmd::Refresh { provider } => provider.clone(),
        CredentialCmd::Show { provider, .. } => Some(provider.clone()),
    }
}
