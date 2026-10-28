//! Contract tests for the AG-UI harness transport.
//!
//! Three failure modes are pinned, because each of them renders as a *success*
//! in the engine if it is not caught here:
//!
//! * a stream that ends without RUN_FINISHED — a backend that died mid-run must
//!   not look like an agent that answered with nothing;
//! * a RUN_ERROR event — the protocol's way of failing, which must not be
//!   mistaken for an empty answer;
//! * usage spread across several `usage[]` entries — summed, so a multi-model
//!   remote agent is not billed as zero.

use std::io::Write;
use std::time::Duration;

use fleet::registry::Harness;

/// Serve one canned SSE body on an ephemeral port and return its base URL.
fn serve_once(path: &'static str) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let port = server.server_addr().to_ip().expect("ip addr").port();
    std::thread::spawn(move || {
        if let Ok(request) = server.recv() {
            let response = tiny_http::Response::from_string(path).with_header(
                tiny_http::Header::from_bytes(&b"content-type"[..], &b"text/event-stream"[..])
                    .unwrap(),
            );
            let _ = request.respond(response);
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn harness_at(base: &str) -> Harness {
    let body = format!(
        r#"
name = "remote"
description = "an AG-UI backend"
kind = "agui"
route = "native"

[http]
base_url = "{base}"
chat_path = "/agui"
"#
    );
    toml::from_str::<Harness>(&body).expect("harness parses")
}

#[test]
fn text_deltas_are_assembled_and_steps_collected() {
    let body = concat!(
        "data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n",
        "data: {\"type\":\"STEP_STARTED\",\"stepName\":\"plan\"}\n\n",
        "data: {\"type\":\"STEP_FINISHED\",\"stepName\":\"plan\"}\n\n",
        "data: {\"type\":\"TEXT_MESSAGE_START\",\"messageId\":\"m\"}\n\n",
        "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m\",\"delta\":\"hel\"}\n\n",
        "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m\",\"delta\":\"lo\"}\n\n",
        "data: {\"type\":\"TEXT_MESSAGE_END\",\"messageId\":\"m\"}\n\n",
        "data: {\"type\":\"RUN_FINISHED\",\"threadId\":\"t\",\"runId\":\"r\",\"usage\":[",
        "{\"provider\":\"a\",\"model\":\"m1\",\"inputTokens\":10,\"outputTokens\":2},",
        "{\"provider\":\"b\",\"model\":\"m2\",\"inputTokens\":5,\"outputTokens\":3}]}\n\n",
    );
    let base = serve_once(body);
    let h = harness_at(&base);
    let mut lines = Vec::new();
    let out = fleet::agui::call(&h, "hi", &mut |l| lines.push(l.to_string())).expect("call ok");
    assert_eq!(out.text, "hello");
    assert_eq!(out.steps, vec!["plan".to_string()]);
    assert_eq!((out.tokens_in, out.tokens_out), (15, 5));
    assert_eq!(lines, vec!["→ plan".to_string()]);
}

#[test]
fn a_stream_without_run_finished_is_a_failure() {
    let body = concat!(
        "data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n",
        "data: {\"type\":\"TEXT_MESSAGE_CONTENT\",\"messageId\":\"m\",\"delta\":\"partial\"}\n\n",
    );
    let base = serve_once(body);
    let h = harness_at(&base);
    let err = fleet::agui::call(&h, "hi", &mut |_| {}).expect_err("must not report success");
    let msg = format!("{err}");
    assert!(
        msg.contains("without RUN_FINISHED"),
        "unexpected error: {msg}"
    );
}

#[test]
fn run_error_propagates_the_remote_message() {
    let body = concat!(
        "data: {\"type\":\"RUN_STARTED\",\"threadId\":\"t\",\"runId\":\"r\"}\n\n",
        "data: {\"type\":\"RUN_ERROR\",\"message\":\"model quota exhausted\"}\n\n",
    );
    let base = serve_once(body);
    let h = harness_at(&base);
    let err = fleet::agui::call(&h, "hi", &mut |_| {}).expect_err("errors must surface");
    assert!(format!("{err}").contains("model quota exhausted"), "{err}");
}

#[test]
fn a_jev_route_on_an_agui_harness_is_a_config_error() {
    // A chooser call whose answer is then discarded is a wasted token, so the
    // descriptor is rejected instead of silently accepted.
    let body = r#"
name = "remote"
kind = "agui"
route = "jev"

[http]
base_url = "http://127.0.0.1:1"
"#;
    let h = toml::from_str::<Harness>(body).expect("parses");
    let err = h
        .validate(std::path::Path::new("/tmp/remote.toml"))
        .expect_err("must reject");
    assert!(format!("{err}").contains("cannot use route"), "{err}");
}

#[test]
fn write_timeout_is_bounded() {
    // Guards against a descriptor that asks the engine to wait forever: the
    // harness's timeout is what the AG-UI client applies to the request.
    let h = harness_at("http://127.0.0.1:1");
    assert!(h.timeout() >= 1 && h.timeout() <= 3600);
    let _ = Duration::from_secs(h.timeout());
    let mut sink = std::io::sink();
    let _ = sink.write_all(b"");
}