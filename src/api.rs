//! HTTP surface: the same engine the CLI uses, reachable by anything.
//!
//! Deliberately small — a handful of routes over `tiny_http`, no framework. The
//! API exists so a hosted app, a script, or another agent can drive the fleet
//! without a shell:
//!
//!   GET  /health              liveness + harness count
//!   GET  /v1/harnesses        the live registry (re-read every call)
//!   POST /v1/harnesses        install a descriptor: body is the TOML
//!   DEL  /v1/harnesses/{name} remove one; takes effect on the next call
//!   GET  /v1/harnesses/{name}/probe   can it run right now?
//!   POST /v1/run              {task, harness?, mode?, session?, resume?}
//!   GET  /v1/sessions         recent sessions with turn counts
//!   GET  /v1/sessions/{id}    full transcript
//!   GET  /v1/memory           ?scope=&prefix=&q=
//!   PUT  /v1/memory           {scope, key, value} — durable before it answers
//!   GET  /v1/lessons          what the fleet has learned
//!   GET  /v1/ledger           ?kind=&limit=
//!   POST /v1/selfmod          verify-and-mark a self-modification
//!   GET  /v1/config           effective configuration
//!
//! Every mutating route answers only after the write is durable, so a caller
//! that gets 200 can immediately read the value back.

use anyhow::Result;
use serde_json::json;
use tiny_http::{Header, Method, Response, Server};

use crate::engine::{self, RunOptions};
use crate::ledger::Ledger;
use crate::memory::Memory;
use crate::registry::{Harness, Registry};

fn cors() -> Vec<Header> {
    vec![
        Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        Header::from_bytes(
            &b"Access-Control-Allow-Methods"[..],
            &b"GET,POST,PUT,DELETE,OPTIONS"[..],
        )
        .unwrap(),
        Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"content-type"[..]).unwrap(),
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap(),
    ]
}

fn json_response(code: u16, body: serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".into());
    let mut r = Response::from_string(text).with_status_code(code);
    for h in cors() {
        r = r.with_header(h);
    }
    r
}

/// Serve the API on `host:port` until the process is stopped.
pub fn serve(host: &str, port: u16) -> Result<()> {
    crate::paths::ensure_dirs()?;
    // A service started without an EnvironmentFile still needs its keys.
    crate::paths::load_env_file();
    let addr = format!("{host}:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow::anyhow!("binding {addr}: {e}"))?;
    eprintln!(
        "fleet api listening on http://{addr}  (harness dir {})",
        crate::paths::harness_dir().display()
    );
    let registry = Registry::default_dir();

    for mut request in server.incoming_requests() {
        let method = request.method().clone();
        let url = request.url().to_string();
        let path = url.split('?').next().unwrap_or("").to_string();
        let query = url
            .split_once('?')
            .map(|(_, q)| q.to_string())
            .unwrap_or_default();
        // Headers first: the request is consumed by reading its body.
        let headers: Vec<(String, String)> = request
            .headers()
            .iter()
            .map(|h| {
                (
                    h.field.as_str().as_str().to_ascii_lowercase(),
                    h.value.as_str().to_string(),
                )
            })
            .collect();
        let mut body = String::new();
        {
            let r = request.as_reader();
            let _ = r.read_to_string(&mut body);
        }
        if method == Method::Options {
            let _ = request.respond(json_response(204, json!({})));
            continue;
        }
        let q = Query::parse(&query);
        let result: Result<Response<std::io::Cursor<Vec<u8>>>> = (|| {
            // The embedded app, before anything else: it is a GET on a path
            // that no API route claims, so there is nothing to disambiguate.
            if method == Method::Get {
                if let Some((ctype, body)) = crate::webapp::route(&path) {
                    let mut r = Response::from_string(body).with_status_code(200);
                    for h in cors() {
                        r = r.with_header(h);
                    }
                    r = r.with_header(
                        Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
                    );
                    return Ok(r);
                }
            }
            match (&method, path.as_str()) {
                (Method::Get, "/health") => {
                    let (all, errors) = registry.all();
                    Ok(json_response(
                        200,
                        json!({
                            "ok": true,
                            "version": env!("CARGO_PKG_VERSION"),
                            "home": crate::paths::home().to_string_lossy(),
                            "harness_dir": registry.dir().to_string_lossy(),
                            "harnesses": all.len(),
                            "enabled": all.iter().filter(|h| h.enabled).count(),
                            "load_errors": errors.len(),
                            "time": crate::now(),
                        }),
                    ))
                }
                (Method::Get, "/v1/harnesses") => {
                    let (all, errors) = registry.all();
                    let rows: Vec<serde_json::Value> = all
                        .iter()
                        .map(|h| {
                            let (available, why) = crate::runner::probe(h);
                            harness_json(h, available, &why)
                        })
                        .collect();
                    Ok(json_response(
                        200,
                        json!({
                            "harnesses": rows,
                            "errors": errors.iter().map(|(p, e)| json!({"path": p.to_string_lossy(), "error": e})).collect::<Vec<_>>(),
                        }),
                    ))
                }
                (Method::Post, "/v1/harnesses") => {
                    // Body is the descriptor itself. Written atomically, so a
                    // concurrent reader never sees a half-file.
                    let name = match toml::from_str::<Harness>(&body) {
                        Ok(h) => h.name,
                        Err(e) => {
                            return Ok(json_response(
                                400,
                                json!({"error": format!("invalid descriptor: {e}")}),
                            ))
                        }
                    };
                    // The HTTP API is an OPERATOR door, like the CLI — not the
                    // model-driven path. The self-modification ceiling bounds
                    // what the engine may do to itself on a model's say-so; it
                    // must not stop the person who owns the machine from adding
                    // a harness. Authorization here is a bearer token when one
                    // is configured, and nothing more, which is why exposing
                    // this port without a proxy in front is documented as a bad
                    // idea.
                    if let Some(expected) = api_token() {
                        let got = bearer(&headers);
                        if got.as_deref() != Some(expected.as_str()) {
                            return Ok(json_response(
                                401,
                                json!({"error": "this API requires a bearer token: set FLEET_API_TOKEN"}),
                            ));
                        }
                    }
                    let rel = format!("harnesses/{name}.toml");
                    let led = Ledger::default_path();
                    led.log(
                        "harness",
                        "api",
                        "",
                        &rel,
                        "installed over the HTTP API",
                        true,
                    );
                    let dir = registry.dir();
                    std::fs::create_dir_all(dir)?;
                    let tmp = dir.join(format!(".{name}.tmp"));
                    std::fs::write(&tmp, &body)?;
                    let parsed = registry.install(&tmp);
                    let _ = std::fs::remove_file(&tmp);
                    match parsed {
                        Ok(h) => Ok(json_response(
                            201,
                            json!({"installed": h.name, "path": h.source.map(|p| p.to_string_lossy().to_string())}),
                        )),
                        Err(e) => Ok(json_response(400, json!({"error": e.to_string()}))),
                    }
                }
                (Method::Delete, p) if p.starts_with("/v1/harnesses/") => {
                    let name = &p["/v1/harnesses/".len()..];
                    if name.is_empty() {
                        return Ok(json_response(400, json!({"error": "name required"})));
                    }
                    match registry.remove(name) {
                        Ok(path) => {
                            Ledger::default_path().log(
                                "harness",
                                "api",
                                "",
                                name,
                                "removed over the HTTP API",
                                true,
                            );
                            Ok(json_response(
                                200,
                                json!({"removed": name, "path": path.to_string_lossy()}),
                            ))
                        }
                        Err(e) => Ok(json_response(404, json!({"error": e.to_string()}))),
                    }
                }
                (Method::Get, p) if p.starts_with("/v1/harnesses/") && p.ends_with("/probe") => {
                    let name = &p["/v1/harnesses/".len()..p.len() - "/probe".len()];
                    match registry.get(name) {
                        Ok(h) => {
                            let (ok, why) = crate::runner::probe(&h);
                            Ok(json_response(
                                200,
                                json!({"harness": name, "available": ok, "detail": why}),
                            ))
                        }
                        Err(e) => Ok(json_response(404, json!({"error": e.to_string()}))),
                    }
                }
                (Method::Get, p) if p.starts_with("/v1/harnesses/") => {
                    let name = &p["/v1/harnesses/".len()..];
                    match registry.get(name) {
                        Ok(h) => {
                            let (ok, why) = crate::runner::probe(&h);
                            Ok(json_response(200, harness_json(&h, ok, &why)))
                        }
                        Err(e) => Ok(json_response(404, json!({"error": e.to_string()}))),
                    }
                }
                (Method::Post, "/v1/run") => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let task = v
                        .get("task")
                        .or_else(|| v.get("prompt"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string();
                    let resume = v
                        .get("resume")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string());
                    if task.trim().is_empty() && resume.is_none() {
                        return Ok(json_response(400, json!({"error": "task is required"})));
                    }
                    let mode = v.get("mode").and_then(|m| m.as_str()).unwrap_or("auto");
                    let opts = RunOptions {
                        harness: v
                            .get("harness")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        unison: mode == "unison"
                            || v.get("unison").and_then(|b| b.as_bool()).unwrap_or(false),
                        serial: mode == "serial",
                        resume,
                        session: v
                            .get("session")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        verbose: false,
                        brief_prefix: v
                            .get("brief")
                            .and_then(|t| t.as_str())
                            .map(|s| s.to_string()),
                        no_lessons: v
                            .get("no_lessons")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false),
                        dry: false,
                    };
                    let res = task_blocking(task, opts)?;
                    Ok(json_response(200, res))
                }
                (Method::Get, "/v1/sessions") => {
                    let mem = Memory::open_default()?;
                    let limit = q.get_i64("limit", 50);
                    let rows: Vec<serde_json::Value> = mem
                        .sessions(limit)?
                        .into_iter()
                        .map(|s| serde_json::to_value(s).unwrap_or(json!({})))
                        .collect();
                    Ok(json_response(200, json!({"sessions": rows})))
                }
                (Method::Get, p) if p.starts_with("/v1/sessions/") => {
                    let id = &p["/v1/sessions/".len()..];
                    let mem = Memory::open_default()?;
                    let turns = mem.turns(id, q.get_i64("limit", 500))?;
                    Ok(json_response(
                        200,
                        json!({"session": id, "turns": turns, "context": mem.resume_context(id, 20)?}),
                    ))
                }
                (Method::Get, "/v1/memory") => {
                    let mem = Memory::open_default()?;
                    let limit = q.get_i64("limit", 100);
                    if let Some(needle) = q.get("q") {
                        let rows: Vec<serde_json::Value> = mem
                            .search(&needle, limit)?
                            .into_iter()
                            .map(|(s, k, v)| json!({"scope": s, "key": k, "value": v}))
                            .collect();
                        return Ok(json_response(200, json!({"results": rows})));
                    }
                    let scope = q.get("scope").unwrap_or_else(|| "global".into());
                    let prefix = q.get("prefix").unwrap_or_default();
                    let rows: Vec<serde_json::Value> = mem
                        .list(&scope, &prefix, limit)?
                        .into_iter()
                        .map(|(k, v)| json!({"key": k, "value": v}))
                        .collect();
                    Ok(json_response(200, json!({"scope": scope, "entries": rows})))
                }
                (Method::Put, "/v1/memory") | (Method::Post, "/v1/memory") => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let scope = v.get("scope").and_then(|x| x.as_str()).unwrap_or("global");
                    let key = v.get("key").and_then(|x| x.as_str()).unwrap_or("");
                    let value = v.get("value").and_then(|x| x.as_str()).unwrap_or("");
                    if key.is_empty() {
                        return Ok(json_response(400, json!({"error": "key is required"})));
                    }
                    let mem = Memory::open_default()?;
                    let version = mem.put(scope, key, value, "api")?;
                    Ok(json_response(
                        200,
                        json!({"scope": scope, "key": key, "version": version}),
                    ))
                }
                (Method::Get, p) if p.starts_with("/v1/memory/history/") => {
                    let rest = &p["/v1/memory/history/".len()..];
                    let (scope, key) = rest.split_once('/').unwrap_or(("global", rest));
                    let mem = Memory::open_default()?;
                    let rows: Vec<serde_json::Value> = mem
                        .history(scope, key, q.get_i64("limit", 50))?
                        .into_iter()
                        .map(|(v, val, at)| json!({"version": v, "value": val, "replaced": at}))
                        .collect();
                    Ok(json_response(
                        200,
                        json!({"scope": scope, "key": key, "history": rows}),
                    ))
                }
                (Method::Get, "/v1/lessons") => {
                    let mem = Memory::open_default()?;
                    let l = crate::lessons::briefing(&mem, q.get_i64("limit", 50) as usize)?;
                    Ok(json_response(200, json!({"lessons": l})))
                }
                (Method::Get, "/v1/ledger") => {
                    let led = Ledger::default_path();
                    let kind = q.get("kind");
                    let rows = led.tail(kind.as_deref(), q.get_i64("limit", 100) as usize)?;
                    Ok(json_response(200, json!({"entries": rows})))
                }
                (Method::Post, "/v1/selfmod") => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let target = v.get("target").and_then(|x| x.as_str()).unwrap_or("");
                    let reason = v.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                    let content = v.get("content").and_then(|x| x.as_str());
                    if target.is_empty() {
                        return Ok(json_response(400, json!({"error": "target is required"})));
                    }
                    let grant = crate::selfmod::Grant {
                        target: target.to_string(),
                        reason: reason.to_string(),
                        content: content.map(|s| s.to_string()),
                        digest: String::new(),
                    };
                    let ceiling = crate::selfmod::Ceiling::load();
                    let led = Ledger::default_path();
                    match crate::selfmod::propose(&ceiling, &grant, &led, "api") {
                        Ok(digest) => {
                            // Admission is a promise that the artifact matches
                            // the digest. Applying it is the caller's business.
                            Ok(json_response(
                                200,
                                json!({"admitted": true, "digest": digest}),
                            ))
                        }
                        Err(e) => Ok(json_response(
                            403,
                            json!({"admitted": false, "error": e.to_string()}),
                        )),
                    }
                }
                (Method::Get, "/v1/selfmod") => {
                    let led = Ledger::default_path();
                    Ok(json_response(200, crate::selfmod::status(&led)?))
                }
                (Method::Get, "/v1/credentials") => {
                    // Never returns a value: the SOURCE and whether it resolved.
                    // A web panel is the last place a token should be rendered.
                    let ps = crate::providers::Providers::default_file();
                    let creds = crate::credential::Credentials::new();
                    let rows: Vec<serde_json::Value> = ps
                        .all()
                        .iter()
                        .map(|p| {
                            let src = p.source();
                            let (ok, detail) = match p.key_with(&creds, false) {
                                Ok(Some(_)) => (true, "resolved".to_string()),
                                Ok(None) => (false, "source is empty".to_string()),
                                Err(e) => (false, e.to_string()),
                            };
                            json!({
                                "provider": p.name,
                                "source": src.label(),
                                "dynamic": src.is_dynamic(),
                                "ok": ok,
                                "detail": detail,
                            })
                        })
                        .collect();
                    Ok(json_response(200, json!({"credentials": rows})))
                }
                (Method::Post, "/v1/credentials/refresh") => {
                    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
                    let want = v.get("provider").and_then(|x| x.as_str());
                    let ps = crate::providers::Providers::default_file();
                    let creds = crate::credential::Credentials::new();
                    let mut done = Vec::new();
                    for p in ps.all() {
                        if let Some(w) = want {
                            if p.name != w {
                                continue;
                            }
                        }
                        let src = p.source();
                        creds.invalidate(&src);
                        let ok = matches!(p.key_with(&creds, true), Ok(Some(_)));
                        done.push(json!({"provider": p.name, "source": src.label(), "ok": ok}));
                    }
                    Ok(json_response(200, json!({"refreshed": done})))
                }
                (Method::Get, "/v1/targets") => {
                    let t = crate::deploy_targets::Targets::default_dir();
                    let rows: Vec<serde_json::Value> = t
                        .all()
                        .iter()
                        .map(|s| {
                            json!({
                                "scheme": s.scheme,
                                "description": s.description,
                                "example": s.example,
                                "url_template": s.url_template,
                            })
                        })
                        .collect();
                    let builtin = ["ssh", "tailscale", "reason", "docker", "file"];
                    Ok(json_response(
                        200,
                        json!({
                            "builtin": builtin,
                            "plugin": rows,
                            "dir": t.dir().to_string_lossy(),
                        }),
                    ))
                }
                (Method::Get, "/v1/config") => {
                    let c = crate::config::Config::load();
                    Ok(json_response(
                        200,
                        json!({
                            "chooser": c.chooser,
                            "defaults": c.defaults,
                            "max_parallel": c.parallel(),
                            "path": crate::paths::config_path().to_string_lossy(),
                        }),
                    ))
                }
                (Method::Get, "/v1/runs") => {
                    let mem = Memory::open_default()?;
                    Ok(json_response(
                        200,
                        json!({"runs": mem.runs(q.get_i64("limit", 50))?}),
                    ))
                }
                (Method::Get, "/") | (Method::Get, "/v1") => Ok(json_response(
                    200,
                    json!({
                        "name": "fleet",
                        "what": "a harness for harnesses",
                        "docs": "https://github.com/evergreentechnologywy/fleet",
                        "routes": [
                            "GET /health",
                            "GET /v1/harnesses",
                            "POST /v1/harnesses",
                            "DELETE /v1/harnesses/{name}",
                            "GET /v1/harnesses/{name}/probe",
                            "POST /v1/run",
                            "GET /v1/sessions",
                            "GET /v1/sessions/{id}",
                            "GET /v1/memory",
                            "PUT /v1/memory",
                            "GET /v1/memory/history/{scope}/{key}",
                            "GET /v1/lessons",
                            "GET /v1/ledger",
                            "GET /v1/selfmod",
                            "POST /v1/selfmod",
                            "GET /v1/credentials",
                            "POST /v1/credentials/refresh",
                            "GET /v1/targets",
                            "GET /v1/config",
                            "GET /v1/runs",
                        ],
                    }),
                )),
                _ => Ok(json_response(
                    404,
                    json!({"error": "no such route", "path": path}),
                )),
            }
        })();

        let response = match result {
            Ok(r) => r,
            Err(e) => json_response(500, json!({"error": e.to_string()})),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

/// The bearer token this API requires, if one is configured. Unset means the
/// API is open, which is only safe on loopback or behind a proxy.
fn api_token() -> Option<String> {
    std::env::var("FLEET_API_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
}

/// Read a bearer token out of the collected headers.
fn bearer(headers: &[(String, String)]) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k == "authorization")
        .and_then(|(_, v)| v.strip_prefix("Bearer ").map(|s| s.trim().to_string()))
        .or_else(|| {
            headers
                .iter()
                .find(|(k, _)| k == "x-fleet-token")
                .map(|(_, v)| v.trim().to_string())
        })
}

/// Runs are blocking; the caller streams nothing back until the answer is whole.
fn task_blocking(task: String, opts: RunOptions) -> Result<serde_json::Value> {
    let res = engine::run(&task, &opts)?;
    Ok(serde_json::to_value(res)?)
}

fn harness_json(h: &Harness, available: bool, why: &str) -> serde_json::Value {
    json!({
        "name": h.name,
        "description": h.description,
        "kind": format!("{:?}", h.kind).to_lowercase(),
        "route": format!("{:?}", h.route).to_lowercase(),
        "model": h.model,
        "models": h.models,
        "effort": h.effort,
        "tags": h.tags,
        "priority": h.priority,
        "cost_hint": h.cost_hint,
        "verifier": h.verifier,
        "enabled": h.enabled,
        "available": available,
        "detail": why,
        "source": h.source.as_ref().map(|p| p.to_string_lossy().to_string()),
    })
}

/// Minimal query-string parsing: `?a=1&b=two`.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(s: &str) -> Self {
        let mut out = Vec::new();
        for pair in s.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.push((url_decode(k), url_decode(v)));
        }
        Query(out)
    }
    fn get(&self, key: &str) -> Option<String> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    }
    fn get_i64(&self, key: &str, default: i64) -> i64 {
        self.get(key)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(default)
    }
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}
