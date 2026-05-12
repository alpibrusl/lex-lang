# lex-lsp

Language Server Protocol bridge for Lex. Pipes
`lex_types::check_program` errors to editor inline-error surfaces
(VS Code, Cursor, Continue, Zed, JetBrains AI).

## Phase 1 (this slice — closes the first AC block of #304)

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

## What's queued (phases 2–4 of #304)

- Phase 2: hover, definition, completion, references.
- Phase 3: code actions backed by #280's typed transforms.
- Phase 4: surface `RepairHint` attestations as code actions
  (one-click `lex repair --apply`).
