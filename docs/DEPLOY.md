# Deploy

## One line

```console
$ fleet deploy --to ssh://root@your-host --run --release
```

That builds the release binary, streams it over `ssh` (no `scp` needed), copies
the descriptors, writes a systemd unit when systemd is present, starts it, and
prints the health URL.

`--dry-run` prints the same command without executing anything — that is what the
onboarding shows when a server is optional.

## Targets

| target | what it does |
|---|---|
| `ssh://user@host[:port]` | streams the binary over `ssh`, installs, starts |
| `host` | shorthand for `ssh://host` |
| `tailscale://host` | same, over a tailnet name — the tailnet is the credential |
| `reason://peer` | `reason exec` a tarball onto a Reason Network peer |
| `docker://context` | prints the `docker build` / `docker run` pair for that context |
| `file:./out` | writes a bundle and `install.sh`; no server touched |

```console
$ fleet deploy --to file:./bundle        # then hand ./bundle to anyone
$ sh bundle/install.sh                   # on the server
```

## Plugin targets (`fleet targets`)

A built-in target cannot know the differences between your machines. A plugin
target is a small directory with a `target.toml` and a script; the engine hands it
the built binary, the descriptor bundle, and the request as JSON on stdin, and the
target decides how to move them.

```console
$ fleet targets list                     # what is installed, and where
$ fleet targets add ./deploy-targets     # install a directory of targets
$ fleet targets probe fleetnode://host   # is it reachable, and will this work?
$ fleet deploy --to fleetnode://host --port 8787
```

The verbs are `probe`, `plan`, and `deploy`. `probe` must answer with JSON
`{"ready":true|false,"detail":"…"}` and a matching exit status; `plan` lists the
commands without running them; `deploy` does the work.

### What a target has to get right

Each of these was a real failure, not a hypothetical:

- **The binary must match the peer's platform.** An ELF uploaded to a Windows
  desk fails at exec time — after the deploy reported success — with *"not
  compatible with the version of Windows you are running"*. Detect the peer's
  platform, pick the right artifact (`cargo build --release --target …`), and
  refuse with a named reason when none exists.
- **`scp` expands nothing.** `%TEMP%/fleet.exe` as a destination fails with *"No
  such file or directory"*, which reads like a missing directory. Use a
  **relative** destination: it resolves against the remote home on both
  platforms.
- **Stdin redirection over `ssh` does not work to `cmd.exe`** (*"Input
  redirection is not supported"*). Stream files with `scp`.
- **Stage where you can write, not where you want to install.** `/tmp` is not
  writable by a non-root login on many nodes; the user's home always is. Install
  with elevation afterwards.
- **A running service holds its own binary.** Windows refuses to overwrite or
  delete an open file, but a *rename* is allowed — so stop it and move the old
  file aside. Check the result; otherwise a locked file leaves the old binary in
  place and the deploy reports success.
- **Start it in a way that outlives the connection.** `start /b` over ssh dies
  with the session. On Windows, `Win32_Process.Create` (WMI) is detached and
  survives; a scheduled task does not, and `/ru SYSTEM` changes what
  `%USERPROFILE%` means.
- **Prove it, don't assume it.** End the deploy by running the installed binary
  and hitting `/health`. A deploy that leaves nothing listening is not finished,
  whatever the exit codes said.

## The install script

`install.sh` in this repo is the standalone path, for a host you cannot reach
with `fleet` yet:

```console
$ curl -fsSL https://raw.githubusercontent.com/evergreentechnologywy/fleet/main/install.sh | sh
```

Options: `--prefix DIR` (default `/usr/local`), `--port N` (default `8787`),
`--version TAG`, `--no-service`.

It downloads the release archive for the detected triple, installs the binary,
creates `~/.fleet/harnesses`, and writes `fleetd.service` when systemd is present.
Re-running upgrades in place.

## What lands on the host

```
/usr/local/bin/fleet              the binary
~/.fleet/harnesses/*.toml         every descriptor from the deploying machine
~/.fleet/providers.toml           provider declarations (no keys — env var names)
~/.fleet/fleet.toml               chooser + defaults
/etc/systemd/system/fleetd.service
```

Keys are **not** deployed. The bundle carries env var *names*, and the target host
is expected to have those variables set, exactly like every other node in a fleet.
That is why the descriptors are safe to commit and to publish.

```console
# on the host, before starting
$ cat >> /etc/fleet.env <<'EOF'
TB_API_KEY=...
EOF
```

## Running it

```console
$ fleet serve --host 0.0.0.0 --port 8787
```

Behind a reverse proxy, the interesting routes are `POST /v1/run` and the
`/v1/harnesses` CRUD pair. The API has permissive CORS so a browser app can drive
it directly; put authentication in front of it if it is exposed.

```console
$ systemctl status fleetd
$ journalctl -u fleetd -f
$ curl -s localhost:8787/health | jq
```

## Docker

```console
$ docker build -t fleet-engine .          # Dockerfile is in the bundle
$ docker run -d --name fleet \
    -p 8787:8787 \
    -v fleet-data:/data \
    -e TB_API_KEY \
    fleet-engine
```

`FLEET_HOME=/data` in the image, so the volume holds the memory database, the
ledger, and the harnesses.

## Upgrading

```console
$ fleet deploy --to ssh://root@your-host --run --release
```

Idempotent: the binary is replaced, the unit is rewritten, the service is
restarted. `fleet.db` and `ledger.jsonl` are never touched, so memory and
transcripts survive.

## Checking a deployment

```console
$ curl -s host:8787/health
{"ok":true,"version":"0.1.0","harnesses":4,"enabled":3,"load_errors":0,…}

$ curl -s host:8787/v1/harnesses | jq '.harnesses[] | {name, available}'
```

`available: false` on a CLI harness means its binary is not on the remote host's
`PATH` — the descriptor deployed but the tool did not, which is the one thing a
deploy cannot fix for you.
