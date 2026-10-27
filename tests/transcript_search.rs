//! Contract tests for cross-harness transcript search.
//!
//! Each test pins a trap the module exists to prevent:
//! * a harness replaying its context (Codex world_state, omp custom) matched as
//!   a hit, which would bury real turns under thousands of preamble echoes;
//! * multiline snippets breaking line-oriented pipelines;
//! * case mismatches causing searches for exact terms to return nothing.

use std::fs;
use std::path::{Path, PathBuf};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fleet-test-search-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_jsonl(path: &Path, lines: &[&str]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let body = lines.join("\n") + "\n";
    fs::write(path, body).unwrap();
}

#[test]
fn searches_across_different_harness_formats() {
    let root = tmpdir("multi");
    let omp_dir = root.join("omp");
    let claude_dir = root.join("claude");
    let codex_dir = root.join("codex");

    // omp transcript
    write_jsonl(
        &omp_dir.join("session-omp.jsonl"),
        &[
            r#"{"type":"session","id":"omp-1"}"#,
            r#"{"type":"message","timestamp":"2026-10-27T12:00:00Z","message":{"role":"user","content":[{"type":"text","text":"deploy to coolify now"}]}}"#,
        ],
    );

    // Claude Code transcript
    write_jsonl(
        &claude_dir.join("session-claude.jsonl"),
        &[
            r#"{"type":"user","timestamp":"2026-10-27T13:00:00Z","message":{"role":"user","content":[{"type":"text","text":"deploy to Coolify was successful"}]}}"#,
        ],
    );

    // Codex transcript
    write_jsonl(
        &codex_dir.join("session-codex.jsonl"),
        &[
            r#"{"type":"session_meta","payload":{"id":"codex-1"}}"#,
            r#"{"type":"response_item","timestamp":"2026-10-27T14:00:00Z","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Deploy to COOLIFY complete."}]}}"#,
        ],
    );

    let roots = vec![
        fleet::transcript::Root {
            source: fleet::transcript::Source::Omp,
            dir: omp_dir,
        },
        fleet::transcript::Root {
            source: fleet::transcript::Source::Claude,
            dir: claude_dir,
        },
        fleet::transcript::Root {
            source: fleet::transcript::Source::Codex,
            dir: codex_dir,
        },
    ];

    let hits = fleet::transcript::search(
        "coolify",
        &roots,
        &fleet::transcript::Options {
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();

    // All three harnesses returned a hit.
    assert_eq!(hits.len(), 3, "expected 3 hits, got: {hits:#?}");
    let sources: Vec<&str> = hits.iter().map(|h| h.source).collect();
    assert!(sources.contains(&"omp"));
    assert!(sources.contains(&"claude"));
    assert!(sources.contains(&"codex"));

    // Sorted newest first.
    assert_eq!(hits[0].source, "codex");
    assert_eq!(hits[1].source, "claude");
    assert_eq!(hits[2].source, "omp");
}

#[test]
fn plumbing_records_do_not_produce_false_hits() {
    let root = tmpdir("plumbing");
    let codex_dir = root.join("codex");

    // Codex world_state replaying AGENTS.md, which contains "standing-rules".
    // Must NOT be reported as a hit.
    write_jsonl(
        &codex_dir.join("session.jsonl"),
        &[
            r#"{"type":"world_state","payload":{"state":{"agents_md":{"text":"see standing-rules.md"}}}}"#,
            r#"{"type":"event_msg","payload":{"text":"standing-rules referenced"}}"#,
            // Only this real turn should match:
            r#"{"type":"response_item","timestamp":"2026-10-27T10:00:00Z","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"did we update standing-rules?"}]}}"#,
        ],
    );

    let roots = vec![fleet::transcript::Root {
        source: fleet::transcript::Source::Codex,
        dir: codex_dir,
    }];

    let hits = fleet::transcript::search(
        "standing-rules",
        &roots,
        &fleet::transcript::Options::default(),
    )
    .unwrap();

    assert_eq!(
        hits.len(),
        1,
        "plumbing records must be filtered out: {hits:#?}"
    );
    assert_eq!(hits[0].line, 3);
}

#[test]
fn snippets_are_single_line_and_contain_the_needle() {
    let root = tmpdir("snippet");
    let omp_dir = root.join("omp");

    write_jsonl(
        &omp_dir.join("s.jsonl"),
        &[
            r#"{"type":"message","timestamp":"2026-10-27T10:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"Line 1\nLine 2 with target-word here\nLine 3"}]}}"#,
        ],
    );

    let roots = vec![fleet::transcript::Root {
        source: fleet::transcript::Source::Omp,
        dir: omp_dir,
    }];

    let hits = fleet::transcript::search(
        "target-word",
        &roots,
        &fleet::transcript::Options {
            context: 15,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(hits.len(), 1);
    let s = &hits[0].snippet;
    assert!(
        s.contains("target-word"),
        "snippet must contain needle: {s}"
    );
    assert!(
        !s.contains('\n'),
        "snippet must be collapsed to a single line: {s}"
    );
}

#[test]
fn limit_caps_results() {
    let root = tmpdir("limit");
    let omp_dir = root.join("omp");

    let lines: Vec<String> = (0..20)
        .map(|i| {
            format!(
                r#"{{"type":"message","timestamp":"2026-10-27T10:{i:02}:00Z","message":{{"role":"user","content":[{{"type":"text","text":"repeat needle item {i}"}}]}}}}"#
            )
        })
        .collect();
    let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    write_jsonl(&omp_dir.join("s.jsonl"), &line_refs);

    let roots = vec![fleet::transcript::Root {
        source: fleet::transcript::Source::Omp,
        dir: omp_dir,
    }];

    let hits = fleet::transcript::search(
        "needle",
        &roots,
        &fleet::transcript::Options {
            limit: 5,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(hits.len(), 5);
}
