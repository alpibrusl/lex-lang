#!/usr/bin/env bash
# Examples for: merge

# Open a merge
lex merge start --src feature --dst main

# See remaining work
lex merge status merge_...

# Submit resolutions
lex merge resolve merge_... --file resolutions.json

# Land the merge
lex merge commit merge_...
