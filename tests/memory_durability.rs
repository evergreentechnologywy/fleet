//! Shared memory and the session store: instant, last-version-wins, and never
//! losing a conversation.

use fleet::memory::{Memory, Turn};
use std::path::PathBuf;

fn mem(tag: &str) -> (Memory, PathBuf) {
    let p = std::env::temp_dir().join(format!("fleet-mem-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    let _ = std::fs::remove_file(p.with_extension("db-wal"));
    let _ = std::fs::remove_file(p.with_extension("db-shm"));
    (Memory::open(&p).unwrap(), p)
}

fn turn(session: &str, seq: i64, text: &str) -> Turn {
    Turn {
        session: session.into(),
        seq,
        ts: fleet::now(),
        role: "user".into(),
        harness: String::new(),
        model: String::new(),
        text: text.into(),
        run_id: "r1".into(),
        ok: true,
    }
}

#[test]
fn a_write_is_readable_from_a_second_connection_immediately() {
    // "Instant" means: the write returns only after it is durable, so a
    // different connection opened afterwards cannot miss it.
    let (m, path) = mem("instant");
    let v = m.put("global", "answer", "42", "test").unwrap();
    assert_eq!(v, 1);

    let other = Memory::open(&path).unwrap();
    assert_eq!(
        other.get("global", "answer").unwrap().as_deref(),
        Some("42")
    );
}

#[test]
fn the_last_version_wins_and_the_older_one_is_kept() {
    let (m, _) = mem("version");
    assert_eq!(m.put("s", "k", "first", "a").unwrap(), 1);
    assert_eq!(m.put("s", "k", "second", "b").unwrap(), 2);
    assert_eq!(m.put("s", "k", "third", "c").unwrap(), 3);

    // Readers get the newest.
    assert_eq!(m.get("s", "k").unwrap().as_deref(), Some("third"));

    // Nothing was lost: the superseded values are still there, newest first.
    let hist = m.history("s", "k", 10).unwrap();
    assert_eq!(hist.len(), 2, "two superseded versions");
    assert_eq!(hist[0].1, "second");
    assert_eq!(hist[1].1, "first");
    assert_eq!(hist[0].0, 2, "version numbers are carried");
}

#[test]
fn a_turn_survives_a_reopen_in_order() {
    let (m, path) = mem("turns");
    m.session_open("s1", "a conversation").unwrap();
    for i in 1..=5 {
        m.turn_add(&turn("s1", i, &format!("message {i}"))).unwrap();
    }

    let reopened = Memory::open(&path).unwrap();
    let turns = reopened.turns("s1", 100).unwrap();
    assert_eq!(turns.len(), 5);
    assert_eq!(turns[0].text, "message 1", "oldest first");
    assert_eq!(turns[4].text, "message 5");
    assert_eq!(
        reopened.next_seq("s1").unwrap(),
        6,
        "resumes at the next slot"
    );
}

#[test]
fn resume_context_carries_the_transcript_and_the_session_facts() {
    let (m, _) = mem("resume");
    m.session_open("s2", "t").unwrap();
    m.turn_add(&turn("s2", 1, "what did we decide?")).unwrap();
    m.put("session:s2", "decision", "ship the boring design", "test")
        .unwrap();

    let ctx = m.resume_context("s2", 20).unwrap();
    assert!(ctx.contains("what did we decide?"), "transcript replayed");
    assert!(
        ctx.contains("ship the boring design"),
        "session facts replayed: {ctx}"
    );

    // A different session's facts do not leak in.
    m.put("session:other", "decision", "a different thing", "test")
        .unwrap();
    let ctx2 = m.resume_context("s2", 20).unwrap();
    assert!(!ctx2.contains("a different thing"));
}

#[test]
fn sessions_are_listed_newest_first_with_turn_counts() {
    let (m, _) = mem("list");
    m.session_open("older", "first").unwrap();
    m.turn_add(&turn("older", 1, "x")).unwrap();
    m.session_open("newer", "second").unwrap();
    m.turn_add(&turn("newer", 1, "y")).unwrap();
    m.turn_add(&turn("newer", 2, "z")).unwrap();

    let s = m.sessions(10).unwrap();
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].id, "newer", "most recently touched first");
    assert_eq!(s[0].turns, 2);
    assert_eq!(s[1].turns, 1);
}

#[test]
fn scopes_isolate_keys_and_prefixes_filter() {
    let (m, _) = mem("scopes");
    m.put("project:a", "k", "one", "t").unwrap();
    m.put("project:a", "k2", "two", "t").unwrap();
    m.put("project:b", "k", "three", "t").unwrap();

    assert_eq!(m.get("project:a", "k").unwrap().as_deref(), Some("one"));
    assert_eq!(m.get("project:b", "k").unwrap().as_deref(), Some("three"));

    let only_k = m.list("project:a", "k2", 10).unwrap();
    assert_eq!(only_k.len(), 1);
    assert_eq!(only_k[0].1, "two");
}

#[test]
fn search_finds_a_value_by_its_text() {
    let (m, _) = mem("search");
    m.put("global", "host", "the hub is at 100.87.28.18", "t")
        .unwrap();
    m.put("global", "other", "unrelated", "t").unwrap();

    let hits = m.search("100.87", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1, "host");

    // A key match works as well as a value match.
    let hits = m.search("host", 10).unwrap();
    assert!(hits.iter().any(|(_, k, _)| k == "host"));
}

#[test]
fn runs_record_their_outcome() {
    let (m, _) = mem("runs");
    m.session_open("s3", "t").unwrap();
    m.run_start("r-1", "s3", "do a thing", "[]").unwrap();
    m.run_finish("r-1", "ok", "the answer").unwrap();

    let runs = m.runs(10).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "ok");
    assert_eq!(runs[0]["task"], "do a thing");
    assert!(runs[0]["finished"].is_string());
}

#[test]
fn a_queue_item_survives_reopen_and_a_stranded_item_is_recoverable() {
    // The queue's entire value is durability, so the two properties that make it
    // worth having are tested against a real file:
    //   1. an item enqueued by one process is claimable by another (it is on disk,
    //      not in a Vec), and
    //   2. an item left `running` by a process that DIED is recoverable — without
    //      that, the queue looks non-empty but has nothing pending and silently
    //      stops.
    let dir = std::env::temp_dir().join(format!("fleet-queue-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("fleet.db");

    let id = {
        let m = fleet::memory::Memory::open(&db).unwrap();
        m.queue_push("do the thing", "omp", 2).unwrap()
    };

    // A DIFFERENT handle, as another process would open it.
    let m = fleet::memory::Memory::open(&db).unwrap();
    let claimed = m.queue_claim().unwrap().expect("an item must be claimable");
    assert_eq!(claimed.id, id);
    assert_eq!(claimed.task, "do the thing");
    assert_eq!(claimed.harness, "omp");
    assert_eq!(claimed.subagents, 2);
    assert_eq!(claimed.status, "running");

    // Claiming again finds nothing: the row is running, not pending. This is the
    // property that stops two workers running the same item.
    assert!(
        m.queue_claim().unwrap().is_none(),
        "a claimed item must not be claimable twice"
    );

    // Simulate the worker dying: the row is still `running`.
    assert_eq!(m.queue_reap().unwrap(), 1, "reap must find the stranded item");
    assert_eq!(
        m.queue_claim().unwrap().map(|q| q.id),
        Some(id),
        "a reaped item must be claimable again"
    );

    // Finish it and confirm the outcome is durable and readable.
    m.queue_finish(id, "s-abc", "the answer", "").unwrap();
    let got = m.queue_get(id).unwrap().expect("still there");
    assert_eq!(got.status, "done");
    assert_eq!(got.answer, "the answer");
    assert!(got.error.is_empty());

    let counts = m.queue_counts().unwrap();
    assert!(counts.iter().any(|(s, n)| s == "done" && *n == 1), "{counts:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
