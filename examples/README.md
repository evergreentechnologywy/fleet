# Example harnesses and plugins

Copy any of these into `~/.fleet/harnesses/` (or `~/.fleet/plugins/<name>/`) and
it is live on the next call. No restart, no registration.

## Harnesses

| file | what it is |
|---|---|
| `harnesses/omp.toml` | the omp coding agent, model chosen by the chooser per run |
| `harnesses/codex.toml` | Codex CLI in `exec` mode |
| `harnesses/opencode.toml` | opencode, good at bulk edits |
| `harnesses/claude.toml` | Claude Code |
| `harnesses/grok.toml` | Grok CLI, search-heavy |
| `harnesses/gemini.toml` | Gemini CLI, wide context |
| `harnesses/provider-openai-compat.toml` | **no CLI at all** — any OpenAI-compatible endpoint |
| `harnesses/verifier.toml` | a deterministic checker that joins a serial plan |
| `harnesses/local-llama.toml` | a local server, no credential |

The last three are the interesting ones: they show that a "harness" is not
necessarily a CLI. An endpoint is a harness, and so is a script that only checks
another harness's work.

## Plugins

| directory | what it does |
|---|---|
| `plugins/prompt-stamp/` | prepends a marker to every prompt, in every harness |
| `plugins/budget-guard/` | vetoes a call when a running cost total is exceeded |
| `plugins/transcript/` | writes every call to a file for auditing |
| `plugins/memory-note/` | records one fact per call into shared memory |

Each one is a `plugin.toml` plus a script. Because plugins live in the engine's
call path, a plugin written once applies to every harness — including the ones
you add tomorrow.

## Try it without any credential

```console
$ cp examples/harnesses/echo.toml ~/.fleet/harnesses/
$ fleet run "hello" -H echo
```

`echo.toml` answers with a fixed string, which is enough to prove the whole path
— planning, routing, execution, transcript, ledger — with no network and no key.
