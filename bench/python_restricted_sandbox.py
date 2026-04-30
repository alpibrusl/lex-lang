#!/usr/bin/env python3
"""
RestrictedPython baseline — the most-reached-for credible Python
sandbox library (used by Plone, Zope; ~1.4M PyPI downloads/month).

Same protocol as `python_naive_sandbox.py`: reads code from stdin,
exits 0 if the code ran (escape from defender's POV), 2 if rejected
at compile time, 3 if it raised during execution.

Setup mirrors RestrictedPython's documented usage:

  - `compile_restricted` rewrites the AST: bans `import`, `exec`,
    `eval`, function/class definitions with private names, etc.
  - `safe_builtins` ships without `open`, `__import__`, `compile`,
    `exec`, `eval`, `input`, etc.
  - `safer_getattr` is wired as `_getattr_` so all `obj.attr`
    rewrites refuse names starting with `_`. This is what closes
    the `().__class__.__base__.__subclasses__()` escape.
  - `PrintCollector` is hooked so `print(...)` collects to a
    buffer instead of writing to stdout (we forward the buffer
    on success).

This is closer to what a security-aware team would actually deploy
than the naive `exec()` baseline.
"""

import argparse
import sys

from RestrictedPython import compile_restricted, safe_builtins
from RestrictedPython.Guards import safer_getattr
from RestrictedPython.PrintCollector import PrintCollector


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--input", default="")
    args = p.parse_args()

    code = sys.stdin.read()

    try:
        byte_code = compile_restricted(code, "<agent-tool>", "exec")
    except SyntaxError as e:
        print(f"BLOCKED: compile_restricted: {e}", file=sys.stderr)
        return 2

    g = {
        "__builtins__": safe_builtins,
        "_getattr_": safer_getattr,
        "_getitem_": lambda o, k: o[k],
        "_getiter_": iter,
        "_print_": PrintCollector,
        "input": args.input,
    }
    try:
        exec(byte_code, g)
    except SystemExit:
        raise
    except BaseException as e:
        print(f"ERROR: {type(e).__name__}: {e}", file=sys.stderr)
        return 3

    # Forward anything the program collected via `print`.
    printed = g.get("_print", None)
    if printed is not None:
        try:
            sys.stdout.write(printed())
        except Exception:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
