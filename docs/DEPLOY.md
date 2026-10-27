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
