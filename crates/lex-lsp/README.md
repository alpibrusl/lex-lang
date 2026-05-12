# lex-lsp

Language Server Protocol bridge for Lex. Pipes
`lex_types::check_program` errors to editor inline-error surfaces
(VS Code, Cursor, Continue, Zed, JetBrains AI).

## What's shipped (phases 1 + 2a + 3a + 3b + 4 of #304)

**Phase 1** — read-only diagnostics:

- `initialize` / `initialized` / `shutdown` lifecycle.
- `textDocument/didOpen` / `didChange` / `didSave` / `didClose`
  with full-document sync.
- `textDocument/publishDiagnostics` emitting
  `lex_types::PositionedError` errors with:
  - `severity = ERROR`
  - `code = <rule_tag>` (e.g. `type-mismatch`, `unknown-identifier`)
    — the stable identifier from #306 slice 2.
  - `source = "lex"`
  - `data = { rule_tag, rule_explanation, suggested_transform, at_node }`
    — code-action providers in phase 3 read the
    `suggested_transform` from here.

**Phase 2a** — navigation:

- `textDocument/hover` — renders the function signature + declared
  effects + budget at the cursor as Markdown.
- `textDocument/definition` — jumps to the `fn` keyword of the
  declaration in the same file.
- `textDocument/completion` — proposes in-scope fn names and
  import aliases. Stdlib-module-member completion (`io.<TAB>`,
  `list.<TAB>`) is queued for phase 2b.

**Phase 3a** — code-action surface (QuickFix from diagnostics):

- `textDocument/codeAction` returns one `QuickFix` action per
  diagnostic whose `data.suggested_transform` is populated (from
  #306 slice 3). The action's `data` carries the full suggestion
  so a client extension can pipe it to
  `lex repair --apply --transform '<json>'`.

**Phase 3b** — first applying refactor (Inline let):

- `textDocument/codeAction` also returns a `Refactor.Inline` action
  for every fn whose body is a top-level `let` and whose
  declaration falls inside the requesting range. Selecting it
  applies a real `WorkspaceEdit` that replaces the file with the
  canonical re-print after `lex_ast::inline_let`, inline in the
  editor — no CLI round-trip. The other three #280 transforms
  (`RenameLocal`, `ReplaceMatchArm`, `ExtractFunction`) need
  cursor-to-NodeId mapping queued for a follow-up.

**Phase 4** — `RepairHint` surface from the store:

- Launch the LSP with `LEX_STORE=<path>` and every fn in the open
  file whose `stage_id` has an active `RepairHint` attestation
  (#281) surfaces as a QuickFix titled *"Lex: repair hint for
  `<fn>` (<rule_tag>) — <kind_hint>"*. The action's `data`
  carries `failed_op_id`, `stage_id`, structured errors,
  `suggested_transform`, and `attestation_id` so a client
  extension can pipe to `lex repair --apply`. Stores not
  configured (no `LEX_STORE`) degrade silently.

## Build

```bash
cargo build --release -p lex-lsp
# binary at: target/release/lex-lsp
```

## VS Code

Add the following to your workspace's `.vscode/settings.json` and
the language-extension config (e.g. via the
[generic-lsp](https://marketplace.visualstudio.com/items?itemName=alefragnani.generic-language-server)
extension or a custom contribution point):

```jsonc
{
  "languageserver": {
    "lex": {
      "command": "/absolute/path/to/lex-lsp",
      "args": [],
      "filetypes": ["lex"],
      "rootPatterns": ["lex.toml", ".git/"]
    }
  }
}
```

A `.vscode/launch.json` snippet for debugging the LSP itself:

```jsonc
{
  "version": "0.2.0",
  "configurations": [
    {
      "name": "Attach to lex-lsp",
      "type": "lldb",
      "request": "attach",
      "program": "${workspaceFolder}/target/debug/lex-lsp",
      "pid": "${command:pickProcess}"
    }
  ]
}
```

## What's queued (phases 2b / 3c / 4b of #304)

- Phase 2b: cross-file definition jumps, references, stdlib-module-
  member completion.
- Phase 3c: applying refactors for the remaining #280 transforms
  (`RenameLocal`, `ReplaceMatchArm`, `ExtractFunction`). Needs
  cursor-to-NodeId mapping.
- Phase 4b: invoking `lex repair --apply` directly from the LSP
  command handler so the RepairHint action lands a real
  `WorkspaceEdit` inline.
