# Self-host the lex agent VCS server

This guide walks through running `lex serve` on a single VPS so
agents and humans can publish, type-check, and audit Lex code
against an HTTP/JSON + browser surface. Single-tenant: one
store, one user list, one set of branches. (Multi-tenant SaaS
is out of scope today.)

The bundled stack is **Docker Compose + Caddy**. Caddy fronts
the server on `:443` with auto-renewing Let's Encrypt TLS;
`lex serve` runs as a non-root container with a persistent
volume for the store.

## Prerequisites

- A Linux host with Docker ≥ 24 and Docker Compose v2.
- A domain name whose A/AAAA records point at the host's
  public IP. **Ports 80 and 443 must be reachable from the
  internet** — Caddy uses port 80 for the HTTP-01 ACME challenge
  on first start.
- ~500 MB disk for the Docker image, plus whatever you'll grow
  the store to.

## First deploy

```bash
git clone https://github.com/alpibrusl/lex-lang
cd lex-lang
cp .env.example .env
$EDITOR .env       # set LEX_HOST to your domain
docker compose up -d --build
```

The first `up` builds the Rust release binary (~5 min on a
small VPS; subsequent rebuilds cache via `cargo-chef`). Caddy
fetches the TLS cert on first request to `LEX_HOST`; expect a
2-3 second first-byte delay on the first hit.

Health check:

```bash
curl -sS https://$LEX_HOST/v1/health
# {"ok":true}
```

If you see a TLS or 502 error, check `docker compose logs caddy`
and `docker compose logs lex`.

## Adding users

The server gates triage actions (pin / defer / block / unblock,
see [agent-native VCS docs](./design/trace-vs-vcs.md)) against
`<store>/users.json`. Without it, **any** authenticated request
is accepted as the env-var fallback `LEX_TEA_USER` — fine for
solo-dev mode but **not** for a production deploy.

Drop a `users.json` into the store volume:

```bash
cat <<'JSON' | docker compose exec -T lex tee /data/users.json > /dev/null
{
  "users": [
    {"name": "alice", "role": "human"},
    {"name": "lexbot", "role": "agent"}
  ]
}
JSON
```

After this lands, every triage action requires the request to
either:

- Set the `X-Lex-User: alice` header (programmatic clients), or
- Reach the server through a frontend that adds that header
  after authenticating the user (e.g. an SSO proxy).

The web UI's HTML forms can't set custom headers directly. For
a single-user deploy you can leave `LEX_TEA_USER` set on the
container env to your name — see "Web-form auth" below.

### Web-form auth (single-user dev)

If you're the only person hitting the web surface, set
`LEX_TEA_USER` on the lex container so the forms identify you
as that user:

```yaml
# docker-compose.yml — under the `lex` service
    environment:
      LEX_TEA_USER: alice
```

`alice` must still be in `users.json`. The env var becomes the
fallback when the request has no `X-Lex-User` header.

For multi-user deploys, front the server with an SSO proxy that
adds `X-Lex-User` per session. Caddy's `forward_auth` directive
or any of Authelia / Keycloak / Vouch can do it.

## Publishing Lex code

Install the `lex` CLI on your laptop (the binary isn't on
crates.io; it lives in `lex-cli` which is workspace-local):

```bash
cargo install --git https://github.com/alpibrusl/lex-lang lex-cli
```

…or grab a pre-built binary from the
[GitHub Release](https://github.com/alpibrusl/lex-lang/releases).

Publish a stage to the remote server:

```bash
echo 'fn add(x :: Int, y :: Int) -> Int { x + y }' > add.lex

curl -X POST "https://$LEX_HOST/v1/publish" \
  -H "Content-Type: application/json" \
  -H "X-Lex-User: alice" \
  -d "$(jq -Rn --rawfile s add.lex '{source: $s, activate: true}')"
```

Or via the CLI's existing `lex publish` once you've configured
it for a remote store (the CLI publishes to a local store
today; remote-publish via the `/v1/publish` HTTP path is the
short-term workflow).

Browse the activity feed at `https://$LEX_HOST/`.

## Backups

The lex-store volume holds your entire history. Back it up.

A simple cron-driven tarball:

```bash
# /etc/cron.daily/lex-backup
#!/bin/sh
set -e
ts=$(date -u +%Y%m%d-%H%M%S)
docker run --rm \
  -v lex-lang_lex-store:/data:ro \
  -v /var/backups/lex:/out \
  alpine sh -c "cd /data && tar czf /out/lex-store-$ts.tgz ."
# Rotate: keep last 14 days
find /var/backups/lex -name 'lex-store-*.tgz' -mtime +14 -delete
```

Restore with the inverse:

```bash
docker compose down
docker volume create lex-lang_lex-store
docker run --rm \
  -v lex-lang_lex-store:/data \
  -v /var/backups/lex:/in \
  alpine sh -c "cd /data && tar xzf /in/lex-store-<ts>.tgz"
docker compose up -d
```

## Updating to a new lex-lang version

```bash
cd lex-lang
git fetch && git checkout v0.2.1   # or whatever the new tag is
docker compose build --pull
docker compose up -d
```

The store format is stable across patch releases by design.
Minor releases (0.X → 0.(X+1)) may add new attestation kinds or
operation kinds — the store reader is forward-compatible (new
kinds parse, old binaries skip them with a warning) but back-
compatibility from a new binary onto an older store is the
default expected direction.

## Operational notes

- **Logs**: `docker compose logs -f lex caddy`. Both stream to
  stdout.
- **Restart**: `docker compose restart lex` is safe — the store
  is on a volume and the server is stateless beyond it.
- **Resource use**: the lex server is single-threaded for the
  HTTP loop; one CPU + 256 MB RAM is plenty for tens of agents.
- **Crashes**: `restart: unless-stopped` brings the container
  back automatically.

## What this guide doesn't cover

- **Multi-tenancy**. One store = one tenant. If you want
  separate Lex stores for separate teams, run separate Compose
  stacks on separate subdomains (`lex.team-a.example.com`,
  `lex.team-b.example.com`). True multi-tenant SaaS is a
  larger design problem.
- **Cloud-managed deploys** (Fly.io / Cloud Run / k8s). The
  Dockerfile is portable; recipes for those targets are
  follow-up issues if there's demand.
- **Auth providers**. The server reads a name from
  `X-Lex-User`; how that name gets there is up to the proxy
  in front. SSO integration is a per-deployment choice.
- **High availability**. Single-node deploy. Horizontal scale-
  out across stores is a tier-3 concern (#173).

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| 502 on first request | Caddy started before lex was ready. Check `docker compose ps`; if `lex` is healthy and the 502 persists, restart caddy. |
| TLS error / "certificate not yet valid" | Clock skew on the host, or LEX_HOST DNS not pointing here. `dig +short $LEX_HOST` should match the host's public IP. |
| 403 on triage actions | Either `X-Lex-User` is missing, or the supplied name isn't in `users.json`. |
| `lex` container restart loops | Almost always a permissions issue on `/data` after a host volume migration. The container runs as UID 1000; `chown -R 1000:1000` the volume dir. |
| `agent.call_mcp` fails with "no such file" | The MCP subprocess command isn't on the container's PATH. Either install it in a derived image or call MCP servers running in *other* containers via their own network endpoint (future: TCP transport). |
