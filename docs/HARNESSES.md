# Writing a harness

A harness is one TOML file in `~/.fleet/harnesses/`. The file **is** the harness:
writing it adds one, deleting it removes one, and neither needs a restart.

The filename must equal `name`, because the filename is the handle you address it
by.

## The minimum

```toml
name = "mything"
kind = "cli"
command = "mything"
args = ["-p", "{prompt}"]
```

That is a complete harness. Everything else has a default.

## Field order matters

TOML has no "return to top level". A key written after a `[table]` header belongs
to that table:

```toml
# ✓ correct: harness fields, then the table
name = "x"
route = "jev"
api_key_env = "X_KEY"

[http]
base_url = "https://api.example.ai/v1"
```

```toml
# ✗ wrong: `route` is now a field of [http], not of the harness
name = "x"
api_key_env = "X_KEY"

[http]
base_url = "https://api.example.ai/v1"
route = "jev"
```

`fleet` accepts `api_key_env` and `api_key` in either place, because that is the
mistake people actually make. Everything else it reports, naming the field and
the table it landed in:

```
harnesses/x.toml: unknown field `route`, expected one of `base_url`, `chat_path`,
`auth_header`, `auth_scheme`, `api_key_env`, `api_key`
```

## Kinds

### `cli` — a process

```toml
kind = "cli"
command = "omp"                       # looked up on PATH unless absolute
args = ["-p", "{prompt}"]             # {prompt} {model} {effort} {cwd}
prompt_on_stdin = false               # true delivers the prompt on stdin instead
model_flag = ["--model", "{model}"]   # appended only when a model was chosen
effort_flag = ["--thinking", "{effort}"]
```

Three flag shapes, all valid:

| written | produced |
|---|---|
| `["-m", "{model}"]` | `-m <id>` |
| `["--model={model}"]` | `--model=<id>` |
| `[]` | nothing — the harness takes no model flag |

### `openai-compat` — an endpoint

```toml
kind = "openai-compat"
api_key_env = "MY_KEY"     # an env var NAME, never the key

[http]
base_url = "https://api.example.ai/v1"
chat_path = "/v1/chat/completions"   # joined without doubling a shared /v1
auth_header = "Authorization"
auth_scheme = "Bearer"
```

### `http` — a local server

Same as `openai-compat`, for a server you started yourself (`llama-server`,
`ollama`, a wrapper). A harness that is not listening is reported as unavailable
rather than failing a run with a confusing error.

## Routing

| `route` | who picks the model | chooser call per |
|---|---|---|
| `jev` (default) | the chooser | run |
| `self-routing` | the harness, via the chooser's OpenAI door | turn (inside the harness) |
| `native` | nobody; the harness's own auth | never |
| `fixed` | the descriptor's `model` | never |

```toml
route = "jev"
models = ["deepseek-v4.1-flash", "gpt-6-luna"]   # the ceiling
effort = "high"                                   # used when the chooser has no opinion
```

`models` is enforced: if the chooser names something outside it, the router clamps
to the first entry and records the reason in the ledger. A harness never receives
a model it declared it cannot serve.

`route = "self-routing"` is the interesting one for a CLI that has its own
per-turn model logic. `fleet` exports `OPENAI_BASE_URL`, `FLEET_CHOOSER_BASE_URL`
and every allowed provider's key into the child, so the harness's own choice goes
through the fleet's chooser:

```toml
route = "self-routing"
```

## Selection

```toml
tags = ["code", "long", "repo"]   # matched against the task's own words
priority = 90                     # higher wins a tie
cost_hint = 0.05                  # informational; used in listings
verifier = true                   # only joins a serial plan, as the last stage
```

The planner's deterministic fallback scores a harness by how many of its tags
appear in the task. `priority` breaks ties.

## Lifecycle

```toml
enabled = true      # a harness you wrote is meant to run, so this defaults on
timeout_s = 3600    # a hung harness is killed, not waited on forever
cwd = "~/work"      # defaults to a per-run scratch directory
env = { MY_FLAG = "1", SECRET = "env:MY_SECRET" }   # `env:NAME` reads the env
deny_providers = ["some-provider"]                   # refuse a provider
plugins = true      # set false to keep this harness out of the plugin path
```

## What a harness receives

Environment, so a harness can discover rather than be configured:

| variable | meaning |
|---|---|
| `FLEET_SESSION` | the conversation id; stable across `--resume` |
| `FLEET_RUN` | this run's id |
| `FLEET_HARNESS` | this harness's own name |
| `FLEET_MODEL` / `FLEET_EFFORT` | what the chooser decided for this call |
| `FLEET_PROVIDER` | which provider was selected |
| `FLEET_MEMORY_DB` | the SQLite file holding shared memory |
| `FLEET_HOME` | the fleet home |
| `FLEET_CHOOSER_BASE_URL` | the chooser's OpenAI-compatible door |
| `FLEET_PROVIDERS` | JSON inventory of the providers this harness may use |
| `OPENAI_BASE_URL` / `OPENAI_API_KEY` | the primary allowed provider |
| `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` | the same, for Anthropic-shaped CLIs |
| `FLEET_PROVIDER_<NAME>_URL` / `_KEY` | a specific provider, by name |

The prompt a harness receives is composed, in order: an optional `--brief`
prefix, `# Task`, `# Your part` (for a pipeline stage), `# Work already done
above you` (serial plans), `# Shared memory`, `# What the fleet learned before`,
and a one-line answer instruction.

## Testing a harness without spending anything

```console
$ fleet harness probe mything     # is it runnable right now?
$ fleet harness show mything      # what it will be invoked as
$ fleet run "hello" -H mything -v # watch the invocation
$ fleet ledger --kind route       # what the chooser answered, and why
```

A harness that answers with a fixed string is a valid harness and the fastest way
to check the path end to end: see `examples/harnesses/echo.toml`.

## Adding a harness on a running node

The file is the registration, so there is nothing to call:

```console
$ fleet harness add ./thing.toml     # copies, validates, reports
$ cp ./thing.toml ~/.fleet/harnesses/ # or just this
$ fleet harness rm thing             # and this to remove it
```

Over HTTP, on a node running `fleet serve`:

```console
$ curl -X POST --data-binary @thing.toml localhost:8787/v1/harnesses
$ curl -X DELETE localhost:8787/v1/harnesses/thing
```

The next call sees the change. A run already in flight keeps the definition it
started with.
