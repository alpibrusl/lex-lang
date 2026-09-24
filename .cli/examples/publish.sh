#!/usr/bin/env bash
# Examples for: publish

# Publish drafts
lex publish app.lex

# Publish + activate
lex publish --activate app.lex

# Publish with a recorded intent
lex publish --intent-prompt 'add triple()' --intent-model ollama/qwen3.8:27b-mlx --intent-session run-1 app.lex

# Publish as the realization of a typed issue
lex publish --intent-prompt 'add triple()' --intent-issue <issue_id> app.lex

# Publish a package's code only, skip capturing its files
lex publish --no-files my-package/
