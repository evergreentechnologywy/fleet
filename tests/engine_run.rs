//! End-to-end runs against a deterministic harness, so the whole path is
//! exercised without a model, a network, or a credential.
//!
//! These tests point `FLEET_HOME` at a temporary directory. That variable is
//! process-global, so the tests that depend on it are combined into one test
//! function rather than run concurrently — a small loss of granularity for a
//! real guarantee.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

/// `FLEET_HOME` is process-global, so any test that sets it must hold this for
/// its whole body. Cargo runs the tests in one process by default, so without
/// the lock they clobber each other's home directory and the failures look like
/// engine bugs.
/// `FLEET_HOME` is process-global, so any test that sets it must hold this for
/// its whole body. Cargo runs the tests in one process by default, so without
/// the lock they clobber each other's home directory and the failures look like
/// engine bugs.
static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn env_lock() -> MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        // A previous test panicked while holding it; the data is a unit, so
        // recovering is correct and keeps one failure from cascading.
        Err(p) => p.into_inner(),
    }
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleet-engine-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("harnesses")).unwrap();
    d
}

/// A harness that echoes a fixed marker, so the answer is verifiable.
fn write_echo(dir: &Path, name: &str, marker: &str, priority: i32, verifier: bool) {
    let body = format!(
        r#"name = "{name}"
description = "echoes {marker}"
kind = "cli"
command = "sh"
args = ["-c", "cat >/dev/null 2>&1; printf '%s' '{marker}'"]
prompt_on_stdin = true
route = "native"
tags = ["test"]
priority = {priority}
verifier = {verifier}
enabled = true
timeout_s = 30
"#
    );
    std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
}

/// A harness that reports what it was asked, so prompt composition is testable.
fn write_reporter(dir: &Path, name: &str) {
    let body = format!(
        r#"name = "{name}"
description = "prints the prompt it received"
kind = "cli"
command = "sh"
args = ["-c", "printf '%s' \"$(cat)\""]
prompt_on_stdin = true
route = "native"
tags = ["test"]
priority = 5
enabled = true
timeout_s = 30
"#
    );
    std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
}

fn write_failing(dir: &Path, name: &str) {
    let body = format!(
        r#"name = "{name}"
description = "always fails"
kind = "cli"
command = "sh"
args = ["-c", "echo 'deliberate failure' >&2; exit 3"]
prompt_on_stdin = true
route = "native"
enabled = true
timeout_s = 30
"#
    );
    std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
}

/// Everything that needs `FLEET_HOME`, in one sequential test.
#[test]
fn a_run_plans_routes_executes_merges_and_remembers() {
    let _g = env_lock();
    let h = home("full");
    std::env::set_var("FLEET_HOME", &h);
    // No chooser: the deterministic path, which is also the from-zero path.
    std::env::remove_var("FLEET_CHOOSER_OPENAI_BASE");

    write_echo(&h.join("harnesses"), "alpha", "ANSWER-ALPHA", 90, false);
    write_echo(&h.join("harnesses"), "beta", "ANSWER-BETA", 80, false);
    write_echo(&h.join("harnesses"), "checker", "CHECKED", 0, true);

    // ---- single: the highest-priority harness answers --------------------
    let opts = fleet::engine::RunOptions {
        harness: Some("alpha".into()),
        ..Default::default()
    };
    let r = fleet::engine::run("say something", &opts).unwrap();
    assert_eq!(r.mode, "single");
    assert_eq!(r.winner, "alpha");
    assert!(r.answer.contains("ANSWER-ALPHA"), "got: {}", r.answer);
    assert_eq!(r.calls.len(), 1);
    assert!(r.calls[0].ok);

    // ---- unison: several answer, the merge keeps the others --------------
    let opts = fleet::engine::RunOptions {
        unison: true,
        ..Default::default()
    };
    let r = fleet::engine::run("compare the options", &opts).unwrap();
    assert_eq!(r.mode, "unison");
    assert!(r.calls.len() >= 2, "more than one harness ran");
    assert!(
        r.answer.contains("Also answered"),
        "the other answers are appended, not discarded: {}",
        fleet::runner::truncate(&r.answer, 400)
    );
    // Every successful harness's answer is present somewhere.
    for o in r.calls.iter().filter(|c| c.ok) {
        assert!(
            r.answer.contains(o.output.trim()),
            "{}'s answer is in the merge",
            o.harness
        );
    }

    // ---- the transcript is durable and ordered ---------------------------
    let mem = fleet::memory::Memory::open(&fleet::paths::db_path()).unwrap();
    let turns = mem.turns(&r.session, 100).unwrap();
    assert!(turns.len() >= 3, "user turn, harness turns, assistant turn");
    assert_eq!(turns[0].role, "user");
    assert_eq!(turns[0].text, "compare the options");
    assert_eq!(turns.last().unwrap().role, "assistant");
    assert!(
        turns.iter().any(|t| t.harness == "alpha"),
        "the serving harness is recorded per turn"
    );

    // ---- memory: last version wins, and the session remembers its run ----
    let last = mem
        .get(&format!("session:{}", r.session), "last_run")
        .unwrap()
        .expect("the session records its last run");
    assert!(last.contains("unison"));

    // ---- ledger: decisions and calls are durable -------------------------
    let led = fleet::ledger::Ledger::default_path();
    let routes = led.tail(Some("route"), 100).unwrap();
    assert!(!routes.is_empty(), "every call left a routing row");
    let calls = led.tail(Some("call"), 100).unwrap();
    assert!(!calls.is_empty(), "every call left a call row");

    // ---- lessons: the pairing is remembered ------------------------------
    let lessons = fleet::lessons::briefing(&mem, 50).unwrap();
    assert!(
        lessons.iter().any(|l| l.kind == "pair"),
        "a unison pairing is remembered for next time"
    );

    // ---- resume: the conversation is carried forward ---------------------
    let opts = fleet::engine::RunOptions {
        resume: Some(r.session.clone()),
        harness: Some("alpha".into()),
        ..Default::default()
    };
    let r2 = fleet::engine::run("and why?", &opts).unwrap();
    assert_eq!(r2.session, r.session, "the same conversation continues");
    let turns2 = mem.turns(&r2.session, 200).unwrap();
    assert!(
        turns2.len() > turns.len(),
        "the transcript grew rather than being replaced"
    );
    // The resumed run's brief carries the earlier task, so no harness starts
    // blind: prove it by asking a harness that prints what it received.
    write_reporter(&h.join("harnesses"), "reporter");
    let opts = fleet::engine::RunOptions {
        resume: Some(r.session.clone()),
        harness: Some("reporter".into()),
        ..Default::default()
    };
    let r3 = fleet::engine::run("continue", &opts).unwrap();
    assert!(
        r3.answer.contains("compare the options"),
        "the resumed brief replays the transcript: {}",
        fleet::runner::truncate(&r3.answer, 600)
    );

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_failing_harness_does_not_stop_a_run_and_is_reported() {
    let _g = env_lock();
    let h = home("partial");
    std::env::set_var("FLEET_HOME", &h);
    write_echo(&h.join("harnesses"), "good", "GOOD-ANSWER", 90, false);
    write_failing(&h.join("harnesses"), "broken");

    let opts = fleet::engine::RunOptions {
        unison: true,
        ..Default::default()
    };
    let r = fleet::engine::run("do the thing", &opts).unwrap();

    let ok: Vec<_> = r.calls.iter().filter(|c| c.ok).collect();
    let bad: Vec<_> = r.calls.iter().filter(|c| !c.ok).collect();
    assert_eq!(ok.len(), 1, "the working harness still ran");
    assert_eq!(bad.len(), 1, "the broken one is recorded as failed");
    assert!(
        r.answer.contains("GOOD-ANSWER"),
        "the answer survives a sibling's failure"
    );
    assert!(
        r.answer.contains("FAILED") && r.answer.contains("broken"),
        "the failure is visible in the merged answer, not hidden: {}",
        fleet::runner::truncate(&r.answer, 600)
    );

    // The failure produced a lesson, so the next run knows.
    let mem = fleet::memory::Memory::open(&fleet::paths::db_path()).unwrap();
    let lessons = fleet::lessons::briefing(&mem, 50).unwrap();
    assert!(
        lessons
            .iter()
            .any(|l| l.kind == "harness" && l.text.contains("broken")),
        "a failure becomes a durable lesson"
    );

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_serial_plan_hands_each_stage_what_came_before() {
    let _g = env_lock();
    let h = home("serial");
    std::env::set_var("FLEET_HOME", &h);
    // First stage emits a token; second stage prints its prompt, which must
    // contain that token.
    write_echo(&h.join("harnesses"), "first", "STAGE-ONE-OUTPUT", 90, false);
    write_reporter(&h.join("harnesses"), "second");

    let opts = fleet::engine::RunOptions {
        serial: true,
        ..Default::default()
    };
    let r = fleet::engine::run("build the thing", &opts).unwrap();
    assert_eq!(r.mode, "serial");
    assert!(r.calls.len() >= 2, "both stages ran");
    assert!(
        r.calls[1].output.contains("STAGE-ONE-OUTPUT"),
        "the later stage saw the earlier answer: {}",
        fleet::runner::truncate(&r.calls[1].output, 600)
    );
    assert!(
        r.calls[1].output.contains("Work already done above you"),
        "and it is labelled as prior work"
    );

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_harness_added_or_deleted_between_runs_is_used_or_not_used() {
    let _g = env_lock();
    let h = home("hotplug");
    std::env::set_var("FLEET_HOME", &h);
    write_echo(&h.join("harnesses"), "always", "ALWAYS-HERE", 90, false);

    let opts = fleet::engine::RunOptions {
        harness: Some("always".into()),
        ..Default::default()
    };
    assert!(fleet::engine::run("t", &opts).is_ok());

    // Add a harness by writing a file, and use it on the very next call.
    write_echo(&h.join("harnesses"), "fresh", "FRESH-ANSWER", 90, false);
    let opts = fleet::engine::RunOptions {
        harness: Some("fresh".into()),
        ..Default::default()
    };
    let r = fleet::engine::run("t", &opts).unwrap();
    assert!(r.answer.contains("FRESH-ANSWER"));

    // Delete it, and the next call refuses it — no restart anywhere.
    std::fs::remove_file(h.join("harnesses/fresh.toml")).unwrap();
    let opts = fleet::engine::RunOptions {
        harness: Some("fresh".into()),
        ..Default::default()
    };
    let err = fleet::engine::run("t", &opts).unwrap_err();
    assert!(
        err.to_string().contains("no harness named"),
        "the error names the problem: {err}"
    );

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_run_with_no_harnesses_says_so_clearly() {
    let _g = env_lock();
    let h = home("empty");
    std::env::set_var("FLEET_HOME", &h);
    let err = fleet::engine::run("anything", &fleet::engine::RunOptions::default()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no harnesses") && msg.contains("harness add"),
        "the message tells you what to do: {msg}"
    );
    std::env::remove_var("FLEET_HOME");
}
