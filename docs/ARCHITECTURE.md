# Architecture

How a task becomes an answer, and why each seam is where it is.

## The shape

```
                       ┌───────────────┐
  task ───────────────▶│   planner     │  one harness? several in unison? a pipeline?
                       └───────┬───────┘
                               │ steps
                       ┌───────▼───────┐
                       │   router      │  per call: which model, which effort
                       │  (the chooser)│  Jev · http · static · self-routing
                       └───────┬───────┘
                               │ model + effort
                       ┌───────▼───────┐
   plugins ───────────▶│   engine      │◀─────────── providers (shared, with blocks)
   (pre_call …)        │  run_job      │
                       └───────┬───────┘
                               │ argv / http body
                       ┌───────▼───────┐
                       │   runner      │  a process, or an endpoint
                       └───────┬───────┘
                               │ answer
                       ┌───────▼───────┐
                       │ merge+remember│  fused answer · transcript · lessons
                       └───────────────┘
                               │
                     memory (SQLite, WAL) · ledger (JSONL)
```

## The seams, and why each one exists

### The registry is the filesystem

`Registry::all()` re-reads the harness directory and caches by
`(path, mtime, len)`. That single decision buys the property the whole design
rests on: **add a harness by writing a file, remove one by deleting it**, with no
registration step, no restart, no daemon state, no recompile.

A run resolves its steps to owned `Harness` values before executing anything, so
deleting a file cannot pull a harness out from under a run already using it.

A malformed descriptor is reported against its path and skipped. One broken file
never takes the engine down, and the error carries the serde detail — because
"parsing failed" is indistinguishable from a dozen different mistakes, and the
most common one (a key written after a `[table]` header, so TOML files it under
that table) is only visible in that detail.

### The router is a decision, not a dependency

`Router::decide(harness, task)` returns a model and an effort. Four routes:

| route | who chooses | when |
|---|---|---|
| `jev` | the chooser | the default |
| `self-routing` | the harness, through the chooser's door | per-turn model choice |
| `native` | nobody; the harness's own auth | no model abstraction |
| `fixed` | the descriptor | cheapest possible path |

A chooser is pluggable (`cli` / `http` / `static`) and its output is read
forgivingly — a JSON object, a `MODEL=… EFFORT=…` line, or JSON embedded in other
output — because a chooser is often a model, and models wrap their answers.

**A chooser failure is never a run failure.** It falls back to the harness's
declared default and records why in the ledger. Routing is a decision inside a
run, not a precondition of it.

The decision is cached for a TTL keyed by `(harness, task)`, so a unison run pays
for one chooser call instead of one per harness.

A harness's `models` list is a ceiling: the router clamps the chooser's answer to
it and says so in the reason. A harness never receives a model it declared it
cannot serve.

### The planner is allowed to be wrong

Three modes, and the deterministic fallback is tag overlap — no model, no
network, always available.

* **single** — a clear winner runs alone;
* **unison** — equal scores mean neither is clearly right, so both run; that is
  the premise of the engine;
* **serial** — a pipeline, and a declared verifier is appended as the last stage.

A plan from a chooser is parsed leniently and a step naming an absent harness is
**dropped, not fatal** — a plan produced on another machine must not fail the run
here.

### Plugins live in the call path

A plugin is invoked with an event name and a JSON payload, and prints JSON. The
events are `pre_call`, `post_call`, `pre_run`, `post_run`.

Because the hook is in the engine, one plugin applies to **every** harness,
including one added five minutes from now. That is the difference between a
plugin system and a per-CLI configuration file.

A broken plugin — non-JSON output, a non-zero exit, a missing binary, a timeout —
is logged and bypassed. A plugin can never veto a call by failing; only by
succeeding with `{"skip": true}`.

### Providers are shared; blocking is explicit

Declared once in `providers.toml`, usable by every harness. The environment a CLI
harness receives exports all three naming conventions the common CLIs read
(`OPENAI_*`, `ANTHROPIC_*`) plus a namespaced pair and an inventory, so a harness
discovers its options instead of having them hardcoded.

Blocking runs in **both** directions, because a fleet policy and a local
preference are different authorities:

* `provider.block = ["omp"]` — the provider refuses a harness;
* `harness.deny_providers = ["x"]` — the harness refuses a provider.

### Memory is durable before it answers

`Memory::put` opens a transaction, writes the superseded value to
`memory_version`, upserts the live row, and commits. WAL plus
`synchronous=FULL` means the write is on disk before the call returns, so a
harness that reads a moment later cannot miss it.

`(scope, key)` is the identity, so **last version wins** and nothing is lost:
`history()` shows every superseded value.

A turn is appended **at the moment it arrives**, not at the end of the run. A
crash mid-run cannot lose a conversation; the next `--resume` replays it.

### The merge is deterministic and free

No second model call. Unison ranks by length then speed then name; serial takes
the **last successful stage**, because that is the one that saw everything before
it. Failures are appended by name with their error, so a unison run never hides a
broken harness behind a good one.

If *everything* failed, the failures are the answer — a run that produced nothing
still explains itself.

### Self-modification fails closed

`policy.toml` is a static frame: which paths, which verifiers. A request is
checked against the frame **before** any verifier runs. Then each verifier reads
one request JSON and prints one verdict JSON; non-JSON output, a non-zero exit, a
missing `passed`, or a timeout all count as failed.

The ledger is truth: a failed verdict is a durable row. Off by default.

### Lessons are harvested, not scheduled

After every run, `lessons::harvest` derives durable facts from evidence the run
already has — a harness that failed, a routing decision that fell back, a pairing
that worked — and writes them to shared memory immediately. Later runs read them
back before planning, most-observed first.

No model call, no cron job. The loop is the run.

## Concurrency

A fan-out runs one thread per harness, bounded by `max_parallel`. The `Router` is
behind an `Arc` because its decision cache is already mutex-guarded — which is
what makes a unison run pay for one chooser call rather than one per harness.

Results are sorted back into **plan order**, not completion order, so a run is
reproducible regardless of which harness finished first.

## Failure policy

| failure | behaviour |
|---|---|
| malformed descriptor | reported against its path, skipped |
| harness binary missing | reported by `harness list`; a run that selects it fails with a named error |
| chooser unavailable | falls back to the declared default, recorded in the ledger |
| chooser names a model outside the ceiling | clamped, with the reason recorded |
| one harness fails in a unison run | the others still answer; the failure is named |
| every harness fails | the errors are the answer |
| broken plugin | logged, bypassed |
| provider has no key | declared but unavailable; `providers check` says so |
| peer unreachable on all transports | the error names all three that were tried |
| self-modification outside the ceiling | refused before any verifier runs |

## Dependencies

`anyhow`, `clap`, `serde`/`serde_json`/`toml`, `rusqlite` (bundled), `tiny_http`,
`ureq`, `sha2`, `hex`, `time`.

No async runtime, no framework, no plugin ABI, no dynamic loading. Processes,
HTTP, a `Mutex`, and SQLite. The list is short on purpose: this has to build in
seconds on a small VPS, and every dependency is a thing that can break at 3am.
