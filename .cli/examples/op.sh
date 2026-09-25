#!/usr/bin/env bash
# Examples for: op

# List ops
lex op log

# Replay-verify an op
lex op replay <op_id> --candidate regen.lex

# Garbage-collect
lex op gc --confirm

# Import a git repo's branch tip as one snapshot (#892)
lex op import-git ./repo --head-only --store-branch imported

# Import a repo's full first-parent history, folding commits that do not type-check (re-run to extend)
lex op import-git https://example.com/team/repo.git --on-error fold --examples tip
