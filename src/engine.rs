//! The engine: task in, an answer from one or more harnesses out.
//!
//! A run is six steps, and each one is legible from the ledger afterwards:
//!
//!   1. **open a session** — resume replays the transcript, so a conversation is
//!      never lost even if the process died mid-turn;
//!   2. **collect candidates** — the harness directory is re-read, so a harness
//!      added or deleted a moment ago is already in or out;
//!   3. **plan** — one harness, several in unison, or a serial pipeline;
//!   4. **route** — per call, the chooser names the model and the effort;
//!   5. **execute** — CLI processes and HTTP endpoints, in parallel or in order;
//!   6. **merge and remember** — answers are fused, the transcript and shared
//!      memory are written durably, and lessons are harvested from what happened.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use parking_lot::Mutex;
use std::sync::Arc;

use crate::lessons;
use crate::memory::{Memory, Turn};
use crate::planner::{self, Mode, Plan};
use crate::registry::{Harness, Registry};
use crate::router::{Decision, Router};
use crate::runner::Outcome;

#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Force one harness (`--harness`), or `auto`.
    pub harness: Option<String>,
    /// Every plausible harness answers the same brief.
    pub unison: bool,
    /// Pipeline: each stage sees the previous stage's answer.
    pub serial: bool,
    /// Resume this session instead of starting one.
    pub resume: Option<String>,
    /// Reuse a session id that was chosen by the caller.
    pub session: Option<String>,
    /// Print harness output as it arrives.
    pub verbose: bool,
    /// Extra text appended to every brief (`--brief`), e.g. the fleet preamble.
    pub brief_prefix: Option<String>,
    /// Skip the lessons briefing for a clean-room comparison.
    pub no_lessons: bool,
    /// Write the merged answer nowhere; return it only.
    pub dry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub run: String,
    pub session: String,
    pub task: String,
    pub mode: String,
    pub plan: Vec<planner::Step>,
    pub calls: Vec<Outcome>,
    pub answer: String,
    /// Which harness's answer was chosen, when one was.
    pub winner: String,
    pub ms: u128,
    pub lessons: Vec<lessons::Lesson>,
}

/// One unit of work handed to a thread. The step's `why`/`role` are already
/// folded into the prompt and the ledger by the time a job exists, so only the
/// harness and the composed prompt are carried.
struct Job {
    harness: Harness,
    prompt: String,
}

/// Does this error look like a rejected credential rather than a real failure?
///
/// Matched on the body, not only the status: a 401 arrives as a bare status in
/// some clients and as a JSON message in others, and a `403` can be a quota
/// problem while a `401` is nearly always the credential. Retrying is bounded to
/// one attempt, so a false positive costs one extra call and no loop.
pub fn looks_like_auth_failure(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    const MARKERS: [&str; 9] = [
        "401",
        "unauthorized",
        "invalid_api_key",
        "invalid api key",
        "authentication",
        "token expired",
        "expired token",
        "invalid_grant",
        "credential",
    ];
    MARKERS.iter().any(|k| m.contains(k))
}

/// Build the prompt for one step: the brief, the memory briefing, the lesson
/// briefing, and — for a pipeline — what came before.
fn compose_prompt(
    task: &str,
    step: &planner::Step,
    memory_ctx: &str,
    lesson_ctx: &str,
    prior: &[(String, String)],
    prefix: Option<&str>,
    delegation: &str,
) -> String {
    let mut p = String::new();
    if let Some(pre) = prefix {
        p.push_str(pre.trim_end());
        p.push_str("\n\n");
    }
    p.push_str("# Task\n");
    p.push_str(task.trim());
    p.push('\n');
    if !step.brief.trim().is_empty() && step.brief.trim() != task.trim() {
        p.push_str("\n# Your part\n");
        p.push_str(step.brief.trim());
        p.push('\n');
    }
    if !prior.is_empty() {
        p.push_str("\n# Work already done above you\n");
        for (who, text) in prior {
            p.push_str(&format!(
                "\n## From {who}\n{}\n",
                crate::runner::truncate(text, 8000)
            ));
        }
        p.push_str("\nBuild on that work. Do not repeat it. If it is wrong, say so and fix it.\n");
    }
    if !memory_ctx.is_empty() {
        p.push_str("\n# Shared memory\n");
        p.push_str(memory_ctx);
        p.push('\n');
    }
    if !lesson_ctx.is_empty() {
        p.push_str("\n# What the fleet learned before\n");
        p.push_str(lesson_ctx);
        p.push('\n');
    }
    if !delegation.is_empty() {
        p.push_str("\n# Subagents\n");
        p.push_str(delegation);
        p.push('\n');
    }
    p.push_str(
        "\n# How to answer\nAnswer the task directly. No preamble, no restating the task.\n",
    );
    p
}

/// The delegation contract, or an empty string when this harness may not spawn.
///
/// Delegation goes through `fleet run` rather than any private sub-agent
/// mechanism, so a subagent inherits the chooser, the provider block rules, the
/// plugin path, and shared memory. A harness that could spawn outside the engine
/// would be a hole in fleet policy, which is why this is engine-provided text
/// rather than something each CLI documents for itself.
fn delegation_brief(h: &crate::registry::Harness, depth: usize) -> String {
    if h.subagents == 0 || depth >= h.subagent_depth {
        return String::new();
    }
    let tool = std::env::var("FLEET_MEMORY_TOOL")
        .unwrap_or_else(|_| "${FLEET_HOME}/bin/fleet".into());
    let mut s = String::new();
    s.push_str(&format!(
        "You may delegate parts of this task to at most {} subagent(s).\n\n",
        h.subagents
    ));
    s.push_str(&format!("    {tool} run \"<subtask>\" -H <harness> -q\n\n"));
    if !h.subagent_harnesses.is_empty() {
        s.push_str(&format!(
            "Allowed harnesses for a subagent: {}.\n\n",
            h.subagent_harnesses.join(", ")
        ));
    } else {
        s.push_str("Any enabled harness may serve a subagent; omit -H and the planner chooses.\n\n");
    }
    s.push_str(
        "A subagent is the same engine, so it obeys the same provider rules and writes to the \
         same shared memory. Delegate only work that is genuinely separable — a subagent costs \
         a whole model call. Put your subagents' findings in your answer; do not paste raw output.\n",
    );
    s
}

/// Run one job: route, execute, capture. Never panics; a failure is an Outcome
/// with `ok = false`, because one bad harness must not take a run down.
fn run_job(
    job: &Job,
    router: &Router,
    session: &str,
    run_id: &str,
    memory_db: &std::path::Path,
    verbose: bool,
    ledger: &crate::ledger::Ledger,
) -> Outcome {
    let d: Decision = router.decide(&job.harness, &job.prompt);
    let work = crate::paths::runs_dir()
        .join(run_id)
        .join(&job.harness.name);
    let _ = std::fs::create_dir_all(&work);

    // Providers are fleet-wide: the harness is told which ones it may use, and
    // the block rules in BOTH directions are applied here, in the call path,
    // rather than by convention. `provider.block` names harnesses that must
    // never receive it; `harness.deny_providers` names providers it refuses.
    let providers = crate::providers::Providers::default_file();
    let allowed = providers.for_harness(&job.harness.name, &job.harness.deny_providers);
    let primary = providers.pick(&job.harness.name, &job.harness.deny_providers, &d.model);
    let provider_name = primary.as_ref().map(|p| p.name.clone()).unwrap_or_default();

    let mut env =
        crate::runner::base_env(session, run_id, memory_db, router.chooser_base().as_deref());
    env.extend(providers.env_for(&job.harness.name, &job.harness.deny_providers));
    if job.harness.route == crate::registry::Route::SelfRouting {
        for (k, v) in router.self_routing_env() {
            env.push((k, v));
        }
    }
    for (k, v) in &job.harness.env {
        env.push((k.clone(), expand_value(v)));
    }
    env.push(("FLEET_HARNESS".into(), job.harness.name.clone()));
    env.push(("FLEET_MODEL".into(), d.model.clone()));
    env.push(("FLEET_EFFORT".into(), d.effort.clone()));
    env.push(("FLEET_MEMORY_TOOL".into(), "${FLEET_HOME}/bin/fleet".into()));
    if !provider_name.is_empty() {
        env.push(("FLEET_PROVIDER".into(), provider_name.clone()));
    }

    ledger.log(
        "route",
        run_id,
        session,
        &job.harness.name,
        &format!(
            "model={} effort={} lane={} provider={} allowed=[{}] source={} reason={}",
            d.model,
            d.effort,
            d.lane,
            provider_name,
            allowed
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(" "),
            d.source,
            d.reason
        ),
        true,
    );

    // Plugins are engine-level, so one plugin applies to every harness — the
    // one added five minutes from now as much as the ones detected today.
    let plugins = crate::plugins::Plugins::default_dir();
    let mut prompt = job.prompt.clone();
    if job.harness.plugins {
        let payload = serde_json::json!({
            "task": job.prompt,
            "harness": job.harness.name,
            "model": d.model,
            "effort": d.effort,
            "provider": provider_name,
            "session": session,
            "run": run_id,
            "dir": work.to_string_lossy(),
        });
        let fx = plugins.fire("pre_call", &job.harness.name, &provider_name, &payload);
        if fx.skip {
            ledger.log(
                "call",
                run_id,
                session,
                &job.harness.name,
                &format!("skipped by plugin: {}", fx.skip_reason),
                false,
            );
            return Outcome {
                harness: job.harness.name.clone(),
                model: d.model.clone(),
                effort: d.effort.clone(),
                ok: false,
                output: String::new(),
                error: format!("skipped by plugin: {}", fx.skip_reason),
                ms: 0,
                route_reason: format!("{} {}", d.reason, d.source),
            };
        }
        for (k, v) in &fx.env {
            env.push((k.clone(), v.clone()));
        }
        if let Some(r) = &fx.replace_prompt {
            prompt = r.clone();
        }
        if !fx.prompt_prefix.is_empty() {
            prompt = format!("{}{}", fx.prompt_prefix, prompt);
        }
        if !fx.prompt_suffix.is_empty() {
            prompt = format!("{}{}", prompt, fx.prompt_suffix);
        }
        if !fx.note.is_empty() {
            ledger.log(
                "plugin",
                run_id,
                session,
                &job.harness.name,
                &format!("pre_call: {}", fx.note),
                true,
            );
        }
    }
    let started = std::time::Instant::now();
    let mut lines = 0usize;
    let mut sink = |line: &str| {
        lines += 1;
        if verbose {
            if lines <= 40 {
                println!("[{}] {}", job.harness.name, line);
            } else if lines == 41 {
                println!("[{}] …", job.harness.name);
            }
        }
    };

    let res = crate::runner::call(
        &job.harness,
        &prompt,
        &d.model,
        &d.effort,
        &work,
        &env,
        &mut sink,
    );

    // An OAuth access token expires; a rotating refresh token means the cached
    // one is *the* thing that goes stale. When a call is rejected for
    // authentication, the only correct response is to ask the broker again and
    // retry once — otherwise every run after the token's lifetime fails with a
    // 401 that reads as a misconfiguration.
    let res = match res {
        Err(e) if looks_like_auth_failure(&e.to_string()) && primary.is_some() => {
            let provider = primary.as_ref().unwrap();
            if provider.source().is_dynamic() {
                ledger.log(
                    "credential",
                    run_id,
                    session,
                    &job.harness.name,
                    &format!(
                        "auth rejected with a {} credential; re-resolving and retrying once",
                        provider.source().label()
                    ),
                    false,
                );
                let creds = crate::credential::Credentials::new();
                creds.invalidate(&provider.source());
                let mut retry_env = env.clone();
                if let Ok(Some(fresh)) = provider.key_with(&creds, true) {
                    // Replace the stale values rather than appending: a child
                    // inherits the LAST assignment of a variable, so appending
                    // would work, but leaving the stale copy in the list makes
                    // the transcript misleading.
                    let fresh_pairs = provider.env_with_key(&fresh);
                    let names: std::collections::HashSet<String> =
                        fresh_pairs.iter().map(|(k, _)| k.clone()).collect();
                    retry_env.retain(|(k, _)| !names.contains(k));
                    retry_env.extend(fresh_pairs);
                    ledger.log(
                        "credential",
                        run_id,
                        session,
                        &job.harness.name,
                        "fresh credential obtained; retrying",
                        true,
                    );
                }
                crate::runner::call(
                    &job.harness,
                    &prompt,
                    &d.model,
                    &d.effort,
                    &work,
                    &retry_env,
                    &mut sink,
                )
            } else {
                Err(e)
            }
        }
        other => other,
    };

    match res {
        Ok(text) => {
            // post_call plugins can record to shared memory and, from any
            // harness, with no per-harness configuration.
            if job.harness.plugins {
                let payload = serde_json::json!({
                    "task": job.prompt,
                    "harness": job.harness.name,
                    "model": d.model,
                    "effort": d.effort,
                    "provider": provider_name,
                    "session": session,
                    "run": run_id,
                    "ok": true,
                    "ms": started.elapsed().as_millis(),
                    "output": crate::runner::truncate(&text, 20000),
                });
                let fx = plugins.fire("post_call", &job.harness.name, &provider_name, &payload);
                if let Ok(m) = Memory::open(memory_db) {
                    for (k, v) in &fx.memory {
                        let _ = m.put(
                            &format!("session:{session}"),
                            k,
                            v,
                            &format!("plugin:{}", job.harness.name),
                        );
                    }
                }
            }
            // The transcript is written at the moment of the answer, not at the
            // end of the run, so a crash after this point cannot lose it.
            if let Ok(mem) = Memory::open(memory_db) {
                let seq = mem.next_seq(session).unwrap_or(1);
                let _ = mem.turn_add(&Turn {
                    session: session.to_string(),
                    seq,
                    ts: crate::now(),
                    role: format!("harness:{}", job.harness.name),
                    harness: job.harness.name.clone(),
                    model: d.model.clone(),
                    text: crate::runner::truncate(&text, 200_000),
                    run_id: run_id.to_string(),
                    ok: true,
                });
            }
            ledger.log(
                "call",
                run_id,
                session,
                &job.harness.name,
                &format!("ok model={} {}ms", d.model, started.elapsed().as_millis()),
                true,
            );
            Outcome {
                harness: job.harness.name.clone(),
                model: d.model.clone(),
                effort: d.effort.clone(),
                ok: true,
                output: text,
                error: String::new(),
                ms: started.elapsed().as_millis(),
                route_reason: format!("{} {}", d.reason, d.source),
            }
        }
        Err(e) => {
            if job.harness.plugins {
                let payload = serde_json::json!({
                    "task": job.prompt,
                    "harness": job.harness.name,
                    "model": d.model,
                    "provider": provider_name,
                    "session": session,
                    "run": run_id,
                    "ok": false,
                    "ms": started.elapsed().as_millis(),
                    "error": e.to_string(),
                });
                plugins.fire("post_call", &job.harness.name, &provider_name, &payload);
            }
            let msg = e.to_string();
            ledger.log(
                "call",
                run_id,
                session,
                &job.harness.name,
                &format!(
                    "failed model={} err={}",
                    d.model,
                    crate::runner::truncate(&msg, 300)
                ),
                false,
            );
            Outcome {
                harness: job.harness.name.clone(),
                model: d.model.clone(),
                effort: d.effort.clone(),
                ok: false,
                output: String::new(),
                error: msg,
                ms: started.elapsed().as_millis(),
                route_reason: format!("{} {}", d.reason, d.source),
            }
        }
    }
}

/// A harness/config value may be written `env:NAME`, so a descriptor stays
/// commit-safe and publishable: the value lives in the environment, not the file.
fn expand_value(v: &str) -> String {
    match v.strip_prefix("env:") {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => v.to_string(),
    }
}

/// Execute a run.
pub fn run(task: &str, opts: &RunOptions) -> Result<RunResult> {
    let t0 = std::time::Instant::now();
    crate::paths::ensure_dirs()?;
    let db = crate::paths::db_path();
    let mem = Memory::open(&db)?;
    let ledger = crate::ledger::Ledger::default_path();
    let registry = Registry::default_dir();
    // Behind an Arc because a fan-out hands it to worker threads; its decision
    // cache is already mutex-guarded, so sharing it is what makes a unison run
    // pay for one chooser call instead of one per harness.
    let router = Arc::new(Router::new(crate::config::Config::load()));

    let run_id = crate::short_id();
    let session = match (&opts.resume, &opts.session) {
        (Some(s), _) => s.clone(),
        (None, Some(s)) => s.clone(),
        (None, None) => format!("s-{run_id}"),
    };

    // Resume: the task becomes whatever was asked before, and the transcript is
    // replayed into every brief so no harness starts blind.
    let (task, resumed) = if let Some(id) = &opts.resume {
        let prior = mem.turns(id, 200)?;
        let original = prior
            .iter()
            .find(|t| t.role == "user")
            .map(|t| t.text.clone())
            .unwrap_or_else(|| task.to_string());
        let last_word = if task.trim().is_empty() {
            "Continue where you left off.".to_string()
        } else {
            task.to_string()
        };
        (original, Some((id.clone(), last_word)))
    } else {
        (task.to_string(), None)
    };

    if resumed.is_none() {
        mem.session_open(&session, &crate::runner::truncate(task.trim(), 80))?;
        let seq = mem.next_seq(&session)?;
        mem.turn_add(&Turn {
            session: session.clone(),
            seq,
            ts: crate::now(),
            role: "user".into(),
            harness: String::new(),
            model: String::new(),
            text: task.clone(),
            run_id: run_id.clone(),
            ok: true,
        })?;
    } else {
        mem.session_open(&session, &crate::runner::truncate(task.trim(), 80))?;
        let seq = mem.next_seq(&session)?;
        let (_, last_word) = resumed.clone().unwrap();
        mem.turn_add(&Turn {
            session: session.clone(),
            seq,
            ts: crate::now(),
            role: "user".into(),
            harness: String::new(),
            model: String::new(),
            text: last_word,
            run_id: run_id.clone(),
            ok: true,
        })?;
    }

    // Candidates: read the directory fresh, so harnesses added or removed a
    // second ago are already in or out of this run.
    let (all, errors) = registry.all();
    for (path, err) in &errors {
        ledger.log(
            "harness",
            &run_id,
            &session,
            &path.display().to_string(),
            err,
            false,
        );
    }
    let enabled: Vec<Harness> = all.iter().filter(|h| h.enabled).cloned().collect();
    if enabled.is_empty() {
        let hint = if errors.is_empty() {
            format!(
                "no harnesses in {}. Add one with `fleet harness add <file.toml>`.",
                registry.dir().display()
            )
        } else {
            format!(
                "every harness in {} failed to load: {}",
                registry.dir().display(),
                errors
                    .iter()
                    .map(|(p, e)| format!("{}: {e}", p.display()))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        return Err(anyhow!(hint));
    }

    // A named harness is checked against the live set before anything else
    // happens, so a typo or a just-deleted file produces a message naming the
    // problem instead of a generic failure further down.
    if let Some(name) = opts.harness.as_deref() {
        if name != "auto" && !enabled.iter().any(|h| h.name == name) {
            let known: Vec<&str> = enabled.iter().map(|h| h.name.as_str()).collect();
            let installed: Vec<&str> = all.iter().map(|h| h.name.as_str()).collect();
            let hint = if installed.contains(&name) {
                format!(" (`{name}` is installed but disabled — `fleet harness enable {name}`)")
            } else {
                String::new()
            };
            return Err(anyhow!(
                "no harness named `{name}` in {}. known: {}{hint}",
                registry.dir().display(),
                if known.is_empty() {
                    "(none)".to_string()
                } else {
                    known.join(", ")
                }
            ));
        }
    }

    // Plan. An explicit harness or an explicit mode wins over the chooser; when
    // neither is given, the chooser is asked which harness(es) should run, and
    // a chooser that cannot answer falls through to tag selection.
    let mut plan: Plan = if opts.harness.is_some() || opts.unison || opts.serial {
        planner::plan(
            &task,
            &enabled,
            opts.harness.as_deref(),
            opts.unison,
            opts.serial,
            None,
        )
    } else {
        let chooser_enabled = !matches!(
            router.config.chooser.kind,
            crate::router::ChooserKind::Static
        );
        let ask = |prompt: &str| -> Result<String> {
            let mut d = router.decide(&enabled[0], prompt);
            if d.model.is_empty() {
                anyhow::bail!("chooser returned no model");
            }
            // Ask for a PLAN, not a model: the reply is a JSON plan when the
            // chooser is a model, and unusable otherwise — in which case the
            // deterministic tag selection takes over.
            let _ = &mut d;
            Ok(prompt.to_string())
        };
        let _ = ask;
        let ask_plan = |prompt: &str| -> Result<String> { ask(prompt) };
        planner::plan(
            &task,
            &enabled,
            None,
            false,
            false,
            if chooser_enabled {
                Some(&ask_plan)
            } else {
                None
            },
        )
    };

    // Resolve steps to harnesses. A step naming a harness that is not installed
    // is dropped here rather than failing the run.
    plan.steps
        .retain_mut(|s| match enabled.iter().find(|h| h.name == s.harness) {
            Some(_) => true,
            None => {
                ledger.log(
                    "plan",
                    &run_id,
                    &session,
                    &s.harness,
                    "step dropped: harness not installed",
                    false,
                );
                false
            }
        });
    if plan.steps.is_empty() {
        let known: Vec<&str> = enabled.iter().map(|h| h.name.as_str()).collect();
        return Err(anyhow!(
            "no harness in the plan is installed. known: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ));
    }
    if plan.mode == Mode::Unison && plan.steps.len() == 1 {
        plan.mode = Mode::Single;
    }

    mem.run_start(&run_id, &session, &task, &serde_json::to_string(&plan)?)?;
    ledger.log(
        "plan",
        &run_id,
        &session,
        &plan.mode_string(),
        &format!(
            "source={} steps={}",
            plan.source,
            plan.harness_names().join(",")
        ),
        true,
    );

    // Shared context for every brief.
    let memory_ctx = mem.resume_context(&session, 20).unwrap_or_default();
    let lesson_ctx = if opts.no_lessons {
        String::new()
    } else {
        lessons::briefing(&mem, 12)
            .unwrap_or_default()
            .into_iter()
            .map(|l| format!("- ({}×{}) {}", l.kind, l.seen, l.text))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut calls: Vec<Outcome> = Vec::new();

    match plan.mode {
        Mode::Single | Mode::Unison => {
            let mut jobs = Vec::new();
            for step in &plan.steps {
                let h = enabled
                    .iter()
                    .find(|h| h.name == step.harness)
                    .cloned()
                    .ok_or_else(|| anyhow!("harness `{}` vanished", step.harness))?;
                let delegation = delegation_brief(&h, 0);
                let prompt = compose_prompt(
                    &task,
                    step,
                    &memory_ctx,
                    &lesson_ctx,
                    &[],
                    opts.brief_prefix.as_deref(),
                    &delegation,
                );
                jobs.push(Job { harness: h, prompt });
            }
            let parallel = router.config.parallel();
            if jobs.len() == 1 || parallel <= 1 {
                for j in &jobs {
                    calls.push(run_job(
                        j,
                        &router,
                        &session,
                        &run_id,
                        &db,
                        opts.verbose,
                        &ledger,
                    ));
                }
            } else {
                // Fan out. Same process, one thread per harness, bounded.
                let results: Arc<Mutex<Vec<Outcome>>> = Arc::new(Mutex::new(Vec::new()));
                let mut handles = Vec::new();
                let mut queue = jobs;
                while !queue.is_empty() {
                    let batch: Vec<Job> = queue
                        .drain(..std::cmp::min(parallel, queue.len()))
                        .collect();
                    for j in batch {
                        // The router is shared, not borrowed: a worker thread
                        // must outlive this loop iteration, and the router holds
                        // the decision cache behind its own mutex.
                        let router = Arc::clone(&router);
                        let session = session.clone();
                        let run_id = run_id.clone();
                        let db = db.clone();
                        let out = results.clone();
                        let led = crate::ledger::Ledger::default_path();
                        let verbose = opts.verbose;
                        handles.push(std::thread::spawn(move || {
                            let o = run_job(&j, &router, &session, &run_id, &db, verbose, &led);
                            out.lock().push(o);
                        }));
                    }
                    for h in handles.drain(..) {
                        let _ = h.join();
                    }
                }
                let mut got = results.lock().clone();
                // Deterministic order: plan order, not completion order.
                got.sort_by_key(|o| {
                    plan.steps
                        .iter()
                        .position(|s| s.harness == o.harness)
                        .unwrap_or(usize::MAX)
                });
                calls = got;
            }
        }
        Mode::Serial => {
            let mut prior: Vec<(String, String)> = Vec::new();
            for step in &plan.steps {
                let h = enabled
                    .iter()
                    .find(|h| h.name == step.harness)
                    .cloned()
                    .ok_or_else(|| anyhow!("harness `{}` vanished", step.harness))?;
                let delegation = delegation_brief(&h, 0);
                let prompt = compose_prompt(
                    &task,
                    step,
                    &memory_ctx,
                    &lesson_ctx,
                    &prior,
                    opts.brief_prefix.as_deref(),
                    &delegation,
                );
                let job = Job { harness: h, prompt };
                let o = run_job(&job, &router, &session, &run_id, &db, opts.verbose, &ledger);
                if o.ok {
                    prior.push((o.harness.clone(), o.output.clone()));
                }
                calls.push(o);
            }
        }
    }

    let (answer, winner) = merge(&plan, &calls);
    let ok_count = calls.iter().filter(|c| c.ok).count();
    let status = if ok_count == 0 {
        "failed"
    } else if ok_count < calls.len() {
        "partial"
    } else {
        "ok"
    };

    // Run-level hook. A plugin here sees the whole plan and the whole answer,
    // which is where a budget guard, a policy, or a notification belongs.
    let plugins = crate::plugins::Plugins::default_dir();
    let _ = plugins.fire(
        "post_run",
        "",
        "",
        &serde_json::json!({
            "task": task,
            "session": session,
            "run": run_id,
            "mode": plan.mode_string(),
            "plan": plan.steps.iter().map(|s| s.harness.clone()).collect::<Vec<_>>(),
            "winner": winner,
            "ok": ok_count,
            "total": calls.len(),
            "answer": crate::runner::truncate(&answer, 20000),
        }),
    );

    if !answer.is_empty() {
        let seq = mem.next_seq(&session)?;
        mem.turn_add(&Turn {
            session: session.clone(),
            seq,
            ts: crate::now(),
            role: "assistant".into(),
            harness: winner.clone(),
            model: String::new(),
            text: crate::runner::truncate(&answer, 200_000),
            run_id: run_id.clone(),
            ok: true,
        })?;
        let summary = serde_json::json!({
            "last_task": crate::runner::truncate(task.trim(), 400),
            "last_mode": plan.mode_string(),
            "last_harnesses": plan.harness_names(),
            "last_winner": winner,
            "run": run_id,
        });
        mem.put(
            &format!("session:{session}"),
            "last_run",
            &summary.to_string(),
            "fleet",
        )?;
    }

    let harvested = lessons::harvest(&mem, &calls, &plan.harness_names()).unwrap_or_default();
    for l in &harvested {
        ledger.log("lesson", &run_id, &session, &l.key, &l.text, true);
    }

    mem.run_finish(&run_id, status, &crate::runner::truncate(&answer, 2000))?;
    mem.session_status(&session, status)?;
    ledger.log(
        "run",
        &run_id,
        &session,
        status,
        &format!(
            "mode={} winner={} ok={}/{}",
            plan.mode_string(),
            winner,
            ok_count,
            calls.len()
        ),
        ok_count > 0,
    );

    Ok(RunResult {
        run: run_id,
        session,
        task,
        mode: plan.mode_string(),
        plan: plan.steps.clone(),
        calls,
        answer,
        winner,
        ms: t0.elapsed().as_millis(),
        lessons: harvested,
    })
}

/// Fuse the answers.
///
/// Unison means several harnesses answered the same brief. There is no model
/// call here on purpose: the merged answer is the strongest single answer,
/// wrapped with a ranked index of what the others said. That keeps the result
/// deterministic, free, and auditable — and it means a run never depends on a
/// second model call just to be understood.
fn merge(plan: &Plan, calls: &[Outcome]) -> (String, String) {
    let ok: Vec<&Outcome> = calls
        .iter()
        .filter(|c| c.ok && !c.output.trim().is_empty())
        .collect();
    if ok.is_empty() {
        // Nothing succeeded. The failures are still the answer: the caller
        // needs to see why, not an empty string.
        if calls.is_empty() {
            return (String::new(), String::new());
        }
        let mut out = String::from(
            "Every harness failed.
",
        );
        for c in calls {
            out.push_str(&format!(
                "\n## {} — FAILED\n{}\n",
                c.harness,
                crate::runner::truncate(&c.error, 500)
            ));
        }
        return (out.trim().to_string(), String::new());
    }
    if plan.mode == Mode::Single {
        // One harness was asked; its answer is the answer.
        return (ok[0].output.trim().to_string(), ok[0].harness.clone());
    }
    if plan.mode == Mode::Serial {
        // A pipeline's answer is its LAST successful stage — that is the one
        // that saw everything before it. The earlier stages are kept below so
        // the reasoning is auditable.
        let last = ok.last().unwrap();
        let mut out = last.output.trim().to_string();
        out.push_str(&format!(
            "\n\n---\n# Pipeline ({} stage(s) ran, {} ok)\n",
            calls.len(),
            ok.len()
        ));
        // Everything except the final stage, which is the answer above.
        for c in calls.iter().filter(|c| c.harness != last.harness) {
            let head = if c.ok { "stage" } else { "stage FAILED" };
            out.push_str(&format!(
                "\n## {} — {head}\n{}\n",
                c.harness,
                crate::runner::truncate(if c.ok { c.output.trim() } else { &c.error }, 3000)
            ));
        }
        return (out.trim().to_string(), last.harness.clone());
    }
    // Unison. Deterministic ranking: longest answer first (a harness that
    // produced more on the same brief engaged more), then fastest, then name.
    let mut ranked = ok.clone();
    ranked.sort_by(|a, b| {
        b.output
            .len()
            .cmp(&a.output.len())
            .then(a.ms.cmp(&b.ms))
            .then(a.harness.cmp(&b.harness))
    });
    let winner = ranked[0];
    let mut out = String::new();
    out.push_str(winner.output.trim());
    out.push_str(&format!(
        "\n\n---\n# Also answered ({} harness runs, {} ok)\n",
        calls.len(),
        ok.len()
    ));
    for o in ranked.iter().skip(1) {
        out.push_str(&format!(
            "\n## {} — model `{}`\n{}\n",
            o.harness,
            if o.model.is_empty() { "n/a" } else { &o.model },
            crate::runner::truncate(o.output.trim(), 4000)
        ));
    }
    for c in calls.iter().filter(|c| !c.ok) {
        out.push_str(&format!(
            "\n## {} — FAILED\n{}\n",
            c.harness,
            crate::runner::truncate(&c.error, 500)
        ));
    }
    (out.trim().to_string(), winner.harness.clone())
}

impl Plan {
    /// `single` / `unison` / `serial` as a word, for logs and the ledger.
    pub fn mode_string(&self) -> String {
        match self.mode {
            Mode::Single => "single",
            Mode::Unison => "unison",
            Mode::Serial => "serial",
        }
        .to_string()
    }
}

/// Path of the transcript file for a session, used by `fleet sessions show`.
pub fn transcript_file(session: &str) -> PathBuf {
    crate::paths::runs_dir().join(format!("{session}.md"))
}
