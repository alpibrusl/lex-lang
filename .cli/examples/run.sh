#!/usr/bin/env bash
# Examples for: run

# Run main()
lex run app.lex main

# Run with fs read scope
lex run --allow-fs-read /tmp app.lex load "/tmp/x.json"

# Let the program set the shell's exit status (std.process.exit)
lex run --allow-effects proc_exit check.lex verify
