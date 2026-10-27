//! The property the whole design rests on: a harness is a file, and adding or
//! removing one takes effect on the next call with no restart.

use fleet::registry::{Kind, Registry, Route};
use std::path::PathBuf;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleet-test-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn descriptor(name: &str, extra: &str) -> String {
    format!(
        r#"name = "{name}"
description = "test harness"
kind = "cli"
command = "echo"
args = ["{{prompt}}"]
route = "native"
tags = ["test"]
priority = 5
enabled = true
timeout_s = 30
{extra}
"#
    )
}

#[test]
fn a_harness_appears_the_moment_its_file_lands() {
    let dir = tmpdir("add");
    let reg = Registry::new(dir.clone());

    assert!(reg.all().0.is_empty(), "nothing installed yet");

    // Add: write the file. Nothing else.
    std::fs::write(dir.join("alpha.toml"), descriptor("alpha", "")).unwrap();
    let names: Vec<String> = reg.all().0.into_iter().map(|h| h.name).collect();
    assert_eq!(names, vec!["alpha"], "the file IS the registration");

    // Add a second one.
    std::fs::write(dir.join("beta.toml"), descriptor("beta", "")).unwrap();
    let names: Vec<String> = reg.all().0.into_iter().map(|h| h.name).collect();
    assert_eq!(names, vec!["alpha", "beta"], "sorted, both live");

    // Delete: remove the file.
    std::fs::remove_file(dir.join("alpha.toml")).unwrap();
    let names: Vec<String> = reg.all().0.into_iter().map(|h| h.name).collect();
    assert_eq!(names, vec!["beta"], "gone on the next read");

    // Delete the last one: the engine is simply empty, not broken.
    std::fs::remove_file(dir.join("beta.toml")).unwrap();
    assert!(reg.all().0.is_empty());
}

#[test]
fn an_edit_is_seen_without_a_restart() {
    let dir = tmpdir("edit");
    let reg = Registry::new(dir.clone());
    let path = dir.join("gamma.toml");
    std::fs::write(&path, descriptor("gamma", "")).unwrap();

    let before = reg.get("gamma").unwrap();
    assert_eq!(before.priority, 5);
    assert_eq!(before.tags, vec!["test"]);

    // Rewrite the file with different values. The fingerprint (mtime+len)
    // changes, so the cached parse is discarded.
    let changed = descriptor("gamma", "").replace("priority = 5", "priority = 42");
    std::fs::write(&path, changed).unwrap();

    let after = reg.get("gamma").unwrap();
    assert_eq!(after.priority, 42, "the new definition is in use");
}

#[test]
fn a_broken_file_is_reported_and_does_not_hide_the_good_ones() {
    let dir = tmpdir("broken");
    let reg = Registry::new(dir.clone());
    std::fs::write(dir.join("good.toml"), descriptor("good", "")).unwrap();
    std::fs::write(dir.join("bad.toml"), "this is not toml at all = = =").unwrap();

    let (all, errors) = reg.all();
    assert_eq!(all.len(), 1, "the valid harness still loads");
    assert_eq!(errors.len(), 1, "the broken one is reported");
    assert!(
        errors[0].1.contains("bad.toml"),
        "the error names the file: {}",
        errors[0].1
    );
}

#[test]
fn a_key_written_after_the_http_table_is_still_found() {
    // TOML has no "return to top level": a key after an `[http]` header belongs
    // to that table. That is the natural mistake, and dropping the key silently
    // reads as "this provider is logged out" rather than as a typo, so the key
    // is resolved from either place.
    let dir = tmpdir("keyscope");
    let reg = Registry::new(dir.clone());
    std::fs::write(
        dir.join("scoped.toml"),
        r#"name = "scoped"
kind = "openai-compat"
route = "fixed"
model = "m"
enabled = true

[http]
base_url = "http://127.0.0.1:1"
api_key_env = "SCOPED_KEY"
"#,
    )
    .unwrap();
    let h = reg.get("scoped").unwrap();
    assert_eq!(h.key_env(), Some("SCOPED_KEY"), "found in the [http] table");

    // And the top-level form wins when both are present.
    std::fs::write(
        dir.join("both.toml"),
        r#"name = "both"
kind = "openai-compat"
route = "fixed"
model = "m"
api_key_env = "TOP_LEVEL_KEY"
enabled = true

[http]
base_url = "http://127.0.0.1:1"
api_key_env = "NESTED_KEY"
"#,
    )
    .unwrap();
    assert_eq!(reg.get("both").unwrap().key_env(), Some("TOP_LEVEL_KEY"));
}

#[test]
fn the_file_stem_must_match_the_name() {
    // The file IS the harness, so a mismatch is a real error and is reported
    // against the path instead of being silently resolved either way.
    let dir = tmpdir("stem");
    let reg = Registry::new(dir.clone());
    std::fs::write(dir.join("wrong.toml"), descriptor("right", "")).unwrap();
    let (all, errors) = reg.all();
    assert!(all.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].1.contains("must match the file stem"),
        "clear message: {}",
        errors[0].1
    );
}

#[test]
fn install_is_atomic_and_validates_against_the_destination_name() {
    let dir = tmpdir("install");
    let reg = Registry::new(dir.clone());
    let src = dir.join("incoming.toml");
    std::fs::write(&src, descriptor("installed", "")).unwrap();

    let h = reg.install(&src).unwrap();
    assert_eq!(h.name, "installed");
    assert!(dir.join("installed.toml").exists(), "landed under its name");
    assert!(
        !dir.join(".installed.tmp").exists(),
        "no temporary file left behind"
    );
    assert_eq!(reg.all().0.len(), 1);

    // A descriptor whose name does not match the destination is refused.
    let bad = dir.join("bad.toml");
    std::fs::write(&bad, "name = \"\"\nkind = \"cli\"\ncommand = \"echo\"\n").unwrap();
    assert!(reg.install(&bad).is_err());

    // Removing a name that matches nothing is an error, not a silent success.
    assert!(reg.remove("nonexistent").is_err());
    assert!(reg.remove("installed").is_ok());
    assert!(reg.all().0.is_empty());
}

#[test]
fn placeholders_and_flags_are_substituted_correctly() {
    let dir = tmpdir("argv");
    let reg = Registry::new(dir.clone());
    std::fs::write(
        dir.join("flags.toml"),
        r#"name = "flags"
kind = "cli"
command = "echo"
args = ["-p", "{prompt}", "--cwd", "{cwd}"]
model_flag = ["--model", "{model}"]
effort_flag = ["--thinking", "{effort}"]
route = "native"
enabled = true
"#,
    )
    .unwrap();
    let h = reg.get("flags").unwrap();
    let argv = h.argv(
        "do the thing",
        Some("m-1"),
        Some("high"),
        std::path::Path::new("/tmp/x"),
    );
    assert_eq!(
        argv,
        vec![
            "-p",
            "do the thing",
            "--cwd",
            "/tmp/x",
            "--model",
            "m-1",
            "--thinking",
            "high"
        ]
    );

    // A one-token flag form carries the value inside the token.
    std::fs::write(
        dir.join("inline.toml"),
        r#"name = "inline"
kind = "cli"
command = "echo"
args = ["{prompt}"]
model_flag = ["--model={model}"]
route = "native"
enabled = true
"#,
    )
    .unwrap();
    let h = reg.get("inline").unwrap();
    let argv = h.argv("t", Some("m-2"), None, std::path::Path::new("/tmp"));
    assert_eq!(argv, vec!["t", "--model=m-2"]);
}

#[test]
fn route_and_kind_defaults_are_sane() {
    let dir = tmpdir("defaults");
    let reg = Registry::new(dir.clone());
    // The minimum a descriptor can be and still work.
    std::fs::write(
        dir.join("minimal.toml"),
        "name = \"minimal\"\nkind = \"cli\"\ncommand = \"echo\"\n",
    )
    .unwrap();
    let h = reg.get("minimal").unwrap();
    assert_eq!(h.route, Route::Jev, "a chooser is the default");
    assert_eq!(h.kind, Kind::Cli);
    assert!(h.enabled, "a harness you wrote is meant to run");
    assert!(h.models.is_empty(), "no ceiling unless declared");
    assert_eq!(h.timeout(), 1800);
}

#[test]
fn an_alias_is_not_a_model_field_and_must_not_clamp() {
    // The bug this pins, measured 2026-10-27: `models = ["jev/auto", "auto"]`
    // looked like a ceiling but names only ROUTER ALIASES. The router treated it
    // as a ceiling anyway, so every real chooser decision was rejected and
    // replaced by the alias — 492 times in one ledger — while the reason string
    // was rewritten to read like a normal route. The run looked routed and a
    // fixed model was used.
    use fleet::registry::is_model_field;

    // Aliases and wildcards constrain nothing.
    for alias in ["jev/auto", "auto", "*", "", "  "] {
        assert!(
            !is_model_field(alias),
            "`{alias}` must not count as a model field"
        );
    }
    // Real ids do, including provider-qualified and versioned spellings.
    for real in [
        "deepseek-v4.1-flash",
        "gpt-6-luna",
        "liquid/deepseek-v4.1-flash",
        "step-5-preview",
        "qwen3.8-flash",
    ] {
        assert!(is_model_field(real), "`{real}` must count as a model field");
    }
}

#[test]
fn a_ceiling_of_aliases_accepts_anything_but_a_ceiling_of_models_does_not() {
    // The two halves must stay distinguishable: `accepts` answers "may this
    // harness receive X", and `is_model_field` answers "does this list constrain
    // anything". Conflating them is what produced the silent clamp.
    use fleet::registry::{is_model_field, Registry};

    let dir = std::env::temp_dir().join(format!("fleet-alias-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("aliased.toml");
    std::fs::write(
        &p,
        "name = \"aliased\"\nkind = \"cli\"\ncommand = \"true\"\nmodels = [\"jev/auto\", \"auto\"]\n",
    )
    .unwrap();
    let reg = Registry::new(dir.clone());
    let (hs, errs) = reg.all();
    assert!(errs.is_empty(), "descriptor must load: {errs:?}");
    let h = hs.iter().find(|h| h.name == "aliased").expect("loaded");
    assert!(
        !h.models.iter().any(|m| is_model_field(m)),
        "an alias-only list declares no model fields"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
