//! Credentials (including OAuth) and plugin-provided deploy targets.
//!
//! `FLEET_HOME` is process-global, so tests that set it hold one lock.

use fleet::credential::{Credentials, Source};
use fleet::deploy_targets::{Targets, TargetSpec};
use fleet::providers::Provider;
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
    let d = std::env::temp_dir().join(format!("fleet-oauth-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_target(root: &Path, scheme: &str, script_body: &str) {
    let dir = root.join("deploy-targets").join(scheme);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("target.toml"),
        format!(
            "scheme = \"{scheme}\"\ndescription = \"test target\"\ncommand = \"sh\"\nargs = [\"t.sh\"]\nenabled = true\n"
        ),
    )
    .unwrap();
    let p = dir.join("t.sh");
    std::fs::write(&p, script_body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn req(scheme: &str, host: &str) -> fleet::deploy_targets::Request {
    fleet::deploy_targets::Request {
        scheme: scheme.into(),
        host: host.into(),
        port: 8787,
        prefix: "/usr/local".into(),
        binary: "/tmp/fleet".into(),
        bundle: "/tmp/bundle.tar.gz".into(),
        env: Default::default(),
    }
}

// ------------------------------------------------------------ credentials

#[test]
fn a_provider_without_a_credential_block_still_uses_its_env_var() {
    // Every descriptor written before this feature existed must keep working.
    let p = Provider {
        name: "legacy".into(),
        base_url: "https://api.example.ai/v1".into(),
        key_env: "LEGACY_KEY".into(),
        extra: Default::default(),
        models: vec!["*".into()],
        block: vec![],
        credential: None,
        enabled: true,
        notes: String::new(),
    };
    assert!(matches!(p.source(), Source::Env { .. }));
    assert_eq!(p.source().label(), "env:LEGACY_KEY");

    std::env::set_var("LEGACY_KEY", "k1");
    assert_eq!(p.key().as_deref(), Some("k1"));
}

#[test]
fn an_oauth_provider_resolves_through_its_broker() {
    let _g = env_lock();
    let h = home("oauth");
    std::env::set_var("FLEET_HOME", &h);
    // A stand-in broker: any executable that prints an OAuth-shaped body. A real
    // one is `omp`, whose `token <provider>` prints exactly this.
    let broker = h.join("broker.sh");
    std::fs::write(
        &broker,
        "#!/bin/sh\n# $1 is the provider id the engine asked for.\nprintf '%s' \"{\\\"oauthAccessToken\\\":\\\"dca:test-token\\\"}\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&broker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let ps = fleet::providers::Providers::default_file();
    ps.put(&Provider {
        name: "muse".into(),
        base_url: "https://api.muse.example/v1".into(),
        key_env: String::new(),
        extra: Default::default(),
        models: vec!["*".into()],
        block: vec![],
        credential: Some(Source::Oauth {
            broker: broker.to_string_lossy().to_string(),
            args: vec![],
            provider: "muse-code".into(),
            account: None,
            force_refresh: false,
            ttl_s: Some(300),
            json_field: "oauthAccessToken".into(),
        }),
        enabled: true,
        notes: String::new(),
    })
    .unwrap();

    let p = ps.get("muse").unwrap();
    assert!(p.source().label().starts_with("oauth:"));
    assert!(p.source().is_dynamic(), "an OAuth source needs a subprocess");

    // The environment a harness receives must carry the TOKEN, not the JSON body
    // that wrapped it: handing a harness the literal JSON is a 401 that looks
    // like an expired login.
    let env = p.env();
    let key = env
        .iter()
        .find(|(k, _)| k == "OPENAI_API_KEY")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert!(
        !key.starts_with('{'),
        "the raw JSON body must not be the credential: {key}"
    );
    assert_eq!(key, "dca:test-token", "the token, not the JSON around it");

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_broker_that_is_not_logged_in_says_so() {
    let c = Credentials::new();
    let s = Source::Oauth {
        broker: "sh".into(),
        args: vec!["-c".into(), "echo 'not logged in' >&2; exit 3".into()],
        provider: "muse-code".into(),
        account: None,
        force_refresh: false,
        ttl_s: None,
        json_field: "oauthAccessToken".into(),
    };
    // A broker that reports "not logged in" and exits non-zero. The message must
    // point at the login, not at this engine's configuration.
    let err = c.resolve("k", &s, false).unwrap_err().to_string();
    assert!(
        err.contains("could not produce a token"),
        "the failure is named as a token failure: {err}"
    );
}

#[test]
fn a_missing_broker_is_reported_as_a_missing_broker() {
    let c = Credentials::new();
    let s = Source::Oauth {
        broker: "/nonexistent/broker-binary".into(),
        args: vec![],
        provider: "x".into(),
        account: None,
        force_refresh: false,
        ttl_s: None,
        json_field: "oauthAccessToken".into(),
    };
    let err = c.resolve("k", &s, false).unwrap_err().to_string();
    assert!(
        err.contains("not runnable") || err.contains("spawning"),
        "a missing broker is distinct from a logged-out provider: {err}"
    );
}

#[test]
fn a_vault_command_source_resolves_and_caches() {
    let c = Credentials::new();
    let s = Source::Command {
        command: "sh".into(),
        args: vec!["-c".into(), "echo vault-secret".into()],
        env: Default::default(),
        ttl_s: Some(300),
        json_field: None,
    };
    assert_eq!(c.resolve("k", &s, false).unwrap().as_deref(), Some("vault-secret"));
    // The cache makes the second call free; invalidating is what makes a
    // rotation visible.
    c.invalidate(&s);
    assert_eq!(c.resolve("k", &s, false).unwrap().as_deref(), Some("vault-secret"));
}

#[test]
fn a_rejected_call_is_recognised_as_an_auth_failure() {
    // The refresh path keys off this predicate, so a false negative means a
    // stale OAuth token never recovers.
    for msg in [
        "harness `x` HTTP 401: {\"error\":{\"message\":\"Invalid API key\"}}",
        "HTTP 401 Unauthorized",
        "invalid_api_key: the key is not valid",
        "token expired",
        "Authentication failed",
        "invalid_grant",
    ] {
        assert!(
            fleet::engine::looks_like_auth_failure(msg),
            "should be treated as auth: {msg}"
        );
    }
    // And a real failure must NOT be, or every timeout would cost a retry.
    for msg in [
        "harness `x` exited 2: a value is required for '--model'",
        "connection refused",
        "context deadline exceeded",
    ] {
        assert!(
            !fleet::engine::looks_like_auth_failure(msg),
            "should not be treated as auth: {msg}"
        );
    }
}

// -------------------------------------------------------- deploy targets

#[test]
fn a_target_is_discovered_from_its_directory() {
    let _g = env_lock();
    let h = home("target-discovery");
    std::env::set_var("FLEET_HOME", &h);
    let t = Targets::default_dir();
    assert!(t.all().is_empty());

    write_target(
        &h,
        "mycloud",
        "#!/bin/sh\necho '{\"ready\":true,\"detail\":\"ok\"}'\n",
    );
    let all = t.all();
    assert_eq!(all.len(), 1, "the directory IS the registration");
    assert_eq!(all[0].scheme, "mycloud");
    assert!(t.has("mycloud"));
    assert!(!t.has("nope"));

    // Removing the directory removes the target.
    std::fs::remove_dir_all(h.join("deploy-targets/mycloud")).unwrap();
    assert!(t.all().is_empty());

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn probe_plan_and_deploy_are_separate_verbs() {
    let _g = env_lock();
    let h = home("target-verbs");
    std::env::set_var("FLEET_HOME", &h);
    // A target that logs which verb it was asked for, so the contract can be
    // asserted rather than assumed.
    write_target(
        &h,
        "logger",
        r#"#!/bin/sh
verb="$1"; req=$(cat)
echo "$verb" >> /tmp/fleet-target-log.txt
case "$verb" in
  probe)  echo '{"ready":true,"detail":"logged in"}' ;;
  plan)   echo '{"commands":["do the thing"],"url":"http://x/health"}' ;;
  deploy) echo "deployed ok" ;;
esac
"#,
    );
    let log = PathBuf::from("/tmp/fleet-target-log.txt");
    let _ = std::fs::remove_file(&log);

    let t = Targets::default_dir();
    let spec: TargetSpec = t.get("logger").unwrap();
    let r = req("logger", "host1");

    let p = t.probe(&spec, &r);
    assert!(p.ready);
    assert_eq!(p.detail, "logged in");

    let plan = t.plan(&spec, &r);
    assert_eq!(plan.commands, vec!["do the thing"]);
    assert_eq!(plan.url, "http://x/health");

    // A PLAN MUST NOT DEPLOY. Asking for a plan and having it act is the worst
    // possible bug in a dry run, so the log proves the verb.
    let after_plan = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(after_plan.contains("plan"));
    assert!(
        !after_plan.contains("deploy"),
        "plan must not have run deploy: {after_plan}"
    );

    let lines = t.deploy(&spec, &r).unwrap();
    assert!(lines.iter().any(|l| l.contains("deployed ok")));
    let after = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(after.contains("deploy"));

    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_target_that_is_not_ready_blocks_the_deploy_and_explains() {
    let _g = env_lock();
    let h = home("target-notready");
    std::env::set_var("FLEET_HOME", &h);
    write_target(
        &h,
        "offline",
        r#"#!/bin/sh
verb="$1"
case "$verb" in
  probe) echo '{"ready":false,"detail":"not logged in to the cloud"}'; exit 1 ;;
  deploy) echo "SHOULD NOT RUN"; exit 0 ;;
esac
"#,
    );
    let t = Targets::default_dir();
    let spec = t.get("offline").unwrap();
    let r = req("offline", "host");
    let err = t.deploy(&spec, &r).unwrap_err().to_string();
    assert!(
        err.contains("not logged in to the cloud"),
        "the target's own reason is shown: {err}"
    );
    assert!(
        !err.contains("SHOULD NOT RUN"),
        "the deploy verb must not have run"
    );
    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_target_that_fails_its_probe_on_stdout_keeps_its_reason() {
    // A probe explains itself on stdout; discarding that loses the reason.
    let _g = env_lock();
    let h = home("target-probe-out");
    std::env::set_var("FLEET_HOME", &h);
    write_target(
        &h,
        "noisy",
        "#!/bin/sh\necho '{\"ready\":false,\"detail\":\"the CLI is not installed\"}'\nexit 1\n",
    );
    let t = Targets::default_dir();
    let spec = t.get("noisy").unwrap();
    let r = req("noisy", "host");
    let err = t.deploy(&spec, &r).unwrap_err().to_string();
    assert!(
        err.contains("the CLI is not installed"),
        "the reason from stdout survives: {err}"
    );
    std::env::remove_var("FLEET_HOME");
}

#[test]
fn a_target_without_a_probe_is_assumed_ready() {
    let _g = env_lock();
    let h = home("target-noprobe");
    std::env::set_var("FLEET_HOME", &h);
    // The probe verb is optional: requiring it would make every target longer.
    write_target(
        &h,
        "minimal",
        r#"#!/bin/sh
verb="$1"
case "$verb" in
  probe) echo "" ;;
  plan)  echo '{"commands":["x"]}' ;;
  deploy) echo "done" ;;
esac
"#,
    );
    let t = Targets::default_dir();
    let spec = t.get("minimal").unwrap();
    let r = req("minimal", "host");
    assert!(t.probe(&spec, &r).ready);
    assert!(t.deploy(&spec, &r).is_ok());
    std::env::remove_var("FLEET_HOME");
}

#[test]
fn an_unknown_scheme_is_a_plugin_target_not_a_parse_error() {
    // `Target::parse` must not reject an unknown scheme: it may be a plugin.
    use fleet::deploy::Target;
    assert_eq!(Target::parse("mycloud://host").unwrap().0, Target::Plugin);
    assert_eq!(Target::parse("k8s://ctx").unwrap().0, Target::Plugin);
    // A scheme with no destination is still an error.
    assert!(Target::parse("mycloud://").is_err());
    // And the built-ins are unchanged.
    assert_eq!(Target::parse("ssh://box").unwrap().0, Target::Ssh);
    assert_eq!(Target::parse("box").unwrap().0, Target::Ssh);
}

#[test]
fn the_target_contract_is_documented_by_a_working_example() {
    // The shipped example must parse and implement all three verbs, because it
    // is what a person copies.
    let spec: TargetSpec = toml::from_str(fleet::deploy_targets::EXAMPLE_SPEC).unwrap();
    assert_eq!(spec.scheme, "myssh");
    assert!(!spec.example.is_empty());
    let script = fleet::deploy_targets::EXAMPLE_SCRIPT;
    for verb in ["probe", "plan", "deploy"] {
        assert!(script.contains(verb), "the example implements {verb}");
    }
    assert!(
        script.contains("json_field") || script.contains("json()"),
        "the example shows how to read the request"
    );
}
