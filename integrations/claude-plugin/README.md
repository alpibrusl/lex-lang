# Lex — Claude Code plugin

A [Claude Code](https://code.claude.com) plugin that teaches Claude to drive
the **Lex** toolchain and, optionally, a hosted **lex-hub** gateway.

It bundles three things:

- **A skill** (`skills/lex/`) — when/how to run the `lex` CLI plus the
  effect-discipline idiom rules and syntax pitfalls. Auto-loads when you're
  working with `.lex` files or `lex.toml`.
- **Slash commands** — one-key wrappers over the agent-code loop:
  - `/lex:lex-check` — type-check and surface structured errors.
  - `/lex:lex-repair` — the repair-not-regenerate loop (`lex repair --apply`).
  - `/lex:lex-publish` — fmt + check + publish to the store.
  - `/lex:lex-run` — check + run a function under a narrow policy.
- **An MCP server** (`lex-hub`) — first-class tools (`lex_check`,
  `lex_publish`, `lex_run`, `lex_patch`, `lex_stage_attestations`,
  `lex_health`) against a remote lex-hub gateway.

## Install

```text
/plugin marketplace add alpibrusl/lex-lang
/plugin install lex@lex
```

That installs the skill and slash commands immediately. The two integration
points below are only needed for the workflows that use them.

### Local `lex` CLI (for the skill + slash commands)

The skill and commands shell out to the `lex` binary. Install a release from
<https://github.com/alpibrusl/lex-lang/releases> (or `cargo build --release`
in a checkout) and make sure `lex` is on your `PATH`:

```sh
lex --version
```

### Remote lex-hub (for the MCP tools)

The `lex-hub` MCP server runs the `lex-hub-mcp` binary, which lives in the
[lex-hub](https://github.com/alpibrusl/lex-hub) repo. Put it on your `PATH`:

```sh
cargo install --git https://github.com/alpibrusl/lex-hub lex-hub-mcp
```

Then set the gateway URL and credentials in your environment (the plugin's
`.mcp.json` passes these through). Provide **either** a pre-minted token
**or** a secret + tenant to mint one:

```sh
export LEXHUB_URL="https://api.example.com"

# Option A — a pre-minted bearer JWT:
export LEXHUB_TOKEN="eyJhbGciOi..."

# Option B — mint a short-lived HS256 token (sub = tenant):
export LEXHUB_JWT_SECRET="your-secret"
export LEXHUB_TENANT="my-tenant"
```

If `LEXHUB_URL` and credentials aren't set, the MCP server won't start — the
skill and slash commands (local CLI) still work without it.

## Layout

```
integrations/claude-plugin/
├── .claude-plugin/plugin.json   # manifest
├── .mcp.json                    # lex-hub MCP server (runs lex-hub-mcp)
├── commands/                    # /lex:lex-check, lex-repair, lex-publish, lex-run
├── skills/lex/SKILL.md          # the lex CLI skill + idiom rules
└── README.md
```

The marketplace manifest is at the repo root: `.claude-plugin/marketplace.json`.

## Why this split

The local `lex` CLI is already agent-discoverable (it implements the ACLI
contract: `lex introspect` / `lex skill` / `lex agent-guidelines`), so the
skill + commands are thin orchestration over a self-describing tool. The
hosted gateway isn't MCP-native — it speaks JWT'd HTTP/JSON — so the
`lex-hub-mcp` bridge (shipped in the lex-hub repo) turns it into first-class
MCP tools.
