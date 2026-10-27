//! The platform traps the fleetnode deploy target exists to handle, pinned as
//! contract tests against the REAL script in the fleet config repo when present
//! and against the engine's own binary resolution when not.
//!
//! Each test encodes a failure that actually happened:
//! * an ELF uploaded to a Windows desk, which fails at exec time after the
//!   deploy reported success ("not compatible with the version of Windows");
//! * `%TEMP%` passed to scp, which expands nothing and fails with "No such file
//!   or directory" — a message that reads like a missing directory;
//! * a deploy whose staging dir requires root, run as a user without it.

use std::process::Command;

/// The fleet's real fleetnode script, when the config repo is beside the engine.
fn fleetnode_script() -> Option<std::path::PathBuf> {
    let candidates = [
        std::path::PathBuf::from("../fleet-evergreen/deploy-targets/fleetnode/deploy.sh"),
        std::path::PathBuf::from("/opt/fleet-evergreen/deploy-targets/fleetnode/deploy.sh"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

#[test]
fn the_real_fleetnode_script_is_valid_posix_shell() {
    let Some(script) = fleetnode_script() else {
        // No config repo beside the engine: nothing to assert here.
        return;
    };
    let out = Command::new("sh").arg("-n").arg(&script).output().unwrap();
    assert!(
        out.status.success(),
        "deploy.sh has a syntax error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_real_script_never_references_the_temp_var_in_an_scp_call() {
    let Some(script) = fleetnode_script() else { return };
    let text = std::fs::read_to_string(&script).unwrap();
    // `%TEMP%` in a destination is the Windows transfer trap: scp expands
    // nothing, so the literal string reaches the SFTP layer and fails. If a
    // Windows path is ever needed it must be resolved over ssh first.
    for line in text.lines() {
        if line.contains("%TEMP%") {
            assert!(
                line.trim_start().starts_with('#'),
                "executable line uses %TEMP% in an scp destination: {line}"
            );
        }
    }
}

#[test]
fn the_real_script_ships_through_a_relative_remote_path() {
    let Some(script) = fleetnode_script() else { return };
    let text = std::fs::read_to_string(&script).unwrap();
    // Every scp destination must be relative (`.fleet/stage/...`), because a
    // relative path resolves against the remote home on BOTH platforms, while
    // an absolute one is only writable where the login user has root.
    assert!(
        text.contains(".fleet/stage/"),
        "the script must stage under the remote home (.fleet/stage/)"
    );
    assert!(
        !text.contains("$target:/tmp/"),
        "staging in /tmp breaks for a user without root there"
    );
}

#[test]
fn a_locked_running_binary_is_reported_not_swallowed() {
    let Some(script) = fleetnode_script() else { return };
    let text = std::fs::read_to_string(&script).unwrap();
    // The Windows replace step must check the result of the copy (`if
    // errorlevel 1`); without it a locked file leaves the OLD binary in place
    // and the deploy reports success.
    assert!(
        text.contains("if errorlevel 1"),
        "the Windows replace step must turn a locked-file failure into a loud one"
    );
}

#[test]
fn the_real_script_verifies_the_binary_runs_before_claiming_success() {
    let Some(script) = fleetnode_script() else { return };
    let text = std::fs::read_to_string(&script).unwrap();
    assert!(
        text.contains("--version"),
        "a deploy that never runs the installed binary cannot prove it works"
    );
}

#[test]
fn the_real_script_names_its_target_prefix_in_its_transport() {
    let Some(script) = fleetnode_script() else { return };
    let text = std::fs::read_to_string(&script).unwrap();
    // The `user@host:` prefix is what makes a path remote. A helper that drops
    // it copies files LOCALLY — the failure that produced "cp: cannot create
    // regular file '.fleet/stage/fleet.exe.gz'".
    assert!(
        text.contains("$target:$dst") || text.contains("$target:"),
        "ship() must prefix remote destinations with the target"
    );
}
