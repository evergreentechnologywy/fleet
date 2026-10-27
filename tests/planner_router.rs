//! Planning and routing: the decisions the engine makes before it runs anything.

use fleet::planner::{self, Mode};
use fleet::registry::{Harness, Kind, Route};
use fleet::router::{parse_decision, ChooserConfig, ChooserKind, Config, Defaults, Router};

fn h(name: &str, tags: &[&str], priority: i32, verifier: bool) -> Harness {
    Harness {
        name: name.into(),
        description: format!("{name} harness"),
        kind: Kind::Cli,
        command: Some("echo".into()),
        args: vec![],
        prompt_on_stdin: false,
        model_flag: vec![],
        effort_flag: vec![],
        http: None,
        api_key_env: None,
        api_key: None,
        route: Route::Jev,
        model: None,
        models: vec![],
        effort: None,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        priority,
        cost_hint: 0.0,
        verifier,
        enabled: true,
        timeout_s: None,
        env: Default::default(),
        cwd: None,
        deny_providers: vec![],
        plugins: true,
        source: None,
    }
}

fn static_router() -> Router {
    Router::new(Config {
        chooser: ChooserConfig {
            kind: ChooserKind::Static,
            ..Default::default()
        },
        ..Default::default()
    })
}

#[test]
fn an_explicit_harness_bypasses_the_planner_entirely() {
    let cands = vec![h("a", &["code"], 50, false), h("b", &["chat"], 10, false)];
    let p = planner::plan("anything at all", &cands, Some("b"), false, false, None);
    assert_eq!(p.mode, Mode::Single);
    assert_eq!(p.steps.len(), 1);
    assert_eq!(
        p.steps[0].harness, "b",
        "an instruction is never second-guessed"
    );
    assert_eq!(p.source, "flag");
}

#[test]
fn the_planner_picks_the_harness_whose_tags_match() {
    let cands = vec![
        h("coder", &["code", "repo"], 50, false),
        h("chat", &["chat"], 10, false),
    ];
    let p = planner::plan(
        "refactor this repo module",
        &cands,
        None,
        false,
        false,
        None,
    );
    assert_eq!(p.steps[0].harness, "coder");
    assert_eq!(p.mode, Mode::Single, "a clear winner runs alone");
}

#[test]
fn unison_is_forced_by_the_flag_and_by_an_ambiguous_task() {
    let cands = vec![
        h("one", &["code"], 50, false),
        h("two", &["code"], 50, false),
    ];
    // Forced.
    let p = planner::plan("do it", &cands, None, true, false, None);
    assert_eq!(p.mode, Mode::Unison);
    assert_eq!(p.steps.len(), 2, "both answer");

    // Natural: equal scores mean neither is clearly the right one, so the
    // engine uses them all — that is the premise of the whole thing.
    let p2 = planner::plan("do it", &cands, None, false, false, None);
    assert_eq!(p2.mode, Mode::Unison);
}

#[test]
fn serial_appends_the_declared_verifier() {
    let cands = vec![
        h("worker", &["code"], 50, false),
        h("checker", &[], 0, true),
    ];
    let p = planner::plan("write and verify", &cands, None, false, true, None);
    assert_eq!(p.mode, Mode::Serial);
    assert_eq!(p.steps.last().unwrap().harness, "checker");
    assert_eq!(p.steps.last().unwrap().role, "verify");
}

#[test]
fn a_plan_naming_an_absent_harness_drops_that_step() {
    // A plan produced on another machine must not fail the run here.
    let cands = vec![h("present", &["code"], 50, false)];
    let raw = r#"{"mode":"unison","steps":[{"harness":"present"},{"harness":"ghost"}]}"#;
    let p = planner::parse_plan(raw, &cands).unwrap();
    assert_eq!(p.steps.len(), 1);
    assert_eq!(p.steps[0].harness, "present");
}

#[test]
fn a_plan_survives_prose_around_the_json() {
    // A chooser is often a model; models wrap their answer.
    let cands = vec![h("a", &[], 0, false)];
    let raw = "Sure! Here is the plan:\n```json\n{\"mode\":\"single\",\"steps\":[{\"harness\":\"a\",\"role\":\"work\"}]}\n```\nDone.";
    let p = planner::parse_plan(raw, &cands).unwrap();
    assert_eq!(p.mode, Mode::Single);
    assert_eq!(p.steps[0].harness, "a");
}

#[test]
fn an_unusable_plan_returns_none_rather_than_a_wrong_one() {
    let cands = vec![h("a", &[], 0, false)];
    assert!(planner::parse_plan("no json here", &cands).is_none());
    assert!(planner::parse_plan("{}", &cands).is_none());
    assert!(planner::parse_plan(r#"{"steps":[{"harness":"nobody"}]}"#, &cands).is_none());
}

#[test]
fn a_decision_is_read_from_json_or_from_key_value_lines() {
    let d = parse_decision(
        r#"{"model":"m-1","effort":"high","lane":"liquid","serving_provider":"liquid"}"#,
    )
    .unwrap();
    assert_eq!(d.model, "m-1");
    assert_eq!(d.effort, "high");
    assert_eq!(d.lane, "liquid");
    assert_eq!(d.provider, "liquid");

    // The shape the fleet's own chooser prints.
    let d =
        parse_decision("MODEL=deepseek-v4.1-flash LANE=text EFFORT=minimal REASONING=on").unwrap();
    assert_eq!(d.model, "deepseek-v4.1-flash");
    assert_eq!(d.effort, "minimal");
    assert_eq!(d.lane, "text");

    // JSON embedded in other output, which is what a chatty script emits.
    let d = parse_decision("choosing…\n{\"model\":\"m-2\",\"effort\":\"low\"}\ndone").unwrap();
    assert_eq!(d.model, "m-2");

    assert!(parse_decision("nothing useful").is_err());
}

#[test]
fn a_chooser_that_fails_falls_back_to_the_harness_default() {
    // The property that keeps a broken chooser from stopping a run.
    let cfg = Config {
        chooser: ChooserConfig {
            kind: ChooserKind::Cli,
            command: Some("/nonexistent/chooser".into()),
            ..Default::default()
        },
        defaults: Defaults {
            model: Some("fallback-model".into()),
            effort: Some("medium".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let router = Router::new(cfg);
    let d = router.decide(&h("x", &[], 0, false), "do something");
    assert_eq!(d.model, "fallback-model");
    assert!(
        d.reason.contains("chooser failed") || d.source == "fallback",
        "the fallback is visible, not silent: {d:?}"
    );
}

#[test]
fn a_pinned_harness_spends_no_chooser_call() {
    let router = static_router();
    let mut pinned = h("p", &[], 0, false);
    pinned.route = Route::Fixed;
    pinned.model = Some("only-this".into());
    let d = router.decide(&pinned, "anything");
    assert_eq!(d.model, "only-this");
    assert_eq!(d.source, "harness");
}

#[test]
fn a_self_routing_harness_is_not_given_a_model() {
    let router = static_router();
    let mut sr = h("sr", &[], 0, false);
    sr.route = Route::SelfRouting;
    let d = router.decide(&sr, "anything");
    assert_eq!(d.model, "", "it picks its own, through the chooser's door");
    assert_eq!(d.lane, "self");
}

#[test]
fn a_harness_never_receives_a_model_outside_its_declared_ceiling() {
    // A chooser answers with a model this harness said it cannot serve.
    let router = Router::new(Config {
        chooser: ChooserConfig {
            kind: ChooserKind::Cli,
            command: Some("sh".into()),
            args: vec![
                "-c".into(),
                "echo '{\"model\":\"forbidden-model\",\"effort\":\"high\"}'".into(),
            ],
            ..Default::default()
        },
        ..Default::default()
    });
    let mut limited = h("limited", &[], 0, false);
    limited.models = vec!["allowed-a".into(), "allowed-b".into()];

    let d = router.decide(&limited, "do it");
    assert_eq!(d.model, "allowed-a", "clamped to the ceiling");
    assert!(
        d.reason.contains("not in harness ceiling"),
        "and it says why: {}",
        d.reason
    );
}

#[test]
fn the_chooser_answer_is_reused_inside_its_ttl() {
    // A unison run must pay for one chooser call, not one per harness.
    let counter = std::env::temp_dir().join(format!("fleet-ttl-{}", std::process::id()));
    let _ = std::fs::remove_file(&counter);
    let script = format!(
        "echo x >> {}; echo '{{\"model\":\"m\",\"effort\":\"low\"}}'",
        counter.display()
    );
    let router = Router::new(Config {
        chooser: ChooserConfig {
            kind: ChooserKind::Cli,
            command: Some("sh".into()),
            args: vec!["-c".into(), script],
            ttl_s: Some(300),
            ..Default::default()
        },
        ..Default::default()
    });
    let hh = h("x", &[], 0, false);
    let a = router.decide(&hh, "same task");
    let b = router.decide(&hh, "same task");
    assert_eq!(a.model, "m");
    assert_eq!(b.model, "m");
    assert_eq!(b.source, "cache", "the second decision was reused");

    let calls = std::fs::read_to_string(&counter).unwrap().lines().count();
    assert_eq!(calls, 1, "one chooser process for two identical decisions");

    // A different task is a different decision.
    let c = router.decide(&hh, "different task");
    assert_eq!(c.source, "chooser");
    let calls = std::fs::read_to_string(&counter).unwrap().lines().count();
    assert_eq!(calls, 2);
}
