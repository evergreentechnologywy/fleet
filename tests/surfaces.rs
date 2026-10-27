//! Plugins, providers, the self-modification gate, and the mesh helpers.
//!
//! As in `engine_run.rs`, `FLEET_HOME` is process-global, so the tests that set
//! it hold one lock for their whole body.

use fleet::plugins::{Effects, Plugins};
use fleet::providers::{Provider, Providers};
use fleet::selfmod::{glob_match, Ceiling};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

static ENV_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

fn env_lock() -> MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn home(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleet-surface-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_plugin(root: &Path, name: &str, spec: &str, script: &str) {
    let dir = root.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.toml"), spec).unwrap();
    let p = dir.join("plugin.sh");
    std::fs::write(&p, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ---------------------------------------------------------------- plugins

#[test]
fn a_plugin_is_picked_up_from_disk_with_no_registration() {
    let _g = env_lock();
    let h = home("plugin-add");
    std::env::set_var("FLEET_HOME", &h);
    let plugins = Plugins::default_dir();

    assert!(plugins.all().is_empty());

    write_plugin(
        h.as_path(),
        "stamp",
        "name = \"stamp\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\necho '{\"prompt_prefix\":\"[stamped] \"}'\n",
    );
    let all = plugins.all();
    assert_eq!(all.len(), 1, "the directory IS the registration");
    assert_eq!(all[0].name, "stamp");

    // Removing the directory removes the plugin.
    std::fs::remove_dir_all(h.join("plugins/stamp")).unwrap();
    assert!(plugins.all().is_empty());

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_plugin_applies_to_every_harness_by_default_and_can_narrow_itself() {
    let _g = env_lock();
    let h = home("plugin-scope");
    std::env::set_var("FLEET_HOME", &h);

    // No `harnesses` key: applies everywhere. This is the property that makes
    // one plugin worth writing — including for harnesses added later.
    write_plugin(
        h.as_path(),
        "everywhere",
        "name = \"everywhere\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\necho '{\"prompt_prefix\":\"[all] \"}'\n",
    );
    // Scoped to one harness.
    write_plugin(
        h.as_path(),
        "only-omp",
        "name = \"only-omp\"\nevents = [\"pre_call\"]\nharnesses = [\"omp\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\necho '{\"prompt_suffix\":\" [omp-only]\"}'\n",
    );

    let plugins = Plugins::default_dir();
    let for_omp = plugins.matching("pre_call", "omp", "");
    assert_eq!(for_omp.len(), 2, "both apply to omp");

    let for_codex = plugins.matching("pre_call", "codex", "");
    assert_eq!(for_codex.len(), 1, "only the unscoped one applies to codex");
    assert_eq!(for_codex[0].name, "everywhere");

    // A harness added tomorrow is covered with no configuration at all.
    let for_brand_new = plugins.matching("pre_call", "harness-invented-later", "");
    assert_eq!(for_brand_new.len(), 1);
    assert_eq!(for_brand_new[0].name, "everywhere");

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_plugin_can_change_the_call_skip_it_and_record_memory() {
    let _g = env_lock();
    let h = home("plugin-effects");
    std::env::set_var("FLEET_HOME", &h);

    write_plugin(
        h.as_path(),
        "shaper",
        "name = \"shaper\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\necho '{\"env\":{\"EXTRA\":\"1\"},\"args_append\":[\"--flag\"],\"prompt_prefix\":\"PRE \",\"prompt_suffix\":\" POST\",\"note\":\"shaped\"}'\n",
    );
    let plugins = Plugins::default_dir();
    let fx = plugins.fire("pre_call", "anything", "", &serde_json::json!({"task":"t"}));
    assert_eq!(fx.env.get("EXTRA").map(String::as_str), Some("1"));
    assert_eq!(fx.args_append, vec!["--flag"]);
    assert_eq!(fx.prompt_prefix, "PRE ");
    assert_eq!(fx.prompt_suffix, " POST");
    assert!(
        fx.note.contains("shaper"),
        "the plugin is named in the note"
    );

    write_plugin(
        h.as_path(),
        "veto",
        "name = \"veto\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\npriority = 10\n",
        "#!/bin/sh\necho '{\"skip\":true,\"skip_reason\":\"blocked by policy\"}'\n",
    );
    let fx = plugins.fire("pre_call", "anything", "", &serde_json::json!({}));
    assert!(fx.skip, "a veto is honoured");
    assert_eq!(fx.skip_reason, "blocked by policy");

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_broken_plugin_is_bypassed_rather_than_fatal() {
    let _g = env_lock();
    let h = home("plugin-broken");
    std::env::set_var("FLEET_HOME", &h);

    // Prints nonsense.
    write_plugin(
        h.as_path(),
        "nonsense",
        "name = \"nonsense\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\necho 'this is not json'\n",
    );
    // Exits non-zero.
    write_plugin(
        h.as_path(),
        "explodes",
        "name = \"explodes\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = true\n",
        "#!/bin/sh\nexit 7\n",
    );
    // Command does not exist.
    write_plugin(
        h.as_path(),
        "missing",
        "name = \"missing\"\nevents = [\"pre_call\"]\ncommand = \"/nonexistent/plugin-binary\"\nargs = []\nenabled = true\n",
        "",
    );

    let plugins = Plugins::default_dir();
    // None of them may panic or abort the call; the result is simply no change.
    let fx: Effects = plugins.fire("pre_call", "x", "", &serde_json::json!({}));
    assert!(!fx.skip, "a broken plugin cannot veto a call");
    assert!(fx.env.is_empty());
    assert!(fx.prompt_prefix.is_empty());

    // A plugin that is disabled is not even loaded.
    write_plugin(
        h.as_path(),
        "off",
        "name = \"off\"\nevents = [\"pre_call\"]\ncommand = \"sh\"\nargs = [\"plugin.sh\"]\nenabled = false\n",
        "#!/bin/sh\necho '{\"skip\":true}'\n",
    );
    assert_eq!(plugins.all().len(), 3, "the disabled one is not loaded");

    std::env::remove_var("FLEET_HOME");
}

// ---------------------------------------------------------------- providers

#[test]
fn providers_are_shared_by_every_harness_with_explicit_blocks() {
    let _g = env_lock();
    let h = home("providers");
    std::env::set_var("FLEET_HOME", &h);
    let ps = Providers::default_file();

    ps.put(&Provider {
        name: "shared".into(),
        base_url: "https://api.example.ai/v1".into(),
        key_env: "SHARED_KEY".into(),
        extra: Default::default(),
        models: vec!["*".into()],
        block: vec![],
        enabled: true,
        notes: String::new(),
            credential: None,
    })
    .unwrap();
    ps.put(&Provider {
        name: "restricted".into(),
        base_url: "https://api.other.ai/v1".into(),
        key_env: "OTHER_KEY".into(),
        extra: Default::default(),
        models: vec!["*".into()],
        block: vec!["omp".into()],
        enabled: true,
        notes: String::new(),
            credential: None,
    })
    .unwrap();

    // A harness that exists today and one invented later both see the shared
    // provider: sharing is the default.
    for harness in ["omp", "codex", "brand-new-harness"] {
        let names: Vec<String> = ps
            .for_harness(harness, &[])
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert!(
            names.contains(&"shared".to_string()),
            "{harness} sees shared"
        );
    }

    // The block is honoured in the provider -> harness direction.
    let omp: Vec<String> = ps
        .for_harness("omp", &[])
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(
        !omp.contains(&"restricted".to_string()),
        "omp is blocked from it"
    );
    let codex: Vec<String> = ps
        .for_harness("codex", &[])
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(codex.contains(&"restricted".to_string()), "codex is not");

    // And in the harness -> provider direction, which is a separate authority.
    let denied: Vec<String> = ps
        .for_harness("codex", &["shared".to_string()])
        .into_iter()
        .map(|p| p.name)
        .collect();
    assert!(
        !denied.contains(&"shared".to_string()),
        "a harness can refuse one"
    );
    assert!(denied.contains(&"restricted".to_string()));

    // A provider with no key in the environment is declared but unavailable.
    let p = ps.get("shared").unwrap();
    assert!(!p.available(), "no SHARED_KEY in this environment");
    assert!(p.key().is_none());

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn provider_env_exports_the_names_clis_actually_read() {
    let _g = env_lock();
    let h = home("provider-env");
    std::env::set_var("FLEET_HOME", &h);
    std::env::set_var("TESTPROV_KEY", "secret-value");

    let ps = Providers::default_file();
    ps.put(&Provider {
        name: "testprov".into(),
        base_url: "https://api.test.ai/v1".into(),
        key_env: "TESTPROV_KEY".into(),
        extra: Default::default(),
        models: vec!["*".into()],
        block: vec![],
        enabled: true,
        notes: String::new(),
            credential: None,
    })
    .unwrap();

    let env = ps.env_for("anyharness", &[]);
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    // The three conventions the common CLIs use.
    assert_eq!(get("OPENAI_BASE_URL"), "https://api.test.ai/v1");
    assert_eq!(get("OPENAI_API_KEY"), "secret-value");
    assert_eq!(get("ANTHROPIC_BASE_URL"), "https://api.test.ai/v1");
    // And a namespaced pair for a harness that wants to be explicit.
    assert_eq!(get("FLEET_PROVIDER_TESTPROV_URL"), "https://api.test.ai/v1");
    assert_eq!(get("FLEET_PROVIDER_TESTPROV_KEY"), "secret-value");
    // An inventory the harness can read to discover what it has.
    assert!(get("FLEET_PROVIDERS").contains("testprov"));

    std::env::remove_var("TESTPROV_KEY");
    std::env::remove_var("FLEET_HOME");
}

// ---------------------------------------------------------------- selfmod

#[test]
fn the_ceiling_is_checked_before_any_verifier_runs() {
    // Fail closed: a path outside the frame is refused, and the refusal happens
    // before verification, not after.
    let c = Ceiling {
        enabled: true,
        paths: vec!["harnesses/*.toml".into(), "mods/**".into()],
        verifiers: vec!["nonexistent-verifier".into()],
    };
    assert!(c.permits("harnesses/omp.toml"));
    assert!(c.permits("mods/deep/nested/thing.toml"));
    assert!(!c.permits("fleet.db"));
    assert!(
        !c.permits("harnesses/nested/omp.toml"),
        "* does not cross a separator"
    );
    assert!(!c.permits("/etc/passwd"));

    // Disabled means nothing is modifiable at all.
    let off = Ceiling {
        enabled: false,
        paths: vec!["harnesses/*.toml".into()],
        verifiers: vec![],
    };
    assert!(!off.enabled);
}

#[test]
fn glob_matching_is_predictable() {
    assert!(glob_match("*", "anything"));
    assert!(glob_match("a/*", "a/b"));
    assert!(!glob_match("a/*", "a/b/c"));
    assert!(glob_match("a/**", "a/b/c/d"));
    assert!(glob_match("**", ""));
    assert!(glob_match("harnesses/*.toml", "harnesses/x.toml"));
    assert!(!glob_match("harnesses/*.toml", "harnesses/x.txt"));
    assert!(!glob_match("harnesses/*.toml", "other/x.toml"));
}

#[test]
fn a_missing_verifier_is_a_failed_verification_not_a_pass() {
    let _g = env_lock();
    let h = home("selfmod-fail");
    std::env::set_var("FLEET_HOME", &h);
    std::fs::create_dir_all(h.join("harnesses")).unwrap();
    std::fs::write(
        h.join("harnesses/x.toml"),
        "name = \"x\"\nkind = \"cli\"\ncommand = \"echo\"\n",
    )
    .unwrap();

    let ceiling = Ceiling {
        enabled: true,
        paths: vec!["harnesses/*.toml".into()],
        verifiers: vec!["does-not-exist".into()],
    };
    let ledger = fleet::ledger::Ledger::default_path();
    let grant = fleet::selfmod::Grant {
        target: "harnesses/x.toml".into(),
        reason: "test".into(),
        content: None,
        digest: String::new(),
    };
    let err = fleet::selfmod::propose(&ceiling, &grant, &ledger, "test").unwrap_err();
    assert!(
        err.to_string().contains("verifier") || err.to_string().contains("verification failed"),
        "a missing verifier must not admit: {err}"
    );

    // The failure is a durable ledger row, not a silent drop.
    let rows = ledger.tail(Some("verify"), 10).unwrap();
    assert!(rows.iter().any(|r| !r.ok), "a failed verdict is recorded");
    // And no mark was written.
    assert!(ledger.tail(Some("mark"), 10).unwrap().is_empty());

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn self_modification_is_off_until_the_ceiling_says_otherwise() {
    let _g = env_lock();
    let h = home("selfmod-off");
    std::env::set_var("FLEET_HOME", &h);
    std::fs::create_dir_all(h.join("harnesses")).unwrap();
    std::fs::write(
        h.join("harnesses/x.toml"),
        "name = \"x\"\nkind = \"cli\"\ncommand = \"echo\"\n",
    )
    .unwrap();

    // The default ceiling, written by onboarding, has enabled = false.
    fleet::selfmod::write_starter_policy().unwrap();
    let ceiling = Ceiling::load();
    assert!(!ceiling.enabled, "off by default");

    let ledger = fleet::ledger::Ledger::default_path();
    let grant = fleet::selfmod::Grant {
        target: "harnesses/x.toml".into(),
        reason: "test".into(),
        content: None,
        digest: String::new(),
    };
    assert!(fleet::selfmod::propose(&ceiling, &grant, &ledger, "t").is_err());

    std::env::remove_var("FLEET_HOME");
}

// ---------------------------------------------------------------- mesh

#[test]
fn base64_round_trips_so_messages_survive_a_shell() {
    // Messages are base64'd before crossing ssh so quoting can never break one.
    for s in [
        "plain",
        "with spaces and 'quotes' and \"doubles\"",
        "multi\nline\ntext",
        "unicode: ✓ é 中文 🎉",
        "",
    ] {
        let enc = fleet::mesh::base64_encode(s.as_bytes());
        assert!(
            enc.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
            "output is safe for a shell: {enc}"
        );
        // Decode with the system base64 to prove it is real base64.
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf %s '{enc}' | base64 -d"))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            s,
            "round trip failed for {s:?}"
        );
    }
}

#[test]
fn deploy_targets_parse_including_the_bare_host_shorthand() {
    use fleet::deploy::Target;
    assert_eq!(Target::parse("ssh://root@box").unwrap().0, Target::Ssh);
    assert_eq!(
        Target::parse("root@box").unwrap().0,
        Target::Ssh,
        "bare host means ssh"
    );
    let (t, host, port) = Target::parse("ssh://root@box:2222").unwrap();
    assert_eq!(t, Target::Ssh);
    assert_eq!(host, "root@box");
    assert_eq!(port, 2222);
    assert_eq!(
        Target::parse("tailscale://desk").unwrap().0,
        Target::Tailscale
    );
    assert_eq!(Target::parse("reason://peer-1").unwrap().0, Target::Reason);
    assert_eq!(Target::parse("docker://default").unwrap().0, Target::Docker);
    assert_eq!(Target::parse("file:./out").unwrap().0, Target::File);
    assert!(Target::parse("").is_err());
}

#[test]
fn the_one_line_deploy_names_the_target_and_the_port() {
    use fleet::deploy::{one_liner, DeploySpec};
    let spec = DeploySpec {
        target: "ssh://root@box".into(),
        method: None,
        prefix: None,
        port: Some(9000),
        run: true,
        release: false,
    };
    let line = one_liner(&spec);
    assert!(line.contains("--port 9000"), "{line}");
    assert!(line.contains("root@box"), "{line}");

    // An empty target is the real error case; a bare host is valid shorthand
    // and must NOT be rejected.
    let spec = DeploySpec {
        target: String::new(),
        method: None,
        prefix: None,
        port: None,
        run: false,
        release: false,
    };
    assert!(one_liner(&spec).contains("invalid target"));
}

#[test]
fn append_commands_are_built_for_the_peers_own_shell() {
    // A fleet is heterogeneous: a Windows desk answers ssh with cmd.exe, where
    // none of the POSIX toolchain exists. The command is built for the shell we
    // detected, never guessed at run time — `command -v` does not exist in
    // cmd.exe, so a detecting one-liner is exactly what breaks.
    use fleet::mesh::{append_command, Shell};

    let posix = append_command(Shell::Posix, "YWJj");
    assert!(
        posix.contains("base64 -d"),
        "POSIX decodes with base64: {posix}"
    );
    assert!(posix.contains("$HOME"), "POSIX writes under $HOME: {posix}");
    assert!(
        !posix.contains("certutil") && !posix.contains("%USERPROFILE%"),
        "no Windows toolchain leaks in: {posix}"
    );
    assert!(
        !posix.contains("command -v"),
        "no run-time toolchain detection: {posix}"
    );

    let win = append_command(Shell::Windows, "YWJj");
    assert!(
        win.contains("certutil"),
        "Windows decodes with certutil: {win}"
    );
    assert!(
        win.contains("%USERPROFILE%"),
        "Windows writes under the profile: {win}"
    );
    assert!(
        !win.contains("$HOME") && !win.contains("base64 -d"),
        "no POSIX toolchain leaks in: {win}"
    );
    assert!(
        !win.contains("command -v"),
        "cmd.exe has no `command -v`, so it must not appear: {win}"
    );
}
