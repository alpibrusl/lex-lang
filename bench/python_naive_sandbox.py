#!/usr/bin/env python3
"""
Naive Python "sandbox" — the kind of thing a quickly-built agent
tool runner often does:

  - exec the code with a restricted __builtins__
  - blocklist suspicious source-text tokens

This is INTENTIONALLY representative of common DIY attempts (not of
production-grade options like RestrictedPython, gVisor, or WASM).
The benchmark uses it to show what static effect typing buys you
that an exec-with-string-filter does not.

Usage:
  python3 python_naive_sandbox.py [--blocklist tok1,tok2,...]

Reads the candidate code from stdin. Exit codes:

  0  code ran without the sandbox refusing it (whether benign or
     exfiltrating — from a defender's POV this is an "escape")
  2  blocked by the blocklist
  3  raised an exception during exec (treated by the harness as
     "blocked" since the attack didn't complete)

Stdout/stderr are forwarded so the harness can inspect side effects.
"""

import argparse
import builtins
import sys


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--blocklist", default="")
    p.add_argument("--input", default="")
    args = p.parse_args()

    code = sys.stdin.read()
    banned = [tok for tok in args.blocklist.split(",") if tok]

    # 1) String-based filter. Trivial to bypass with concatenation
    # ("ex" + "ec"), getattr, or escape sequences — included to
    # show that source-text filters are a security theatre.
    for tok in banned:
        if tok in code:
            print(f"BLOCKED: blocklist token `{tok}` in source", file=sys.stderr)
            return 2

    # 2) Restricted globals. We strip `eval`/`exec`/`compile`, but
    # __import__ stays in __builtins__ (the common oversight) and
    # `open` stays available (no obvious way to remove it without
    # breaking benign code). Object-graph escapes via
    # `().__class__.__base__.__subclasses__()` work freely here.
    allowed = {k: getattr(builtins, k) for k in dir(builtins) if not k.startswith("_")}
    for k in ("eval", "exec", "compile"):
        allowed.pop(k, None)

    g = {"__builtins__": allowed, "input": args.input}
    try:
        exec(code, g)  # noqa: S102 — the whole point
    except SystemExit:
        raise
    except BaseException as e:
        # An exception during exec means the attack didn't land
        # successfully; from the defender's POV the bad outcome
        # was avoided, but the sandbox didn't actively detect it.
        print(f"ERROR: {type(e).__name__}: {e}", file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
