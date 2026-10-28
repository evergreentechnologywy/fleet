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


/// This process's delegation depth, from `FLEET_DEPTH`.
///
/// A subagent is a separate `fleet run` process, so depth cannot be counted in
/// memory — it is handed down the environment by `runner::base_env` and read
/// back here. An unset or unparsable value means a top-level run.
pub fn depth_from_env() -> usize {
    std::env::var("FLEET_DEPTH")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
}

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
    /// Call a model directly: no harness, no chooser. Set with `--model`.
    pub direct_model: Option<String>,
    /// Which provider serves `direct_model`.
    pub direct_provider: Option<String>,
    /// A key to use for `direct_provider`, overriding the declared one.
    pub direct_api_key: Option<String>,
    /// Working directory for the run, from `--session-dir` (omp compatibility).
    pub cwd: Option<String>,
    /// Resume the most recent session (`-c`, omp compatibility).
    pub continue_session: bool,
    /// Give each harness its own `git worktree` off HEAD, instead of letting
    /// every job write into the caller's directory.
    ///
    /// Parallel agents editing one tree is the failure mode of multi-agent
    /// coding: two harnesses fix different bugs and overwrite each other, and the
    /// damage looks like a bad model rather than a bad schedule. With this on,
    /// each job gets an isolated checkout and the operator merges what they want.
    pub worktree: bool,
    /// How deep in a delegation tree this run is.
    ///
    /// Subagents are separate `fleet run` PROCESSES, so depth cannot be counted in
    /// memory — it is handed down through the environment (`FLEET_DEPTH`) and read
    /// back here. Without it `subagent_depth` was dead configuration: every
    /// generation passed the literal 0, so the guard never tripped and delegation
    /// recursed without a bound.
    pub depth: usize,

    /// Fan out to this many harnesses in parallel (`--subagents`).
    ///
    /// `0` leaves the count to the planner. A fan-out is a UNISON with a floor on
    /// the number of harnesses, which is why it shares the executor rather than
    /// growing its own: the swarm is an ability of the engine, not a service
    /// beside it.
    pub subagents: usize,
    /// Restrict a fan-out to these harnesses. Empty means any enabled one.
    pub subagent_harnesses: Vec<String>,
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
    // The path to the running binary, so the advertised command is one that
    // exists. It used to be the literal `${FLEET_HOME}/bin/fleet`, which is
    // never expanded here and is not where the installer puts the binary —
    // delegation advertised a command that could not run.
    let tool = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "fleet".into());
    let mut s = String::new();
    s.push_str(&format!(
        "You may delegate parts of this task to at most {} subagent(s).\n\n",
        h.subagents
    ));
    s.push_str(&format!("    {tool} run \"<subtask>\" -H <harness> -q\n\n"));
    s.push_str(&format!(
        "For several independent subtasks at once, one call runs them in parallel:\n\n    \
         {tool} run \"<subtask>\" --subagents {} -q\n\n",
        h.subagents
    ));
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



/// What every job needs and nothing per-call varies. Grouped because passing the
/// same six things to three functions made the signatures unreadable and hid the
/// one argument that DOES differ.
struct RunCtx<'a> {
    session: &'a str,
    run_id: &'a str,
    db: &'a std::path::Path,
    verbose: bool,
    ledger: &'a crate::ledger::Ledger,
    enabled: &'a [crate::registry::Harness],
    /// Where the HARNESS runs. This is the caller's directory, not a scratch
    /// dir: a coding task handed to an agent that cannot see the repo is a task
    /// the agent cannot do. Measured 2026-10-27: `fleet run` from inside the
    /// fleet repo told the harness to list its cwd and it answered EMPTY,
    /// because every job was being run in a fresh runs/<id>/<harness> dir.
    cwd: &'a std::path::Path,
    /// Give each job its own worktree off HEAD (`--worktree`).
    worktree: bool,
}

/// Run a job, and if it FAILS, try the configured alternates in order.
///
/// This is the reliability layer the owner asked for: a fleet that returns "every
/// harness failed" is not usable, and the failure is almost never the task — it is
/// one CLI's auth, one provider's outage, one model that refused. So a failure
/// walks a chain of alternates, and the FIRST success wins.
///
/// The chain comes from `[fallback] chain` in fleet.toml, else a sane default of
/// the direct-provider harnesses, which need no CLI and therefore cannot fail for
/// a CLI-shaped reason. A direct provider is the bottom of every chain because it
/// is the one thing that still works when a CLI, a chooser, or a wrapper is the
/// broken part.
///
/// Never retries the harness that just failed, and never retries more than the
/// configured budget, so a task cannot turn into an unbounded storm.
fn run_job_with_fallback(job: &Job, router: &Router, ctx: &RunCtx) -> Outcome {
    // Only what the fallback loop itself needs; run_job reads the rest from ctx.
    let RunCtx {
        session,
        run_id,
        ledger,
        enabled,
        ..
    } = *ctx;
    let first = run_job(job, router, ctx);
    if first.ok {
        return first;
    }

    let cfg = crate::config::Config::load();
    // The default chain is HARNESSES, not providers: a provider is reachable only
    // through a harness, so naming one here would be naming something that cannot
    // run a task. The direct-provider harnesses are the bottom of the chain
    // because they need no CLI, and therefore cannot fail for a CLI-shaped reason.
    let chain: Vec<String> = if cfg.fallback.chain.is_empty() {
        vec!["muse".into(), "liquid".into(), "openrouter".into()]
    } else {
        cfg.fallback.chain.clone()
    };
    let budget = cfg.fallback.max_attempts.unwrap_or(3);
    let mut attempts = 0usize;
    let mut last = first;

    for name in chain {
        if attempts >= budget {
            break;
        }
        if name == job.harness.name {
            continue;
        }
        let Some(h) = enabled.iter().find(|h| h.name == name) else {
            continue;
        };
        if !h.enabled {
            continue;
        }
        attempts += 1;
        ledger.log(
            "fallback",
            run_id,
            session,
            &h.name,
            &format!(
                "after `{}` failed: {}",
                job.harness.name,
                crate::runner::truncate(&last.error, 160)
            ),
            true,
        );
        // The alternate gets the same prompt; the chooser decides its model, so a
        // harness that routes differently still gets a valid decision.
        let alt = Job {
            harness: h.clone(),
            prompt: job.prompt.clone(),
        };
        let o = run_job(&alt, router, ctx);
        if o.ok {
            // Say which one actually answered, so the transcript never implies
            // the first harness did the work.
            let mut o = o;
            // Name the substitution in the RESULT, not only in the prose. A
            // caller that inspects `calls` (or a test that checks who answered)
            // otherwise sees only the substitute and concludes the requested
            // harness worked.
            o.fell_back_from = job.harness.name.clone();
            o.output = format!(
                "[answered by {} after {} failed]\n{}",
                o.harness, job.harness.name, o.output
            );
            return o;
        }
        last = o;
    }
    last
}


/// Give one job its own git worktree off HEAD, so parallel harnesses cannot
/// overwrite each other.
///
/// Returns None when isolation is not possible, and says why: a directory that is
/// not a git repo, a repo with no commits yet, or a `git` that is absent. Falling
/// back to the shared directory is the right call there — refusing to run would
/// make the flag unusable outside a repo — but the caller is told, because a
/// silently-shared directory is exactly the collision this exists to prevent.
fn make_worktree(repo: &std::path::Path, run_id: &str, harness: &str) -> Option<std::path::PathBuf> {
    if !repo.join(".git").exists() {
        return None;
    }
    let dir = crate::paths::runs_dir()
        .join(run_id)
        .join(format!("wt-{harness}"));
    let _ = std::fs::remove_dir_all(&dir);
    // Detached HEAD: a worktree per job must not fight over a branch name.
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&dir)
        .arg("HEAD")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(dir)
}

/// Drop a worktree that carries no changes; KEEP one that does.
///
/// A clean checkout is pure noise, but a dirty one is the work product: merging
/// what you want is the whole reason `--worktree` exists, so deleting it would
/// throw the result away. Either way the path is said out loud — a run that left
/// edits behind must not be silent about where they are.
fn release_worktree(repo: &std::path::Path, wt: &std::path::Path, harness: &str) {
    let dirty = std::process::Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        // If we cannot read the status we must assume it is dirty: pruning a
        // checkout because `git status` failed is how work gets deleted.
        .unwrap_or(true);
    if dirty {
        eprintln!(
            "fleet: `{harness}` left changes in {} (kept for you to merge)",
            wt.display()
        );
        return;
    }
    let removed = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "remove", "--force"])
        .arg(wt)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !removed {
        let _ = std::fs::remove_dir_all(wt);
    }
    // The per-run directory exists only to hold these checkouts.
    if let Some(parent) = wt.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

/// Remove a run directory once nothing of value is left in it.
///
/// Each run creates `runs/<id>/<harness>/`, which is where a `--worktree`
/// checkout lives. Without this those directories accumulate forever: measured
/// 2026-10-27, 590 of them on the hub, every one holding only EMPTY per-harness
/// subdirectories.
///
/// Only empty entries are removed, so a checkout kept for merging — the whole
/// point of `--worktree` — is never touched.
fn prune_run_dir(run_id: &str) {
    let dir = crate::paths::runs_dir().join(run_id);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            // Fails when non-empty, which is exactly the guarantee we want.
            let _ = std::fs::remove_dir(&p);
        }
    }
    let _ = std::fs::remove_dir(&dir);
}

/// Run a batch of jobs, in parallel when the config allows it.
///
/// Extracted from `run` so a subagent fan-out reuses the SAME executor rather
/// than growing a second, subtly different one — the two paths drifted apart the
/// first time, and the parallel branch silently lost the fallback chain.
///
/// `order` is the harness order to restore afterwards: a fan-out returns in
/// completion order, and a caller that shows results must not reorder between
/// runs just because one harness was slow.
///
/// EVERY job goes through `run_job_with_fallback`, including the parallel ones.
/// The parallel branch used to call `run_job` directly, which meant any plan with
/// two or more concurrent harnesses — the normal unison case, since
/// `max_parallel` defaults to 4 — skipped the `[fallback] chain` entirely. That
/// is invisible when it works, and looks like "every harness failed" when it does
/// not, which is exactly the failure the chain exists to prevent.
fn run_jobs(
    jobs: Vec<Job>,
    router: &Arc<Router>,
    ctx: &RunCtx,
    parallel: usize,
    order: &[String],
) -> Vec<Outcome> {
    if jobs.len() == 1 || parallel <= 1 {
        let got: Vec<Outcome> = jobs
            .iter()
            .map(|j| run_job_with_fallback(j, router, ctx))
            .collect();
        prune_run_dir(ctx.run_id);
        return got;
    }
    // Fan out. Same process, one thread per harness, bounded.
    let results: Arc<Mutex<Vec<Outcome>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    let mut queue = jobs;
    while !queue.is_empty() {
        let batch: Vec<Job> = queue
            .drain(..std::cmp::min(parallel, queue.len()))
            .collect();
        for j in batch {
            // The router is shared, not borrowed: a worker thread must outlive
            // this loop iteration, and the router holds the decision cache behind
            // its own mutex.
            let router = Arc::clone(router);
            let session = ctx.session.to_string();
            let run_id = ctx.run_id.to_string();
            let db = ctx.db.to_path_buf();
            let out = results.clone();
            let led = crate::ledger::Ledger::default_path();
            let verbose = ctx.verbose;
            let enabled = ctx.enabled.to_vec();
            let cwd = ctx.cwd.to_path_buf();
            let worktree = ctx.worktree;
            handles.push(std::thread::spawn(move || {
                let ctx = RunCtx {
                    session: &session,
                    run_id: &run_id,
                    db: &db,
                    verbose,
                    ledger: &led,
                    enabled: &enabled,
                    cwd: &cwd,
                    worktree,
                };
                let o = run_job_with_fallback(&j, &router, &ctx);
                out.lock().push(o);
            }));
        }
        for h in handles.drain(..) {
            let _ = h.join();
        }
    }
    let mut got = results.lock().clone();
    // Deterministic order: caller order, not completion order.
    got.sort_by_key(|o| {
        order
            .iter()
            .position(|n| n == &o.harness)
            .unwrap_or(usize::MAX)
    });
    prune_run_dir(ctx.run_id);
    got
}

/// Run one job: route, execute, capture. Never panics; a failure is an Outcome
/// with `ok = false`, because one bad harness must not take a run down.
fn run_job(job: &Job, router: &Router, ctx: &RunCtx) -> Outcome {
    let RunCtx {
        session,
        run_id,
        db: memory_db,
        verbose,
        ledger,
        enabled: _,
        cwd,
        worktree: _,
    } = *ctx;
    let d: Decision = router.decide(&job.harness, &job.prompt);
    // Two different directories, and conflating them was the bug:
    //   `work`  = this run's artifact dir (logs, dumps) — per run, per harness
    //   `cwd`   = where the HARNESS actually runs — the caller's directory
    // The harness used to get `work` as its cwd, so an agent asked to work on a
    // repo was placed in an empty scratch dir and could see nothing. Measured
    // 2026-10-27: `fleet run` inside the fleet repo, asking the harness to list
    // its cwd, answered EMPTY.
    let work = crate::paths::runs_dir()
        .join(run_id)
        .join(&job.harness.name);
    let _ = std::fs::create_dir_all(&work);
    // Isolation is per JOB, not per run: two harnesses in one unison must not
    // share a checkout, which is the whole point of the flag.
    let isolated = if ctx.worktree {
        make_worktree(cwd, run_id, &job.harness.name)
    } else {
        None
    };
    if ctx.worktree && isolated.is_none() {
        eprintln!(
            "fleet: --worktree requested but `{}` is not a usable git repo; \
             `{}` shares the caller's directory",
            cwd.display(),
            job.harness.name
        );
    }
    let harness_cwd = isolated.as_deref().unwrap_or(cwd);

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
    // The path to THIS binary, resolved rather than templated. It used to be the
    // literal string `${FLEET_HOME}/bin/fleet`, which is wrong twice over: it is
    // never expanded on the way in, and `$FLEET_HOME/bin/` is not where the
    // installer puts the binary. Delegation advertised a command that could not
    // run, so the contract was cosmetic.
    let memory_tool = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "fleet".to_string());
    env.push(("FLEET_MEMORY_TOOL".into(), memory_tool.clone()));
    // The delegation brief is composed before the run, so it cannot read the
    // per-job env; it falls back to this same value.
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
                tokens_in: 0,
                tokens_out: 0,
                fell_back_from: String::new(),
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
        harness_cwd,
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
                    harness_cwd,
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
        Ok(called) => {
            // Split the transport result immediately: everything below wants the
            // text, and only the Outcome and the ledger want the usage.
            let text = called.text;
            let (tokens_in, tokens_out) = (called.tokens_in, called.tokens_out);
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
                &format!(
                    "ok model={} {}ms{}",
                    d.model,
                    started.elapsed().as_millis(),
                    // Only when the transport actually reported it: a CLI
                    // harness has no usage block, and printing tok=0/0 would
                    // read as a real measurement of zero.
                    if tokens_in + tokens_out > 0 {
                        format!(" tok={tokens_in}/{tokens_out}")
                    } else {
                        String::new()
                    }
                ),
                true,
            );
            if let Some(wt) = &isolated {
                release_worktree(cwd, wt, &job.harness.name);
            }
            Outcome {
                harness: job.harness.name.clone(),
                model: d.model.clone(),
                effort: d.effort.clone(),
                ok: true,
                output: text,
                error: String::new(),
                ms: started.elapsed().as_millis(),
                route_reason: format!("{} {}", d.reason, d.source),
                tokens_in,
                tokens_out,
                fell_back_from: String::new(),
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
            if let Some(wt) = &isolated {
                release_worktree(cwd, wt, &job.harness.name);
            }
            Outcome {
                harness: job.harness.name.clone(),
                model: d.model.clone(),
                effort: d.effort.clone(),
                ok: false,
                output: String::new(),
                error: msg,
                ms: started.elapsed().as_millis(),
                route_reason: format!("{} {}", d.reason, d.source),
                tokens_in: 0,
                tokens_out: 0,
                fell_back_from: String::new(),
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


/// Call one model directly: no harness, no planner, no chooser.
///
/// The provider is chosen explicitly when named (a declared provider, or a bare
/// base URL), else the first enabled provider that accepts the model. The key is
/// the one that provider declares, or `--api-key` when the caller supplies one —
/// which is what lets a one-off endpoint be used without declaring it.
#[allow(clippy::too_many_arguments)] // a one-shot path with no shared context yet
fn run_direct(
    task: &str,
    model: &str,
    provider: Option<&str>,
    api_key: Option<&str>,
    session: &str,
    run_id: &str,
    mem: &Memory,
    ledger: &crate::ledger::Ledger,
    dry: bool,
) -> Result<RunResult> {
    let started = std::time::Instant::now();
    let providers = crate::providers::Providers::default_file();

    // Resolve the endpoint: a named provider, a bare URL, or the first enabled
    // provider that accepts this model.
    let (base, key) = match provider {
        Some(p) if p.starts_with("http") => {
            let k = api_key
                .map(|s| s.to_string())
                .or_else(|| std::env::var("FLEET_API_KEY").ok())
                .ok_or_else(|| anyhow::anyhow!("a bare URL provider needs --api-key"))?;
            (p.to_string(), k)
        }
        Some(name) => {
            let p = providers
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("no provider named `{name}`; see `fleet providers list`"))?;
            let k = api_key
                .map(|s| s.to_string())
                .or_else(|| p.key_with(&crate::credential::Credentials::new(), false).ok().flatten())
                .ok_or_else(|| anyhow::anyhow!("provider `{name}` has no usable credential"))?;
            (p.base_url.clone(), k)
        }
        None => {
            let p = providers
                .all()
                .into_iter()
                .filter(|p| p.enabled && p.available())
                .find(|p| p.accepts(model))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no enabled provider accepts `{model}`; name one with --provider, \
                         or see `fleet providers list`"
                    )
                })?;
            let k = p
                .key_with(&crate::credential::Credentials::new(), false)
                .ok()
                .flatten()
                .unwrap_or_default();
            (p.base_url.clone(), k)
        }
    };

    let url = crate::runner::join_url(&base, "/v1/chat/completions");
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": task}],
        // Reasoning counts against the same budget on several lanes, so the floor
        // is generous: a small cap returns finish_reason=length with no content.
        "max_tokens": 8192,
    });

    let mut req = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(300))
        .set("Content-Type", "application/json");
    if !key.is_empty() {
        req = req.set("Authorization", &format!("Bearer {key}"));
    }
    let resp = req.send_string(&body.to_string());

    // Usage comes out of the same body as the message, so the direct path gets
    // real numbers rather than the zeros a CLI harness has to report.
    let direct_usage;
    let text = match resp {
        Ok(r) => {
            let v: serde_json::Value = r.into_json().unwrap_or_default();
            direct_usage = (
                v.pointer("/usage/prompt_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .max(v.pointer("/usage/input_tokens").and_then(|x| x.as_u64()).unwrap_or(0)),
                v.pointer("/usage/completion_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .max(v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()).unwrap_or(0)),
            );
            v.get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string()
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            ledger.log(
                "direct",
                run_id,
                session,
                model,
                &format!("{url} -> {code}: {}", crate::runner::truncate(&body, 200)),
                false,
            );
            anyhow::bail!("{model} via {url} -> HTTP {code}: {}", crate::runner::truncate(&body, 300));
        }
        Err(e) => {
            ledger.log("direct", run_id, session, model, &format!("{url}: {e}"), false);
            anyhow::bail!("{model} via {url}: {e}");
        }
    };

    ledger.log("direct", run_id, session, model, &format!("{url} ok"), true);

    if !dry {
        let _ = mem.session_open(session, &crate::runner::truncate(task, 60));
        let _ = mem.turn_add(&crate::memory::Turn {
            session: session.to_string(),
            seq: mem.next_seq(session)?,
            ts: crate::now(),
            role: "assistant".into(),
            harness: "direct".into(),
            model: model.to_string(),
            text: text.clone(),
            run_id: run_id.to_string(),
            ok: true,
        });
    }

    Ok(RunResult {
        run: run_id.to_string(),
        session: session.to_string(),
        task: task.to_string(),
        mode: "direct".into(),
        plan: Vec::new(),
        winner: "direct".into(),
        lessons: Vec::new(),
        calls: vec![Outcome {
            harness: "direct".into(),
            model: model.to_string(),
            effort: String::new(),
            ok: true,
            output: text.clone(),
            error: String::new(),
            ms: started.elapsed().as_millis(),
            route_reason: format!("direct call to {model} at {url}"),
            tokens_in: direct_usage.0,
            tokens_out: direct_usage.1,
            fell_back_from: String::new(),
        }],
        answer: text,
        ms: started.elapsed().as_millis(),
    })
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

    // `-c` (omp compatibility) means "continue the most recent session". It is
    // resolved HERE rather than in the CLI so an API caller gets the same
    // behaviour, and so a caller that also passed `--resume` still wins by
    // naming a session explicitly.
    let mut opts = opts.clone();
    // The harness the caller NAMED, before any substitution. Recorded so the
    // result can say "you asked for X, Y answered" rather than silently
    // reporting success for a harness that never ran.
    let requested_harness: Option<String> = opts.harness.clone();
    if opts.continue_session && opts.resume.is_none() && opts.session.is_none() {
        if let Some(last) = mem.sessions(1)?.first() {
            opts.resume = Some(last.id.clone());
        }
    }

    // `--session-dir` (omp compatibility) becomes the working directory, so a
    // caller that kept per-chat state in a directory keeps it. An explicit
    // FLEET_CWD wins, because that is the fleet's own knob.
    if let Some(dir) = opts
        .cwd
        .clone()
        .filter(|d| !d.is_empty())
        .or_else(|| std::env::var("FLEET_CWD").ok())
    {
        let path = std::path::PathBuf::from(&dir);
        if path.is_dir() {
            let _ = std::env::set_current_dir(&path);
        } else {
            let _ = std::fs::create_dir_all(&path);
            let _ = std::env::set_current_dir(&path);
        }
    }

    // Where the harnesses run. `--session-dir`/`FLEET_CWD` may have moved the
    // process above; otherwise this is the directory the user invoked `fleet`
    // from — the one that holds their repo. Captured once, before any planner
    // work, so every job in the run sees the same directory.
    let work_cwd: std::path::PathBuf = opts
        .cwd
        .clone()
        .filter(|d| !d.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var("FLEET_CWD").ok().map(std::path::PathBuf::from))
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| crate::paths::home())
        });

    let session = match (&opts.resume, &opts.session) {
        (Some(s), _) => s.clone(),
        (None, Some(s)) => s.clone(),
        (None, None) => format!("s-{run_id}"),
    };

    // A DIRECT model call skips the planner, the harness, and the chooser: the
    // caller named the model, so nothing is left to decide. This is the path for
    // "I just want the model", and it is also the fleet's floor — it depends on
    // no CLI, so no CLI-shaped failure can reach it.
    if let Some(model) = opts.direct_model.as_deref().filter(|m| !m.is_empty()) {
        return run_direct(
            task,
            model,
            opts.direct_provider.as_deref(),
            opts.direct_api_key.as_deref(),
            &session,
            &run_id,
            &mem,
            &ledger,
            opts.dry,
        );
    }

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
    //
    // A harness that EXISTS but is disabled or un-runnable is different from a
    // typo, and it must NOT end the run: the whole point of the fallback chain is
    // that an unavailable first choice still produces an answer. So a real name
    // that cannot run is reported and the chain is used; only an unknown name is
    // an error, because then there is nothing to fall FROM.
    let mut opts = opts.clone();
    if let Some(name) = opts.harness.as_deref() {
        if name != "auto" && !enabled.iter().any(|h| h.name == name) {
            let known: Vec<&str> = enabled.iter().map(|h| h.name.as_str()).collect();
            let installed: Vec<&str> = all.iter().map(|h| h.name.as_str()).collect();
            if installed.contains(&name) {
                // Substitute the first runnable rung of the fallback chain. This
                // is what makes "reliable" true: asking for a harness that cannot
                // run must not be the end of the task, and the substitution is
                // REPORTED so the transcript never implies the requested harness
                // did the work.
                let cfg = crate::config::Config::load();
                let chain = if cfg.fallback.chain.is_empty() {
                    vec!["muse".to_string(), "liquid".to_string(), "opencode-free".to_string()]
                } else {
                    cfg.fallback.chain.clone()
                };
                let sub = chain
                    .iter()
                    .find(|n| enabled.iter().any(|h| h.name == **n))
                    .cloned();
                match sub {
                    Some(sub) => {
                        eprintln!(
                            "fleet: `{name}` is installed but not runnable — running `{sub}` instead"
                        );
                        opts.harness = Some(sub);
                    }
                    None => {
                        // Nothing in the chain can run either. The run proceeds
                        // with the named harness and fails loudly, which is the
                        // honest outcome: a fleet with no runnable harness cannot
                        // pretend otherwise.
                        eprintln!("fleet: `{name}` is not runnable and the fallback chain has no runnable member");
                    }
                }
            } else {
                return Err(anyhow!(
                    "no harness named `{name}` in {}. known: {}",
                    registry.dir().display(),
                    if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known.join(", ")
                    }
                ));
            }
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

    // Fan-out: a unison run with a FLOOR on how many harnesses answer. This is the
    // swarm expressed as an ability of the engine — one process, one run, N
    // harnesses on the same brief concurrently — so it reuses the unison
    // executor, the chooser, the fallback chain, and shared memory rather than
    // duplicating any of them in a separate service.
    if opts.subagents > 0 {
        let mut pool: Vec<&crate::registry::Harness> = enabled
            .iter()
            .filter(|h| {
                opts.subagent_harnesses.is_empty() || opts.subagent_harnesses.contains(&h.name)
            })
            .collect();
        // Highest priority first, so a restricted pool still picks the best ones.
        pool.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.name.cmp(&b.name)));
        if pool.len() < opts.subagents {
            eprintln!(
                "fleet: --subagents {} but only {} harness(es) available{}; using {}",
                opts.subagents,
                pool.len(),
                if opts.subagent_harnesses.is_empty() {
                    String::new()
                } else {
                    format!(" in {:?}", opts.subagent_harnesses)
                },
                pool.len()
            );
        }
        let want = opts.subagents.min(pool.len());
        if want == 0 {
            return Err(anyhow!(
                "--subagents {} but no harness is available to serve them",
                opts.subagents
            ));
        }
        plan.mode = Mode::Unison;
        plan.steps = pool
            .iter()
            .take(want)
            .map(|h| planner::Step {
                harness: h.name.clone(),
                brief: task.clone(),
                role: "work".into(),
                why: format!("fan-out (--subagents {})", opts.subagents),
            })
            .collect();
        plan.source = "fan-out".into();
        plan.notes = format!("{} harness(es) in parallel via --subagents", want);
        ledger.log(
            "plan",
            &run_id,
            &session,
            "fan-out",
            &format!("{} harness(es): {}", want, plan.harness_names().join(",")),
            true,
        );
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
                let delegation = delegation_brief(&h, opts.depth);
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
            let ctx = RunCtx {
                session: &session,
                run_id: &run_id,
                db: &db,
                verbose: opts.verbose,
                ledger: &ledger,
                enabled: &enabled,
                cwd: &work_cwd,
                worktree: opts.worktree,
            };
            let order: Vec<String> = plan.steps.iter().map(|s| s.harness.clone()).collect();
            calls = run_jobs(jobs, &router, &ctx, parallel, &order);
        }
        Mode::Serial => {
            let mut prior: Vec<(String, String)> = Vec::new();
            let ctx = RunCtx {
                session: &session,
                run_id: &run_id,
                db: &db,
                verbose: opts.verbose,
                ledger: &ledger,
                enabled: &enabled,
                cwd: &work_cwd,
                worktree: opts.worktree,
            };
            for step in &plan.steps {
                let h = enabled
                    .iter()
                    .find(|h| h.name == step.harness)
                    .cloned()
                    .ok_or_else(|| anyhow!("harness `{}` vanished", step.harness))?;
                let delegation = delegation_brief(&h, opts.depth);
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
                // Same reliability layer as every other mode. A pipeline stage
                // that fails currently contributes nothing to `prior`, so the
                // next stage reasons over a hole — with the chain, the stage
                // still produces an answer.
                let o = run_job_with_fallback(&job, &router, &ctx);
                if o.ok {
                    prior.push((o.harness.clone(), o.output.clone()));
                }
                calls.push(o);
            }
        }
    }

    // Stamp the substitution on the RESULT, not only in the prose. A caller that
    // inspects `calls` — or a test that checks who answered — otherwise sees only
    // the substitute and concludes the requested harness worked. That mistake
    // briefly enabled nine harnesses that had never run.
    if let Some(want) = requested_harness.as_deref().filter(|w| *w != "auto") {
        for c in calls.iter_mut() {
            if c.harness != want && c.fell_back_from.is_empty() {
                c.fell_back_from = want.to_string();
            }
        }
    }

    let (answer, winner) = merge(&plan, &calls, opts.dry);
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
            // The model, not just the harness. This used to be an empty string,
            // so "which model produced this answer" was unrecoverable after the
            // fact: the harness was named but the model that wrote the text was
            // not. Three of four harnesses asked for exactly this independently —
            // a merged answer is unattributable without it, and a bad claim
            // cannot be traced back to the model that made it.
            model: calls
                .iter()
                .find(|c| c.harness == winner && c.ok)
                .map(|c| c.model.clone())
                .unwrap_or_default(),
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
fn merge(plan: &Plan, calls: &[Outcome], dry: bool) -> (String, String) {
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
    // `dry` means "give me the answer, nothing else" — which is what a chat
    // gateway needs. The appendix stays for an interactive run, where seeing
    // which other harnesses answered is the point of a unison call.
    if dry {
        return (out, winner.harness.clone());
    }
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
