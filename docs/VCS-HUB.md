# lex-vcs & the hub

## What this is

Lex has its own native version control, separate from git. `lex-vcs` (the
`lex-store`/`lex-vcs` crates inside lex-lang) is a **typed, content-addressed
op-log**: every change is a semantic edit to one declaration — add a function,
change a type, modify a body — not a line-based diff. Each op is identified by
a hash of its content, and every branch head is guaranteed **always
type-valid**: the store refuses to advance a head to a state that doesn't
type-check. There is no equivalent of a broken commit on `main`.

`lex-hub` is the hosted, multi-tenant server for this: a real client-server
system (not a local-only tool) that a `lex` client talks to over HTTP —
pushing and pulling ops, publishing package releases, running merges. The
public instance is `vcs.lexlang.org`; its own browsable UI is
`console.lexlang.org`.

**Why not just use GitHub?** Two different strengths. GitHub is built for
human review — diffs, PRs, comments. lex-vcs is built for **agent-driven**
development: structural merges (conflicts are computed per-function, not per
line), a positive gate that can require a passing test or an approval before
a branch can advance at all, and replay — you can ask the store to
independently regenerate any past change from its recorded intent and verify
it reproduces the same result. For a project where agents write most of the
code, that verifiability matters more than a review UI. For everyday human
collaboration, GitHub is still the better fit, which is why most Lex
projects use both: GitHub for the repository people read, and the hub as an
agent-native store for verification and package hosting.

**What's genuinely new as of this release (v0.11.71):** the store used to
hold only `src/**/*.lex` — a package's declarations, not the rest of the
repository (README, tests, CI config, license). It now also holds a
content-addressed manifest of every other file, synced alongside your code
and exported faithfully by `lex export-git`. See [Files](#files-non-source-content)
below.

## Installing

```sh
LEX_VERSION=v0.11.71
TARGET=aarch64-apple-darwin   # or x86_64-apple-darwin / *-unknown-linux-gnu
curl -fsSL "https://github.com/alpibrusl/lex-lang/releases/download/${LEX_VERSION}/lex-${LEX_VERSION}-${TARGET}.tar.gz" -o /tmp/lex.tgz
tar -xzf /tmp/lex.tgz -C /tmp
install -m 755 "/tmp/lex-${LEX_VERSION}-${TARGET}/lex" ~/.local/bin/lex
lex --version
```

Verify the download against its published `.sha256` before installing.

## The real workflow

There's no "create repo" button anywhere yet — repository creation is
entirely CLI-driven. This is the actual sequence, start to finish.

### 1. Start a package

```sh
mkdir my-package && cd my-package
lex pkg init
```

Write your code under `src/` (a single `src/lib.lex`, or a multi-file
package). If you depend on another package:

```toml
# lex.toml
[dependencies]
some-lib = { registry = "vcs.lexlang.org/<tenant>", version = "1.0.0" }
```

### 2. Install and lock dependencies

```sh
lex pkg install   # fetches declared dependencies into ~/.lex/packages
lex pkg lock      # pins their resolved versions into lex.lock
```

Run `lex pkg lock` **before** your first publish — a publish that changes
nothing later won't re-sync an updated lock to the remote (a lock only
travels with a push that has new ops to send).

### 3. Publish (writes ops locally)

```sh
lex publish . --intent-prompt "why this change exists"
```

For a directory publish this does two things: it records your source as
semantic ops (functions, types), and — as of v0.11.71 — it also captures
everything else in your working copy (README, tests, `.gitignore`d files
excluded, executable bits preserved) into one content-addressed manifest
snapshot, alongside your code. Pass `--no-files` to skip the second part.

`--intent-prompt` matters: it's what makes `lex recall` and `lex op replay`
useful later — every op should say *why*, not just *what*.

### 4. Push (creates the store on first push)

```sh
lex op push https://vcs.lexlang.org --branch main
```

There is no separate "create repository" step — the first `lex op push` to
a new branch name creates it. Set `LEXHUB_TOKEN` (or pass `--token`) to
authenticate; a token is scoped to one tenant (`evk_<tenant>.<secret>`) and
to a specific named store if the admin who minted it chose to.

### 5. Release a version (makes it show up in the registry)

Cutting an immutable, versioned release — what shows up when someone else
declares your package as a dependency — is a raw HTTP call today; there's no
`lex` subcommand for it yet:

```sh
curl -X POST -H "Authorization: Bearer $LEXHUB_TOKEN" -H "Content-Type: application/json" \
  -d '{"version":"0.1.0","branch":"main"}' \
  https://vcs.lexlang.org/v1/pkg/my-package/release
```

Releases are immutable — you can never overwrite `0.1.0`, only publish
`0.1.1`. New packages are **private by default**. To make a version's source
and archive readable without authentication:

```sh
curl -X PUT -H "Authorization: Bearer $LEXHUB_TOKEN" -H "Content-Type: application/json" \
  -d '{"visibility":"public"}' \
  https://vcs.lexlang.org/v1/pkg/my-package/visibility
```

### 6. Pull

```sh
lex op pull https://vcs.lexlang.org --branch main
```

Fetches new ops (and, as of v0.11.71, their file manifests) into your local
store, verifying every transferred stage and blob against its own hash.

## Files (non-source content)

`src/**/*.lex` is still the only thing the op-log itself understands
semantically — that's what gives replay and the type-check gate their
meaning. Everything else in your working copy (README, LICENSE, tests,
examples, CI config, binaries) is now captured too, as a separate
content-addressed blob store with a manifest snapshot per commit — recorded,
synced, and exported, but never replayed or type-checked.

- `lex files status` — working copy vs. what's committed
- `lex files ls` / `lex files cat <path>` — inspect what's captured
- `lex files commit -m "..."` — a files-only change, no source edit
- `lex export-git <dir>` — render the full history as a real git repo,
  `src/` from the op-log and everything else from the manifest, one commit
  per op

This is genuinely new and still growing: whole-package git-import (turning
an existing GitHub repo into vcs history) and a post-push GitHub mirror are
both still ahead, deliberately in that order — a mirror only makes sense once
fidelity between an exported tree and its real source is proven.

## Issues: typed intents, before you write code

You can log an issue in the store the same way you log code — before
implementation exists at all. It's a `lex` command, not a web form, and it
travels with your next push automatically (issues sync alongside ops and
locks; no separate command).

```sh
lex issue create --title "Add rate limiting to the publish endpoint" --shape free_form
# -> prints the issue's id (a content hash)
lex op push https://vcs.lexlang.org --branch main
```

Five shapes, chosen with `--shape`:

| Shape | Done when... |
|---|---|
| `free_form` | a human later approves a proposed, sharper acceptance |
| `typed_delta` | a declared API signature (`--api name:sig`) exists and type-checks |
| `failing_example` | a given input/output example passes |
| `metric_invariant` | a predicate holds over a time window |
| `evidence` | some external evidence (an attestation, a proof) is attached |

`free_form` is the natural starting point if you don't yet know the exact
acceptance criteria — refine it later:

```sh
lex issue propose <id> --shape typed_delta --api "rate_limit:fn(req)->Result" --rationale "..."
lex issue approve <proposal-id> --by alfonso
```

Check status locally with `lex issue list` / `lex issue show <id>`, or ask
the store to actually evaluate whether an issue's acceptance criteria hold
against a real head — a genuine pass/fail verdict, not a status you set by
hand:

```sh
lex issue verify <id>
```

## The console

`console.lexlang.org` is a read-only registry browser today: package list,
version history, source browsing (rendered in-browser, no server-side
checkout), and — once you've logged in with GitHub as a tenant owner —
Settings (API keys) and Branches. **There is no create/publish button.**
The console's own empty state says it plainly: *the first `lex op push`
creates a store*. Publishing, running code, and reviewing changes through
the console UI are all still ahead.

## Known limits, honestly

- Whole-repo storage (this release) means `src/**/*.lex` is still the only
  *replayable, type-checked* content — everything else is recorded but
  opaque, by design (a README can't be type-checked).
- A pull that hits a genuinely-missing attestation for a stage now skips it
  and finishes rather than discarding the whole transfer — but if the
  failure is something other than "genuinely absent" (a transient network
  error, say), there's currently no way to resync just that one piece once
  the branch head has moved past it; you'd need to re-pull from scratch.
- The hub only resolves **registry** dependencies (pinned via a hosted
  version), never git dependencies — a package that depends on something
  git-only won't type-check against the hub.
