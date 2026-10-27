//! Command implementations — part 1: run, harness, providers.

use anyhow::{bail, Result};

use fleet::engine::RunOptions;
use fleet::memory::Memory;
use fleet::registry::Registry;

use crate::{HarnessCmd, ProvidersCmd, RunArgs};

// ---------------------------------------------------------------- run

pub fn run(a: RunArgs, json: bool) -> Result<()> {
    let task = a.task.join(" ");
    let resume = match a.resume.as_deref() {
        Some("last") => {
            let mem = Memory::open_default()?;
            let s = mem.sessions(1)?;
            match s.first() {
                Some(s) => Some(s.id.clone()),
                None => bail!("no sessions yet"),
            }
        }
        Some(id) => Some(id.to_string()),
        None => None,
    };
    if task.trim().is_empty() && resume.is_none() {
        bail!("give me a task, or `--resume last`");
    }
    let brief_prefix = match &a.brief {
        Some(b) if b.starts_with('@') || std::path::Path::new(b).is_file() => {
            Some(std::fs::read_to_string(b.trim_start_matches('@'))?)
        }
        other => other.clone(),
    };
    let opts = RunOptions {
        harness: Some(a.harness.clone()),
        unison: a.unison,
        serial: a.serial,
        resume,
        session: a.session,
        verbose: a.verbose,
        brief_prefix,
        no_lessons: a.no_lessons,
        // `-q` means "the answer and nothing else": it suppresses the progress
        // lines AND the "Also answered" appendix, which is what a chat caller
        // needs and what a human comparing harnesses does not.
        dry: a.quiet,
        direct_model: a.model.clone(),
        direct_provider: a.provider.clone(),
        direct_api_key: a.api_key.clone(),
        // `--session-dir` becomes the run's working directory; `-c` resumes the
        // most recent session. Both exist so a caller written against omp keeps
        // working when it is pointed at fleet.
        cwd: a.session_dir.clone(),
        continue_session: a.continue_session,
        // The delegation depth this process was started at, so a subagent's
        // subagents are bounded too. Set by the parent through FLEET_DEPTH.
        depth: fleet::engine::depth_from_env(),
        subagents: a.subagents,
        subagent_harnesses: a.subagent_harnesses.clone(),
    };
    let res = fleet::engine::run(&task, &opts)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&res)?);
        return Ok(());
    }
    if !a.quiet {
        eprintln!(
            "fleet: run {} session {} mode {} · {} call(s), {} ok · {}ms",
            res.run,
            res.session,
            res.mode,
            res.calls.len(),
            res.calls.iter().filter(|c| c.ok).count(),
            res.ms
        );
        for c in &res.calls {
            eprintln!(
                "  {:<12} {:<28} {}{}",
                c.harness,
                if c.model.is_empty() { "-" } else { &c.model },
                if c.ok { "ok" } else { "FAILED" },
                if c.ok {
                    format!(" ({}ms)", c.ms)
                } else {
                    format!(": {}", fleet::runner::truncate(&c.error, 160))
                }
            );
        }
    }
    println!("{}", res.answer);
    Ok(())
}

// ---------------------------------------------------------------- harness

pub fn harness(c: HarnessCmd, json: bool) -> Result<()> {
    let reg = Registry::default_dir();
    match c {
        HarnessCmd::List { reload } => {
            if reload {
                reg.invalidate();
            }
            let (all, errors) = reg.all();
            if json {
                let rows: Vec<serde_json::Value> = all
                    .iter()
                    .map(|h| {
                        let (ok, why) = fleet::runner::probe(h);
                        serde_json::json!({
                            "name": h.name, "kind": format!("{:?}", h.kind).to_lowercase(),
                            "route": format!("{:?}", h.route).to_lowercase(),
                            "tags": h.tags, "priority": h.priority,
                            "enabled": h.enabled, "available": ok, "detail": why,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dir": reg.dir().to_string_lossy(),
                        "harnesses": rows,
                        "errors": errors.iter().map(|(p, e)| serde_json::json!({
                            "path": p.to_string_lossy(), "error": e
                        })).collect::<Vec<_>>(),
                    }))?
                );
                return Ok(());
            }
            println!("harness dir: {}", reg.dir().display());
            if all.is_empty() {
                println!("  (none) — add one with `fleet harness new`");
            }
            for h in &all {
                let (ok, why) = fleet::runner::probe(h);
                println!(
                    "  {:<14} {:<14} {:<8} {:<4} {:>3}  {}{}",
                    h.name,
                    format!("{:?}", h.kind).to_lowercase(),
                    format!("{:?}", h.route).to_lowercase(),
                    if h.enabled { "on" } else { "off" },
                    h.priority,
                    if ok { "● " } else { "○ " },
                    why
                );
                if !h.description.is_empty() {
                    println!("                 {}", h.description);
                }
            }
            for (p, e) in &errors {
                println!("  !! {}: {e}", p.display());
            }
            println!(
                "\n{} live · {} load error(s) · re-read on every call, so add/rm needs no restart",
                all.len(),
                errors.len()
            );
            Ok(())
        }
        HarnessCmd::Show { name } => {
            let h = reg.get(&name)?;
            let (ok, why) = fleet::runner::probe(&h);
            if json {
                println!("{}", serde_json::to_string_pretty(&h)?);
                return Ok(());
            }
            println!("{}  ({})", h.name, h.description);
            println!(
                "  file        {}",
                h.source
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
            println!("  kind        {:?}", h.kind);
            println!("  route       {:?}", h.route);
            if let Some(c) = &h.command {
                println!("  command     {c} {}", h.args.join(" "));
            }
            if let Some(s) = &h.http {
                println!("  base_url    {}", s.base_url);
            }
            println!(
                "  models      {}",
                if h.models.is_empty() {
                    "any".to_string()
                } else {
                    h.models.join(", ")
                }
            );
            println!("  tags        {}", h.tags.join(", "));
            println!("  available   {} ({why})", if ok { "yes" } else { "no" });
            println!("  enabled     {}", h.enabled);
            if !h.deny_providers.is_empty() {
                println!("  denies      {}", h.deny_providers.join(", "));
            }
            Ok(())
        }
        HarnessCmd::Probe { name } => {
            let h = reg.get(&name)?;
            let (ok, why) = fleet::runner::probe(&h);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "harness": name, "available": ok, "detail": why
                    }))?
                );
            } else {
                println!(
                    "{name}: {} — {why}",
                    if ok { "available" } else { "unavailable" }
                );
            }
            Ok(())
        }
        HarnessCmd::Add { file } => {
            let h = reg.install(&file)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&h)?);
            } else {
                println!(
                    "installed `{}` → {}",
                    h.name,
                    h.source
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
                println!("it is live now; no restart needed.");
            }
            Ok(())
        }
        HarnessCmd::New {
            template,
            name,
            base_url,
            key_env,
            dry_run,
        } => new_harness(&reg, &template, name, base_url, key_env, dry_run, json),
        HarnessCmd::Rm { name } => {
            let p = reg.remove(&name)?;
            println!("removed {name} ({})", p.display());
            println!("it is out of the next run; a run already using it is unaffected.");
            Ok(())
        }
        HarnessCmd::Enable { name } => harness_set_enabled(&reg, &name, true),
        HarnessCmd::Disable { name } => harness_set_enabled(&reg, &name, false),
    }
}

/// Flip one descriptor's `enabled` flag in place, leaving the file present so a
/// harness can be parked and un-parked without losing its definition.
fn harness_set_enabled(reg: &Registry, name: &str, on: bool) -> Result<()> {
    let h = reg.get(name)?;
    let path = h
        .source
        .clone()
        .ok_or_else(|| anyhow::anyhow!("`{name}` has no file"))?;
    let text = std::fs::read_to_string(&path)?;
    let updated = set_toml_bool(&text, "enabled", on);
    std::fs::write(&path, updated)?;
    println!("{name} {}", if on { "enabled" } else { "disabled" });
    Ok(())
}

/// Replace a top-level `key = true|false`, or add it.
pub fn set_toml_bool(text: &str, key: &str, value: bool) -> String {
    let mut out = Vec::new();
    let mut done = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with(&format!("{key} =")) || t.starts_with(&format!("{key}=")) {
            let indent = &line[..line.len() - t.len()];
            out.push(format!("{indent}{key} = {value}"));
            done = true;
        } else {
            out.push(line.to_string());
        }
    }
    if !done {
        out.push(format!("{key} = {value}"));
    }
    let mut s = out.join("\n");
    s.push('\n');
    s
}

fn new_harness(
    reg: &Registry,
    template: &str,
    name: Option<String>,
    base_url: Option<String>,
    key_env: Option<String>,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let body = match template {
        "openai-compat" | "provider" | "openai" => {
            let url = base_url
                .clone()
                .ok_or_else(|| anyhow::anyhow!("--base-url is required for this template"))?;
            let key = key_env
                .clone()
                .unwrap_or_else(|| "OPENAI_API_KEY".to_string());
            let n = name.clone().unwrap_or_else(|| "provider".to_string());
            fleet::templates::openai_compat(
                &n,
                &url,
                &key,
                fleet::templates::default_model_for(&url),
                &["chat", "long"],
            )
        }
        "blank" => {
            let n = name.clone().unwrap_or_else(|| "myharness".to_string());
            format!(
                r#"name = "{n}"
description = ""
kind = "cli"

command = "echo"
args = ["{{prompt}}"]
prompt_on_stdin = false

route = "jev"
models = ["*"]

tags = []
priority = 50
cost_hint = 0.0
enabled = false
timeout_s = 1800
"#
            )
        }
        other => {
            let presets = fleet::templates::presets_for_path();
            let p = presets
                .iter()
                .find(|p| p.name == other)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no template `{other}`; try omp, codex, opencode, claude, grok, gemini, verifier, openai-compat, blank"
                    )
                })?;
            // A preset is written verbatim, except for `name`, which is the
            // one field that must match the file. Renaming is an ordinary
            // string swap: the descriptor is data, not code.
            match name.as_deref() {
                Some(n) if n != p.name => p.body.replacen(
                    &format!("name = \"{}\"", p.name),
                    &format!("name = \"{n}\""),
                    1,
                ),
                _ => p.body.clone(),
            }
        }
    };
    if dry_run {
        println!("{body}");
        return Ok(());
    }
    let dir = reg.dir();
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(".new.toml");
    std::fs::write(&tmp, &body)?;
    let h = reg.install(&tmp);
    let _ = std::fs::remove_file(&tmp);
    match h {
        Ok(h) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&h)?);
            } else {
                println!(
                    "created `{}` at {}",
                    h.name,
                    h.source.unwrap_or_default().display()
                );
                println!("edit it, then `fleet run \"…\"` — it is picked up on the next call.");
            }
            Ok(())
        }
        Err(e) => {
            // The descriptor is wrong; show it so it can be fixed in place.
            eprintln!("fleet: {e}");
            eprintln!("--- generated descriptor ---\n{body}");
            Err(anyhow::anyhow!("the generated descriptor did not load"))
        }
    }
}

// ---------------------------------------------------------------- providers

pub fn providers(c: ProvidersCmd, json: bool) -> Result<()> {
    let ps = fleet::providers::Providers::default_file();
    match c {
        ProvidersCmd::List => {
            let all = ps.all();
            if json {
                println!("{}", serde_json::to_string_pretty(&ps.audit())?);
                return Ok(());
            }
            println!("providers: {}  ({})", ps.path().display(), all.len());
            for p in &all {
                println!(
                    "  {:<16} {:<38} {:<9} {:<22} {}",
                    p.name,
                    p.base_url,
                    if p.available() { "ready" } else { "unusable" },
                    // The SOURCE is shown, never a value: an operator needs to
                    // know whether a provider is an env var, a vault command, or
                    // an OAuth login before they can fix a rejection.
                    p.source().label(),
                    if p.block.is_empty() {
                        String::new()
                    } else {
                        format!("blocked for {}", p.block.join(", "))
                    }
                );
            }
            if all.is_empty() {
                println!("  (none) — a harness can still use its own credentials");
            }
            Ok(())
        }
        ProvidersCmd::Add {
            name,
            base_url,
            key_env,
            model,
            block,
        } => {
            let p = fleet::providers::Provider {
                name: name.clone(),
                base_url,
                key_env,
                extra: Default::default(),
                models: if model.is_empty() {
                    vec!["*".into()]
                } else {
                    model
                },
                block,
                enabled: true,
                notes: String::new(),
                        credential: None,
};
            ps.put(&p)?;
            println!(
                "provider `{name}` saved — every harness may use it on the next call{}",
                if p.block.is_empty() {
                    String::new()
                } else {
                    format!(", except {}", p.block.join(", "))
                }
            );
            Ok(())
        }
        ProvidersCmd::Rm { name } => {
            ps.remove(&name)?;
            println!("provider `{name}` removed");
            Ok(())
        }
        ProvidersCmd::Check { harness } => {
            let reg = Registry::default_dir();
            let (all, _) = reg.all();
            let targets: Vec<&fleet::registry::Harness> = match &harness {
                Some(n) => all.iter().filter(|h| &h.name == n).collect(),
                None => all.iter().collect(),
            };
            if targets.is_empty() {
                bail!("no such harness");
            }
            for h in targets {
                let allowed = ps.for_harness(&h.name, &h.deny_providers);
                let blocked: Vec<String> = ps
                    .all()
                    .iter()
                    .filter(|p| !p.allows(&h.name, &h.deny_providers))
                    .map(|p| p.name.clone())
                    .collect();
                println!(
                    "{}: can use [{}]{}",
                    h.name,
                    allowed
                        .iter()
                        .map(|p| p.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    if blocked.is_empty() {
                        String::new()
                    } else {
                        format!("  · denied [{}]", blocked.join(", "))
                    }
                );
            }
            Ok(())
        }
        ProvidersCmd::Detect { write } => {
            let found = fleet::providers::detect_from_env();
            for p in &found {
                println!("found {} → {} (key {})", p.name, p.base_url, p.key_env);
            }
            if found.is_empty() {
                println!("no provider keys found in this environment");
            }
            if write {
                for p in &found {
                    ps.put(p)?;
                }
                println!(
                    "wrote {} provider(s) to {}",
                    found.len(),
                    ps.path().display()
                );
            }
            Ok(())
        }
    }
}
