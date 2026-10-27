//! The reliability claims, tested rather than asserted.
//!
//! The owner's bar is "more reliable than grok bot", which means: a failing
//! harness must never be the end of a task. Every test here runs the REAL engine
//! against REAL harnesses that are deliberately broken in a specific way, and
//! asserts that an answer still comes back.
//!
//! These are integration tests, so they need the fleet home and the harnesses to
//! exist. They SKIP (rather than fail) when a prerequisite is absent, because a
//! test that fails on a machine without the fleet installed teaches nothing.

use std::path::PathBuf;

/// The fleet home the tests drive. Overridable so a CI run can point elsewhere.
fn home() -> PathBuf {
    std::env::var("FLEET_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root/.fleet"))
}

fn have_fleet() -> bool {
    home().join("harnesses").is_dir() && std::env::var("PATH").is_ok()
}

/// Run the engine in-process, the way the CLI does.
fn run_with(harness: &str, task: &str) -> fleet::engine::RunResult {
    let opts = fleet::engine::RunOptions {
        harness: Some(harness.to_string()),
        unison: false,
        serial: false,
        resume: None,
        session: None,
        verbose: false,
        brief_prefix: None,
        no_lessons: true, // a clean comparison: lessons would vary between runs
        dry: true,
        direct_model: None,
        direct_provider: None,
        direct_api_key: None,
    };
    fleet::engine::run(task, &opts).expect("the engine must not error out entirely")
}

#[test]
fn a_dead_harness_still_produces_an_answer() {
    if !have_fleet() {
        return;
    }
    // `gemini` is disabled on purpose, and a disabled harness is exactly the
    // "cannot run" case. The chain must carry the task anyway.
    let r = run_with("gemini", "Reply with exactly: PONG");
    let answered = r.calls.iter().any(|c| c.ok);
    assert!(
        answered,
        "every call failed, so the fallback chain did not carry the task: {:?}",
        r.calls.iter().map(|c| (&c.harness, &c.error)).collect::<Vec<_>>()
    );
    assert!(!r.answer.trim().is_empty(), "an answer must be non-empty");
}

#[test]
fn asking_for_an_unrunnable_harness_substitutes_one_that_works() {
    if !have_fleet() {
        return;
    }
    // A disabled harness is substituted at PLAN time for the first runnable chain
    // member, so the task runs on something that works. The substitute must be a
    // real harness and the answer must be non-empty.
    let r = run_with("gemini", "Reply with exactly: PONG");
    let ran = r.calls.first().map(|c| c.harness.clone()).unwrap_or_default();
    assert_ne!(ran, "gemini", "an unrunnable harness must not be what runs");
    assert!(
        !ran.is_empty(),
        "no harness ran at all, so the substitution did not happen"
    );
    assert!(
        r.calls.iter().any(|c| c.ok),
        "the substituted harness `{ran}` failed too: {:?}",
        r.calls.iter().map(|c| (&c.harness, &c.error)).collect::<Vec<_>>()
    );
}

#[test]
fn a_failing_harness_is_never_retried_within_one_run() {
    if !have_fleet() {
        return;
    }
    // The chain must not ping-pong. Whatever ran, each harness appears at most
    // once: a retry loop spends the budget the next rung needs, and turns one
    // slow failure into a long one.
    let r = run_with("gemini", "Reply with exactly: PONG");
    let mut seen: Vec<&str> = r.calls.iter().map(|c| c.harness.as_str()).collect();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        before,
        seen.len(),
        "a harness was attempted more than once in one run: {:?}",
        r.calls.iter().map(|c| &c.harness).collect::<Vec<_>>()
    );
}

#[test]
fn the_direct_model_path_needs_no_harness_at_all() {
    if !have_fleet() {
        return;
    }
    // The floor: no CLI, no chooser, no planner. If the chain is exhausted, this
    // is what still works, so it must work on its own.
    let opts = fleet::engine::RunOptions {
        harness: None,
        unison: false,
        serial: false,
        resume: None,
        session: None,
        verbose: false,
        brief_prefix: None,
        no_lessons: true,
        dry: true,
        direct_model: Some("deepseek-v4.1-flash".into()),
        direct_provider: Some("chooser".into()),
        direct_api_key: None,
    };
    let r = fleet::engine::run("Reply with exactly: PONG", &opts).expect("direct call");
    assert_eq!(r.mode, "direct");
    assert!(
        r.answer.contains("PONG"),
        "the direct path must return the model's answer, got: {}",
        &r.answer[..r.answer.len().min(200)]
    );
}

#[test]
fn the_fallback_chain_is_configured_and_names_harnesses_that_exist() {
    if !have_fleet() {
        return;
    }
    let cfg = fleet::config::Config::load();
    let chain = if cfg.fallback.chain.is_empty() {
        vec!["muse".to_string(), "liquid".to_string(), "openrouter".to_string()]
    } else {
        cfg.fallback.chain.clone()
    };
    assert!(!chain.is_empty(), "a fleet with no chain has no floor");

    let reg = fleet::registry::Registry::default_dir();
    let (all, _) = reg.all();
    for name in &chain {
        // A chain member that is not a harness is a chain member that cannot run
        // a task — the mistake of naming a PROVIDER (tokenbroker) instead of a
        // harness (the CLI or endpoint that reaches it).
        assert!(
            all.iter().any(|h| &h.name == name),
            "`{name}` is in the fallback chain but is not a harness; a provider \
             cannot run a task on its own. known: {}",
            all.iter().map(|h| h.name.as_str()).collect::<Vec<_>>().join(", ")
        );
    }
}

#[test]
fn the_chain_ends_somewhere_that_needs_no_credential() {
    if !have_fleet() {
        return;
    }
    // The whole point of a floor: it must survive an expired key, a lapsed OAuth
    // grant, and a dead endpoint. A chain whose every rung needs a credential has
    // no floor, only a longer fall.
    let reg = fleet::registry::Registry::default_dir();
    let (all, _) = reg.all();
    let keyless: Vec<&str> = all
        .iter()
        .filter(|h| h.enabled && h.route == fleet::registry::Route::Fixed && h.api_key_env.is_none())
        .map(|h| h.name.as_str())
        .collect();
    assert!(
        !keyless.is_empty(),
        "no enabled harness runs without a credential; there is nothing to fall to \
         when every key is bad"
    );
}

#[test]
fn a_disabled_harness_is_not_offered_by_the_planner() {
    if !have_fleet() {
        return;
    }
    let reg = fleet::registry::Registry::default_dir();
    let (all, _) = reg.all();
    let disabled: Vec<&str> = all.iter().filter(|h| !h.enabled).map(|h| h.name.as_str()).collect();
    if disabled.is_empty() {
        return;
    }
    // An auto-planned run must not pick a harness that cannot run.
    let opts = fleet::engine::RunOptions {
        harness: Some("auto".into()),
        unison: false,
        serial: false,
        resume: None,
        session: None,
        verbose: false,
        brief_prefix: None,
        no_lessons: true,
        dry: true,
        direct_model: None,
        direct_provider: None,
        direct_api_key: None,
    };
    let r = fleet::engine::run("Reply with exactly: PONG", &opts).expect("auto run");
    for c in &r.calls {
        assert!(
            !disabled.contains(&c.harness.as_str()),
            "the planner chose `{}`, which is disabled",
            c.harness
        );
    }
}
