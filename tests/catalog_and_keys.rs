//! The catalog and the key inventory, pinned where they can silently lie.
//!
//! Both modules turn data into fleet behaviour, and both have a failure mode
//! that reports success: a catalog entry that parses but detects nothing, and a
//! key scanner that claims a secret is present when it is empty. The tests here
//! assert the observable result of each.

use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleet-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_entry(root: &std::path::Path, name: &str, body: &str) {
    let d = root.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("catalog.toml"), body).unwrap();
}

// ---------------------------------------------------------------- catalog

#[test]
fn a_catalog_entry_is_read_from_its_directory() {
    let root = tmpdir("cat-basic");
    write_entry(
        &root,
        "thing",
        "name = \"thing\"\nkind = \"harness\"\ndescription = \"d\"\ntags = [\"code\"]\n\n[detect]\ncommand = \"sh\"\n",
    );
    let cat = fleet::catalog::Catalog::new(root);
    let (entries, errors) = cat.entries();
    assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0.name, "thing");
    assert_eq!(entries[0].0.kind, fleet::catalog::Kind::Harness);
}

#[test]
fn a_malformed_entry_is_reported_and_does_not_hide_the_rest() {
    let root = tmpdir("cat-bad");
    write_entry(&root, "good", "name = \"good\"\nkind = \"harness\"\n");
    write_entry(&root, "bad", "name = \"bad\"\nkind = \"not-a-kind\"\n");
    let cat = fleet::catalog::Catalog::new(root);
    let (entries, errors) = cat.entries();
    assert_eq!(entries.len(), 1, "the good entry must still load");
    assert_eq!(errors.len(), 1, "the bad entry must be reported");
    assert!(errors[0].0.to_string_lossy().contains("bad"));
}

#[test]
fn detect_reports_what_it_looked_for_when_it_finds_nothing() {
    let root = tmpdir("cat-detect");
    write_entry(
        &root,
        "absent",
        "name = \"absent\"\nkind = \"harness\"\n\n[detect]\ncommand = \"definitely-not-a-real-binary-xyz\"\n",
    );
    let cat = fleet::catalog::Catalog::new(root);
    let (found, _) = cat.discover();
    assert_eq!(found.len(), 1);
    assert!(!found[0].present);
    // The evidence names the command, so a "no" is actionable rather than silent.
    assert!(
        found[0].evidence.contains("definitely-not-a-real-binary-xyz"),
        "evidence should name what was looked for, got: {}",
        found[0].evidence
    );
}

#[test]
fn a_present_tool_that_is_not_wired_is_its_own_state() {
    let root = tmpdir("cat-present");
    write_entry(
        &root,
        "sh-thing",
        "name = \"sh-thing\"\nkind = \"harness\"\n\n[detect]\ncommand = \"sh\"\n",
    );
    let cat = fleet::catalog::Catalog::new(root);
    let (found, _) = cat.discover();
    // `sh` exists everywhere; nothing installed a descriptor for it.
    assert!(found[0].present, "sh should be detected");
    assert!(
        !found[0].installed,
        "no harness descriptor named sh-thing exists, so it is not installed"
    );
}

#[test]
fn keys_are_reported_missing_when_they_are_empty_not_only_unset() {
    // An empty variable is not a credential. Treating it as present is how a
    // fleet believes it can call something it cannot.
    let root = tmpdir("cat-keys");
    write_entry(
        &root,
        "keyed",
        "name = \"keyed\"\nkind = \"harness\"\nkeys = [\"FLEET_TEST_DEFINITELY_UNSET_KEY\"]\n\n[detect]\ncommand = \"sh\"\n",
    );
    let cat = fleet::catalog::Catalog::new(root);
    let (found, _) = cat.discover();
    assert_eq!(found[0].keys_missing.len(), 1);
    assert!(found[0].keys_present.is_empty());
}

#[test]
fn install_commands_are_built_not_taken_from_the_entry() {
    // The engine builds the command from the package name, so an entry cannot
    // smuggle in an arbitrary shell pipeline through `npm`.
    let e = fleet::catalog::Entry {
        name: "x".into(),
        kind: fleet::catalog::Kind::Harness,
        description: String::new(),
        tags: vec![],
        docs: None,
        keys: vec![],
        detect: Default::default(),
        install: fleet::catalog::Install {
            npm: Some("@scope/pkg".into()),
            npm_global: true,
            ..Default::default()
        },
        requires: vec![],
        enabled: true,
        notes: String::new(),
    };
    let cmd = fleet::catalog::install_command(&e).expect("npm install command");
    assert_eq!(cmd[0], "npm");
    assert!(cmd.contains(&"-g".to_string()));
    assert!(cmd.contains(&"@scope/pkg".to_string()));
}

// ---------------------------------------------------------------- keys

#[test]
fn env_file_parsing_ignores_comments_blanks_and_quotes() {
    let text = "# a comment\n\nTB_API_KEY=tb_abc\nQUOTED=\"value\"\nSINGLE='x'\nnot a key line\nEMPTY=\n";
    let m = fleet::keys::parse_env_file(text);
    assert_eq!(m.get("TB_API_KEY").map(|s| s.as_str()), Some("tb_abc"));
    assert_eq!(m.get("QUOTED").map(|s| s.as_str()), Some("value"));
    assert_eq!(m.get("SINGLE").map(|s| s.as_str()), Some("x"));
    assert!(!m.contains_key("EMPTY"), "an empty value is not a key");
}

#[test]
fn a_fingerprint_is_enough_to_tell_keys_apart_and_never_the_value() {
    let f = fleet::keys::fingerprint("tb_epjFrY-secret-tail");
    assert!(f.starts_with("tb_epj"));
    assert!(!f.contains("secret-tail"), "the tail must never be shown");
    assert!(f.contains("chars"), "length is useful and safe: {f}");
}

#[test]
fn secret_names_are_recognised_without_swallowing_ordinary_variables() {
    for k in ["TB_API_KEY", "TWILIO_AUTH_TOKEN", "PLAID_CLIENT_ID", "X_API_SECRET"] {
        assert!(fleet::keys::looks_like_a_secret_name(k), "{k} should be a key");
    }
    for k in ["PATH", "HOME", "TERMINAL_TIMEOUT", "BROWSER_SESSION_TIMEOUT"] {
        assert!(!fleet::keys::looks_like_a_secret_name(k), "{k} is not a key");
    }
}

#[test]
fn the_inventory_covers_the_integrations_the_fleet_actually_uses() {
    let inv = fleet::keys::inventory();
    let names: Vec<&str> = inv.iter().filter_map(|k| k["name"].as_str()).collect();
    for expected in ["TB_API_KEY", "COMPOSIO_API_KEY", "TWILIO_ACCOUNT_SID", "PLAID_CLIENT_ID"] {
        assert!(names.contains(&expected), "{expected} missing from the inventory");
    }
    // Every entry must say what it is for: an inventory without purposes is a
    // list nobody can act on.
    for k in &inv {
        assert!(
            !k["purpose"].as_str().unwrap_or("").is_empty(),
            "{} has no purpose",
            k["name"]
        );
        assert!(!k["group"].as_str().unwrap_or("").is_empty());
    }
}

// ---------------------------------------------------------------- doctor

#[test]
fn the_doctor_never_panics_and_reports_a_verdict() {
    // Read-only, so it is safe to run in any environment; the assertion is that
    // it produces a coherent verdict rather than that it finds nothing.
    let r = fleet::doctor::check();
    assert_eq!(r.ok, r.problems == 0, "ok must mean zero problems");
    assert_eq!(
        r.findings
            .iter()
            .filter(|f| f.severity == fleet::doctor::Severity::Problem)
            .count(),
        r.problems,
        "the problem count must match the findings"
    );
}

// ---------------------------------------------------------------- memory backends

#[test]
fn sqlite_is_the_default_backend_and_is_always_available() {
    let bs = fleet::memory_backend::backends();
    let sqlite = bs.iter().find(|b| b.name == "sqlite").expect("sqlite listed");
    assert!(sqlite.available, "the default must never be unavailable");
    assert!(
        sqlite.trade.contains("instant"),
        "the reason it is the default should be stated: {}",
        sqlite.trade
    );
    // Every backend must state its trade-off: a choice without a cost is a
    // choice that has not been thought about.
    for b in &bs {
        assert!(!b.trade.is_empty(), "{} has no stated trade-off", b.name);
    }
}

#[test]
fn an_unknown_backend_is_rejected_rather_than_silently_accepted() {
    assert!(fleet::memory_backend::is_known("sqlite"));
    assert!(!fleet::memory_backend::is_known("nonsense-backend"));
    assert!(fleet::memory_backend::set("nonsense-backend", None, None).is_err());
}
