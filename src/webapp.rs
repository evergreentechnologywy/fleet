//! The web app: one page, served by the engine, driving the same API the CLI
//! and every script use.
//!
//! Deliberately a single self-contained document — no build step, no bundler, no
//! node_modules. It is embedded in the binary with `include_str!`, so `fleet
//! serve` gives you a working local *and* hosted app with nothing else installed.
//! That is the same principle as a harness being a file: the app is one artifact,
//! and it cannot drift from the API because it calls the routes documented at
//! `/v1`.
//!
//! What it does, in order of how often you need it:
//!
//! * **run a task** — pick a harness or let the planner choose, single/unison/
//!   serial, and see each harness's answer with the model it was served;
//! * **manage harnesses** — the live list with availability, plus add (paste or
//!   drop a TOML file) and remove, with no restart on either side;
//! * **read the conversation** — sessions and their full transcripts, and the
//!   shared memory those turns wrote;
//! * **inspect the engine** — the ledger, the lessons, and the self-mod ceiling.

/// The page, embedded so there is no static-file path to get wrong.
pub const INDEX: &str = include_str!("webapp.html");

/// A tiny stylesheet is inline in the page; this is the only other asset, so a
/// browser's cache has exactly two entries.
pub const FAVICON: &[u8] = b"";

/// Serve the app for a request path. Returns `None` when the path is not the
/// app, so the caller falls through to the API routes.
pub fn route(path: &str) -> Option<(&'static str, &'static str)> {
    match path {
        "/app" | "/app/" | "/ui" | "/" => Some(("text/html; charset=utf-8", INDEX)),
        _ => None,
    }
}
