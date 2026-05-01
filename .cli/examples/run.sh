#!/usr/bin/env bash
# Examples for: run

# Run main()
lex run app.lex main

# Run with fs read scope
lex run --allow-fs-read /tmp app.lex load "/tmp/x.json"
