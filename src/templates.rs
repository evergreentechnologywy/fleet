//! Templates: everything `fleet onboard` writes.
//!
//! The goal is that a new install reaches a working run with one key and one
//! question — "which provider?" — and everything else is a file the operator can
//! edit. Nothing here is compiled in beyond the text; the files it writes are
//! ordinary descriptors, which is why a harness can be deleted later without
//! touching the engine.

use anyhow::{Context, Result};
use std::path::Path;

/// Well-known harnesses, written as real descriptors when their binary exists.
/// Each one is a different shape on purpose: that is the point of the engine.
pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub tags: &'static [&'static str],
    pub priority: i32,
    /// Rendered TOML.
    pub body: String,
}

/// The descriptor for a generic OpenAI-compatible endpoint — any provider,
/// any custom gateway, no CLI. This is all an install needs to start.
pub fn openai_compat(
    name: &str,
    base_url: &str,
    key_env: &str,
    model: &str,
    tags: &[&str],
) -> String {
    format!(
        r#"# A plain provider: no CLI, no wrapper. fleet calls the endpoint directly.
name = "{name}"
description = "OpenAI-compatible endpoint at {base_url}"
kind = "openai-compat"

# Everything BEFORE the first [table] header belongs to the harness itself.
# TOML has no "back to top level": a key written after [http] belongs to [http],
# so the harness fields come first and the table comes last.
#
# A credential is ALWAYS named as an env var, never written here, so this file
# stays safe to commit and to publish.
api_key_env = "{key_env}"

# `jev` = ask the chooser per call. `fixed` = pin the model below. `native` =
# send no model and let the endpoint decide.
route = "jev"
model = "{model}"
effort = "medium"
models = ["*"]
tags = [{tags}]

priority = 50
cost_hint = 0.0
enabled = true
timeout_s = 600

[http]
base_url = "{base_url}"
chat_path = "/v1/chat/completions"
auth_header = "Authorization"
auth_scheme = "Bearer"
"#,
        tags = tags
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Harnesses detected from `PATH`, each with the flags its CLI actually needs.
pub fn presets_for_path() -> Vec<Preset> {
    let mut out = Vec::new();

    if crate::runner::which("omp").is_some() {
        out.push(Preset {
            name: "omp",
            description: "omp coding agent: tools, PTY, skills, long autonomous runs",
            tags: &["code", "tools", "long", "repo"],
            priority: 90,
            body: r#"name = "omp"
description = "omp coding agent: tools, PTY, skills, long autonomous runs"
kind = "cli"

command = "omp"
# {prompt} is substituted per call. `-p` is print mode: process and exit.
args = ["-p", "{prompt}"]
# omp takes `--model <id>` and `--thinking <level>`.
model_flag = ["--model", "{model}"]
effort_flag = ["--thinking", "{effort}"]
prompt_on_stdin = false

# `self-routing`: point it at the chooser's OpenAI door and let the model it
# picks per turn be a fleet decision. Set route = "jev" instead to pin one model
# per run from the command line.
route = "jev"
models = ["*"]
effort = "high"

tags = ["code", "tools", "long", "repo"]
priority = 90
cost_hint = 0.05
enabled = true
timeout_s = 3600
"#
            .to_string(),
        });
    }

    if crate::runner::which("codex").is_some() {
        out.push(Preset {
            name: "codex",
            description: "Codex CLI: strong single-shot reasoning and review",
            tags: &["code", "review", "reason"],
            priority: 80,
            body: r#"name = "codex"
description = "Codex CLI: strong single-shot reasoning and review"
kind = "cli"

command = "codex"
# `exec` is the non-interactive door; the prompt is a positional argument.
args = ["exec", "--skip-git-repo-check", "{prompt}"]
model_flag = ["-m", "{model}"]
prompt_on_stdin = false

route = "jev"
models = ["*"]
effort = "high"

tags = ["code", "review", "reason"]
priority = 80
cost_hint = 0.08
enabled = true
timeout_s = 3600
"#
            .to_string(),
        });
    }

    if crate::runner::which("opencode").is_some() {
        out.push(Preset {
            name: "opencode",
            description: "opencode: multi-provider agent, good at bulk edits",
            tags: &["code", "bulk", "multi-provider"],
            priority: 70,
            body: [
                "name = \"opencode\"",
                "description = \"opencode: multi-provider agent, good at bulk edits\"",
                "kind = \"cli\"",
                "",
                "command = \"opencode\"",
                "args = [\"run\", \"{prompt}\"]",
                "model_flag = [\"--model\", \"{model}\"]",
                "effort_flag = [\"--variant\", \"{effort}\"]",
                "prompt_on_stdin = false",
                "",
                "route = \"jev\"",
                "models = [\"*\"]",
                "effort = \"medium\"",
                "",
                "tags = [\"code\", \"bulk\", \"multi-provider\"]",
                "priority = 70",
                "cost_hint = 0.04",
                "enabled = true",
                "timeout_s = 1800",
            ]
            .join("\n"),
        });
    }

    if crate::runner::which("claude").is_some() {
        out.push(Preset {
            name: "claude",
            description: "Claude Code: careful long-context work",
            tags: &["code", "long", "careful"],
            priority: 85,
            body: [
                "name = \"claude\"",
                "description = \"Claude Code: careful long-context work\"",
                "kind = \"cli\"",
                "",
                "command = \"claude\"",
                "args = [\"-p\", \"{prompt}\"]",
                "model_flag = [\"--model\", \"{model}\"]",
                "",
                "route = \"jev\"",
                "models = [\"*\"]",
                "",
                "tags = [\"code\", \"long\", \"careful\"]",
                "priority = 85",
                "cost_hint = 0.10",
                "enabled = true",
                "timeout_s = 3600",
            ]
            .join("\n"),
        });
    }

    if crate::runner::which("grok").is_some() {
        out.push(Preset {
            name: "grok",
            description: "Grok CLI: search-heavy and very long context",
            tags: &["search", "long", "chat"],
            priority: 60,
            body: [
                "name = \"grok\"",
                "description = \"Grok CLI: search-heavy and very long context\"",
                "kind = \"cli\"",
                "",
                "command = \"grok\"",
                "args = [\"-p\", \"{prompt}\"]",
                "model_flag = [\"-m\", \"{model}\"]",
                "",
                "route = \"jev\"",
                "models = [\"*\"]",
                "",
                "tags = [\"search\", \"long\", \"chat\"]",
                "priority = 60",
                "cost_hint = 0.02",
                "enabled = true",
                "timeout_s = 1200",
            ]
            .join("\n"),
        });
    }

    if crate::runner::which("gemini").is_some() {
        out.push(Preset {
            name: "gemini",
            description: "Gemini CLI: wide context reads",
            tags: &["long", "vision", "chat"],
            priority: 55,
            body: [
                "name = \"gemini\"",
                "description = \"Gemini CLI: wide context reads\"",
                "kind = \"cli\"",
                "",
                "command = \"gemini\"",
                "args = [\"-p\", \"{prompt}\"]",
                "model_flag = [\"-m\", \"{model}\"]",
                "",
                "route = \"jev\"",
                "models = [\"*\"]",
                "",
                "tags = [\"long\", \"vision\", \"chat\"]",
                "priority = 55",
                "cost_hint = 0.02",
                "enabled = true",
                "timeout_s = 1200",
            ]
            .join("\n"),
        });
    }

    // A verifier: never runs by default, only joins a serial plan to check work.
    out.push(Preset {
        name: "verifier",
        description: "Adversarial check of another harness's answer, no network",
        tags: &["verify"],
        priority: 0,
        body: r#"name = "verifier"
description = "Adversarial check of another harness's answer, no network"
kind = "cli"

# A verifier is a harness like any other. This one is deterministic: it reads
# the brief and the prior work and asserts what must be true. Replace the
# command with a model call, a test runner, or a script of your own — the engine
# does not care which, only that the answer is a check.
command = "sh"
args = ["-c", "cat >/dev/null; echo '{\"checked\": true, \"note\": \"no automated verifier configured\"}'"]
prompt_on_stdin = true

route = "native"
verifier = true
tags = ["verify"]
priority = 0
enabled = false
timeout_s = 300
"#
        .to_string(),
    });

    out
}

/// Write `fleet.toml` pointing at a chooser, or at nothing.
pub fn config_toml(
    chooser: &str,
    provider_base: Option<&str>,
    provider_key_env: Option<&str>,
) -> String {
    let chooser_block = match chooser {
        "jev" => {
            r#"# A chooser turns a task into a model + effort. Jev is the fleet's own.
[chooser]
kind = "cli"
command = "/opt/fleet/scripts/jev_text_route.py"
args = ["--request", "{prompt}", "--json"]
ttl_s = 300
timeout_s = 20
"#
        }
        "http" => {
            r#"[chooser]
kind = "http"
url = "http://127.0.0.1:8792/v1/chat/completions"
auth_header = "Authorization"
api_key_env = "FLEET_CHOOSER_KEY"
ttl_s = 300
"#
        }
        _ => {
            r#"# No chooser: every call uses the defaults below. Add one later by editing
# this file — the engine re-reads it on start, nothing is compiled in.
[chooser]
kind = "static"
"#
        }
    };
    let defaults = match (provider_base, provider_key_env) {
        (Some(b), k) => format!(
            r#"
# Used when the chooser has no opinion, or is not configured at all.
[defaults]
model = "{}"
effort = "medium"
provider_base = "{}"
provider_key_env = "{}"
"#,
            default_model_for(b),
            b,
            k.unwrap_or("OPENAI_API_KEY")
        ),
        _ => String::new(),
    };
    format!("{chooser_block}{defaults}\n# Ceiling on concurrent harness processes in one run.\nmax_parallel = 4\n")
}

/// A sensible model id for a base URL, so the static path is usable immediately.
pub fn default_model_for(base: &str) -> &'static str {
    let b = base.to_ascii_lowercase();
    if b.contains("tokenbroker") {
        "deepseek-v4.1-flash"
    } else if b.contains("openai") {
        "gpt-5.1"
    } else if b.contains("anthropic") {
        "claude-sonnet-4.5"
    } else if b.contains("openrouter") {
        "openai/gpt-5.1"
    } else if b.contains("deepseek") {
        "deepseek-chat"
    } else if b.contains("localhost") || b.contains("127.0.0.1") {
        "local-model"
    } else {
        "default"
    }
}

/// Copy a descriptor into the harness directory and report what happened.
pub fn install_preset(dir: &Path, p: &Preset) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dest = dir.join(format!("{}.toml", p.name));
    std::fs::write(&dest, &p.body).with_context(|| format!("writing {}", dest.display()))?;
    Ok(dest)
}

/// The README a fresh install writes into its own directory, so the fleet home
/// explains itself even with no docs tooling installed.
pub const HOME_README: &str = r#"# This directory IS the fleet.

    harnesses/   one *.toml per harness. Add a file to add one, delete it to
                 remove one. No restart, no registration — the directory is
                 re-read on every call.
    fleet.db     shared memory + every session transcript (SQLite, WAL).
    ledger.jsonl append-only record of every decision and every failure.
    runs/        per-run scratch directories, one per harness per run.
    policy.toml  the ceiling: what the engine may modify on its own behalf.
    fleet.toml   chooser + defaults.

## Add a harness

    fleet harness add ./my-harness.toml
    # or just copy it in — the file is the registration
    cp ./my-harness.toml harnesses/
    # and remove one
    fleet harness rm my-harness

## See what is live

    fleet harness list
    fleet run "your task"
    fleet run "compare these two approaches" --unison
    fleet sessions list
    fleet memory search "what did we decide"
"#;
