# fleet

**A harness for harnesses.**

One orchestrator that routes a task across many agent CLIs, each choosing its own
harness and model per call based on their strengths.

You give it a task; it picks one harness or several, in unison or in a pipeline.
Every call asks a chooser (Jev, or any alternative, or nothing at all) which
harness and which model — and at what reasoning effort — fits that call.

The point is that you stop choosing between harnesses. omp is good at long
autonomous repo work, Codex is good at a single hard review, a raw provider
endpoint is good at bulk text. `fleet` uses all of them for the task in front of
it, with one shared memory and one transcript.

```console
$ fleet run "compare the two approaches in NOTES.md and pick one" --unison
fleet: run 8f21c0 session s-8f21c0 mode unison · 3 call(s), 3 ok · 11.4s
  omp        deepseek-v4.1-flash   ok (7152ms)
  codex      gpt-6-luna            ok (9031ms)
  tb         gemini-3.8-flash      ok (3721ms)
```

## The one idea

**A harness is a file.**

```console
$ fleet harness list
harness dir: ~/.fleet/harnesses
  omp        cli    jev     on    90  ● on PATH
  codex      cli    jev     on    80  ● on PATH
  tb         http   jev     on    40  ● listening

$ cp my-new-harness.toml ~/.fleet/harnesses/     # it is live. no restart.
$ fleet harness rm codex                          # it is gone. no restart.
```

The harness directory is re-read on every call and cached by file fingerprint.
There is no registration step, no daemon state, and no recompile. A harness that
is mid-run is unaffected by its own deletion, because a run holds the definition
it started with.

## Install

```console
$ cargo install --git https://github.com/evergreentechnologywy/fleet
```

Or one line to a server — the deploy command builds, streams, installs a systemd
unit, and starts it:

```console
$ fleet deploy --to ssh://root@your-host --run --release
```

It also accepts `tailscale://host`, `reason://peer`, `docker://context`, and
`file:./out` (a bundle with no server touched). `--dry-run` prints the command
instead of running it.

## Start

Two things are needed:

1. **a chooser** — Jev, or any alternative, or nothing;
2. **a provider** — any OpenAI-compatible base URL and the env var holding its key.

```console
$ fleet onboard
fleet home: ~/.fleet
  ✓ chooser: jev
  ✓ providers detected from the environment: tokenbroker
  ✓ harnesses written: omp, codex, opencode, verifier
```

`fleet onboard` detects every coding CLI on `PATH` and every provider key in the
environment, then writes real descriptor files you can edit. It is optional: a
harness is a file, so you can skip it entirely and drop one in.

### The chooser

A chooser turns a task into a model and an effort level. Three kinds:

| kind | what it is | when |
|---|---|---|
| `cli` | run a command, read `MODEL=… EFFORT=…` or JSON from stdout | Jev, or your own script |
| `http` | POST the task to a JSON endpoint | a hosted chooser |
| `static` | no chooser; the configured defaults win | from zero, one key |

```console
$ fleet config set-chooser jev      # the fleet's own chooser
$ fleet config set-chooser static   # no chooser at all
```

The output shape is forgiving on purpose, because a chooser is often a model:

```json
{"model":"deepseek-v4.1-flash","effort":"minimal","lane":"liquid","serving_provider":"liquid"}
```

A `MODEL=… EFFORT=…` line works too. Anything unusable falls back to the
harness's own declared default and says so in the ledger — routing is a decision,
not a precondition, so a broken chooser never stops a run.

### Harnesses that route themselves

A harness can be pointed at the chooser's OpenAI-compatible door instead of being
handed a model per run. Then its *own* per-turn model choice is still a fleet
decision:

```toml
route = "self-routing"
```

`fleet` exports `OPENAI_BASE_URL`, `FLEET_CHOOSER_BASE_URL` and the provider keys
into the child process, so the harness discovers its options rather than having
them hardcoded.

## Run

```console
$ fleet run "your task"                     # the planner picks
$ fleet run "your task" -H codex            # force one harness
$ fleet run "compare X and Y" --unison      # several answer the same brief
$ fleet run "do X, then verify it" --serial # each stage sees the last
$ fleet run --resume last                   # continue a conversation
```

```console
$ fleet run "your task" --subagents 3          # fan out: 3 harnesses at once
$ fleet run "your task" --subagents 4 \
      --subagent-harnesses omp,codex,claude    # restrict who serves the fan-out
```

`--subagents N` is the swarm as an **ability, not a service**. N harnesses work
the same brief concurrently **in one process**, reusing the executor, the chooser,
the fallback chain and shared memory — so there is no second thing to install,
no port to open, and no key to hold. It is a unison run with a floor on how many
harnesses answer, which is why it shares the unison merge.

**Delegation is the same ability from inside a run.** A harness may spawn
subagents of its own, bounded by `subagents` and `subagent_depth` in its TOML;
the engine injects that contract into the prompt rather than trusting each CLI to
document it. Depth travels through `FLEET_DEPTH`, because a subagent is a separate
process — without that, `subagent_depth` would be dead configuration and
delegation would recurse without a bound.

| mode | what happens |
|---|---|
| **single** | one harness answers |
| **unison** | several answer the same brief; the strongest answer leads and the rest are appended, ranked, with failures shown |
| **serial** | each harness sees what the previous ones produced, so a verifier can check a worker |

The merge is deterministic and free — no second model call. The longest answer
leads, the rest follow, and any harness that failed is named with its error, so a
unison run never hides a broken harness behind a good one.

Every path — sequential, parallel, and fan-out — runs through one executor that
always tries the `[fallback]` chain, so a harness that fails mid-fan-out is
carried by an alternate instead of taking the run down with it.

## Shared memory

One SQLite database in WAL mode, `synchronous=FULL`, one transaction per write.

* **Instant.** A write returns only after it is durable. A harness that reads a
  moment later sees it. No queue, no flush interval.
* **Last version wins.** Memory is keyed `(scope, key)`. A newer write
  supersedes the older row in place and the superseded value is kept, so nothing
  is lost and `fleet memory history` shows every version.
* **Never lose a conversation.** Every turn is appended keyed by
  `(session, seq)`, with the harness and the model that served it. A turn is
  written *at the moment it arrives*, so a crash mid-run cannot lose it.

```console
$ fleet memory put decision "we ship the boring design" --scope project
$ fleet memory history decision --scope project
$ fleet sessions list
$ fleet sessions show s-8f21c0
$ fleet run --resume last "and why?"
```

A resume replays the transcript and the session's facts into every brief, so no
harness starts blind. This is the same property that makes a resumed session on
the fleet's own desk agents safe to kill and restart.

## Queue, cost, and history

```console
$ fleet queue add "audit these 40 files" --subagents 4   # returns an id at once
$ fleet queue work --follow                              # drain, keep waiting
$ fleet usage                                            # tokens per harness/model
$ fleet sessions search "why did we drop pepwave" --source all
```

`fleet queue` exists because `fleet run` blocks until the answer exists. A burst
of work should not need the submitting shell to stay alive, and a process that
dies mid-run should not lose the queue — so the queue is a SQLite table, and
`work` reaps anything stranded in `running` before it starts. Claiming an item is
one `UPDATE ... RETURNING`, because two workers polling the same queue would
otherwise both run the same row.

`fleet usage` aggregates the `tok=in/out` each call writes to the ledger. A CLI
harness reports no usage, so it is counted as **unmetered** rather than zero: a
cost table where half the calls silently read 0 would be lying.

`fleet sessions search` is one query across every harness's history — the engine's
own turn table, omp sessions, [CC] projects, and Codex rollouts. Without it,
answering "what did we already decide about X" meant remembering *which* harness
said it and grepping that one directory by hand, so in practice it was re-derived
from scratch. Reads are on demand: no index to go stale, no second copy of the
transcripts, and a session written a second ago is already searchable.

## Interactive

```console
$ fleet tui                       # a session you can attach to over ssh
$ fleet tui -r s-abc123           # resume one
```

A line-editing REPL over the engine: every line is a task, and a line starting
with `/` is a command (`/harness`, `/model`, `/unison`, `/serial`, `/sessions`,
`/memory`, `/doctor`, `/help`). Deliberately **not** full-screen — a scrolling
REPL survives an 80-column ssh pipe, a tmux pane, and a terminal that never sends
`SIGWINCH`, and needs no cursor-addressing library.

The REPL holds one session id, so every turn appends to the same conversation and
`-r <id>` picks it up again: never losing track of a conversation is a property of
resuming, not of a long-lived process. Raw mode is used **only** while editing a
line, so a harness child sees a normal terminal and Ctrl-C keeps its usual meaning
during a run.

## Providers

Declared **once**, usable by **every** harness — including one added five minutes
from now.

```console
$ fleet providers add myprovider \
    --base-url https://api.myprovider.ai/v1 \
    --key-env MYPROVIDER_API_KEY
$ fleet providers check
omp: can use [tokenbroker, myprovider]
codex: can use [tokenbroker, myprovider]
```

A key is always *named*, never written, so every file here is safe to commit and
to publish. Two directions of blocking exist, because a fleet policy and a local
preference are different authorities:

```toml
# on the provider: harnesses that must never receive it
[[provider]]
name = "adult-lane"
block = ["omp"]
```

```toml
# on the harness: providers it refuses
deny_providers = ["some-provider"]
```

## Plugins

A plugin lives in the engine's call path, so **one plugin applies to every
harness** — the one you add tomorrow included. A plugin is a directory with
`plugin.toml` and a script; it gets the event on argv and a JSON payload on
stdin, and prints JSON to change what happens.

```console
$ fleet plugins example     # writes a working one
$ fleet plugins list
```

Events: `pre_call`, `post_call`, `pre_run`, `post_run`. What a plugin may change:

```json
{"env":{"K":"V"},
 "args_append":["--extra"],
 "prompt_prefix":"[fleet] ",
 "prompt_suffix":"\nAnswer in JSON.",
 "replace_prompt":"…",
 "skip":false,
 "memory":{"last_call":"ok"}}
```

`skip` vetoes a call, `memory` writes to shared memory after it — so a budget
guard, a redaction pass, a tracer or a notifier is written once and applies
everywhere.

## Peers and coordination

Three transports, probed in order, with the one that worked remembered:

1. **Tailscale SSH** — the tailnet is the credential;
2. **plain SSH** — `BatchMode=yes`, key-based only;
3. **Reason Network** — `reason exec --peer` for a node that is a Reason peer but
   not a Tailscale node.

```console
$ fleet peers discover
  zenbook    zenbook.tail50b6ba.ts.net   via tailscale  [new]
$ fleet peers add desk --host zenbook
$ fleet peers send desk "the build is green"
$ fleet peers run desk omp "run the test suite"
$ fleet peers send --email hermes@… "long-form report attached"
```

Inter-agent email is the asynchronous channel, used when no transport reaches a
peer or when an answer is not needed now. A message is recorded in the ledger, so
"sent" and "arrived" stay distinguishable.

## Self-modification

The engine may change itself — a descriptor, a default, a plugin — but a change
becomes a *mark* only after a deterministic verifier passes over the evidence.
Nothing self-reports success.

```console
$ fleet selfmod init      # writes the ceiling and a verifier; inert by default
$ fleet selfmod status
$ fleet selfmod propose harnesses/omp.toml --reason "raise the timeout"
```

Two contracts, carried from the Octopus shape:

* **ceiling vs grant** — `policy.toml` is a static frame: which paths, which
  verifiers, which outcomes. A request is checked against the frame *before* any
  verifier runs. Fail closed.
* **the ledger is truth** — a failed verdict is a durable row, never a silent
  drop. If it is not in the ledger it did not happen.

Self-modification is **off by default**. `enabled = false` until you say
otherwise.

## Lessons

After every run the engine derives lessons from evidence it already has — a
harness that failed, a routing decision that fell back, a pairing that worked —
and writes them to shared memory immediately. Later runs read them back before
planning, most-observed first.

```console
$ fleet lessons list
  [harness×1] `codex` failed and should be treated as suspect until it succeeds again: …
  [pair×3] the pairing [omp, codex] was used together and produced one merged answer; …
```

No model call, no offline batch job. The loop is the run.

## The ledger

Append-only JSON Lines. Every routing decision, every call, every failure, every
mark, every lesson.

```console
$ fleet ledger --kind call --limit 5
2026-10-27T01:46:34Z route    ok          tb model=deepseek-v4.1-flash effort=minimal lane=liquid
2026-10-27T01:46:37Z call     ok          tb ok model=deepseek-v4.1-flash 2858ms
```

## API and web app

```console
$ fleet serve --host 0.0.0.0 --port 8787
```

Open <http://localhost:8787/app>. The page is embedded in the binary — no build
step, no bundler, no `node_modules` — so a local app and a hosted app are the
same artifact, and it cannot drift from the API because it calls the routes
documented below.

| tab | what it is for |
|---|---|
| **run** | give it a task, pick planner/unison/serial, and see each harness's answer with the model it was served |
| **harnesses** | the live list with availability; install by pasting a descriptor, remove with a button — no restart on either side |
| **sessions** | conversations and their full transcripts |
| **memory** | search and per-scope listing, with the history of any key |
| **engine** | configuration, the self-modification ceiling, lessons, and the ledger |

```
GET    /health
GET    /v1/harnesses              POST /v1/harnesses      DELETE /v1/harnesses/{name}
GET    /v1/harnesses/{name}/probe
POST   /v1/run
GET    /v1/sessions               GET  /v1/sessions/{id}
GET    /v1/memory                 PUT  /v1/memory
GET    /v1/memory/history/{scope}/{key}
GET    /v1/lessons                GET  /v1/ledger
GET    /v1/selfmod                POST /v1/selfmod
GET    /v1/config                 GET  /v1/runs
```

Every mutating route answers only after the write is durable, so a caller that
gets 200 can read the value back immediately. Adding a harness over HTTP is a
`POST` of the TOML; deleting one is a `DELETE` — the same seamlessness as the
CLI, because it is the same registry.

## Everything is a file

```
~/.fleet/
  harnesses/*.toml     one file per harness. the file IS the harness.
  providers.toml       credentials, declared once, shared by all
  peers.toml           peer nodes and pinned transports
  plugins/*/           engine-level hooks
  verifiers/*/         verifier packages for self-modification
  policy.toml          the ceiling
  fleet.toml           chooser + defaults
  fleet.db             shared memory + every transcript (SQLite, WAL)
  ledger.jsonl         append-only truth
  runs/                per-run scratch, one dir per harness
```

## Design notes

* **Boring where it matters.** No framework, no async runtime, no plugin ABI.
  Processes and HTTP, a `Mutex`, and SQLite. The dependency list is deliberately
  short so this builds in seconds on a small VPS.
* **Failures are data.** A bad descriptor is reported against its path and
  skipped; a broken plugin is logged and bypassed; a dead chooser falls back and
  says so. One bad component never takes a run down.
* **Deterministic where it can be.** The planner's fallback is tag overlap, the
  merge is a sort, the ledger is append-only. The model is a decision inside a
  run, not a hidden dependency of the engine.

## License

MIT.
