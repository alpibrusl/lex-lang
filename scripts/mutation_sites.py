#!/usr/bin/env python3
"""Find and apply single-operator mutations in a lex test file.

Companion to scripts/mutation-audit.sh, which explains what the audit is for.
This half exists because deciding *where* a mutation may go is the part that
gets it wrong: a mutation that lands somewhere inert leaves the suite green,
which the audit then reports as "the tests did not notice" — a finding that
looks exactly like a real one and is not.

Every false positive in the audit that produced this script was an inert
mutation. Three rules keep them out:

  strings   `assert_true(x, "a is not b")` — flipping the `==` inside that
            message changes prose, not a decision.
  comments  `# uses contextId (not sessionId)` — same.
  harness   `if failures == 0` in run_all — flipping that inverts the suite's
            own pass/fail, so the file "passes" while printing FAIL lines.

Sites are addressed as `line:col:opindex`. The operator is an INDEX rather than
its own text because `(not ` ends in a space, and passing it through a shell
word-split silently truncates it — apply then fails, no mutation is made, and
an unmutated file passes. That is the same phantom-survivor failure, arriving
by a different route, so `apply` also asserts the edit actually happened.

Usage:
  mutation_sites.py list  <file>          -> one `line:col:opindex` per site
  mutation_sites.py apply <file> <site>   -> rewrite that one site in place
"""

import sys

# (from, to). Order is the wire format — append only, never reorder.
OPS = [("==", "!="), (">=", "<"), ("<=", ">"), ("(not ", "(")]

# A mutation inside the suite's own reporting is a mutation of the harness, not
# of any claim under test.
SKIP_FNS = ("run_all", "report", "results", "suite", "raise_failure", "main")


def inert_columns(line):
    """Columns inside a string literal or a trailing comment."""
    inside, escaped, out = False, False, set()
    for i, ch in enumerate(line):
        if escaped:
            escaped = False
            out.add(i)
            continue
        if ch == "\\" and inside:
            escaped = True
            out.add(i)
            continue
        if ch == "#" and not inside:
            out.update(range(i, len(line)))
            break
        if ch == '"':
            out.add(i)
            inside = not inside
            continue
        if inside:
            out.add(i)
    return out


def scaffolding_lines(lines):
    """Line numbers belonging to the suite's reporting functions."""
    out, current = set(), None
    for n, line in enumerate(lines, 1):
        if line.startswith("fn "):
            name = line[3:].split("(")[0].strip()
            current = name if name.startswith(SKIP_FNS) else None
        if current:
            out.add(n)
    return out


def read(path):
    with open(path, encoding="utf-8", errors="surrogateescape") as fh:
        return fh.readlines()


def sites(path):
    """Every mutable operator that is not inert.

    Deliberately not restricted to lines naming an `assert` helper: several
    packages assert by returning `Err(...)` straight out of a match arm and
    never write the word, and an assert-only finder reported those files as
    having no mutants at all — a coverage gap that reads like a clean result.
    """
    lines = read(path)
    skip = scaffolding_lines(lines)
    found = []
    for n, line in enumerate(lines, 1):
        if n in skip:
            continue
        inert = inert_columns(line)
        for index, (needle, _) in enumerate(OPS):
            start = 0
            while True:
                col = line.find(needle, start)
                if col < 0:
                    break
                if col not in inert:
                    found.append((n, col, index))
                start = col + 1
    return found


def apply(path, site):
    n, col, index = (int(part) for part in site.split(":"))
    needle, replacement = OPS[index]
    lines = read(path)
    line = lines[n - 1]
    if line[col:col + len(needle)] != needle:
        raise SystemExit(
            f"{path}:{n}:{col} holds {line[col:col + len(needle)]!r}, not {needle!r} — "
            "the file changed since `list` ran"
        )
    lines[n - 1] = line[:col] + replacement + line[col + len(needle):]
    with open(path, "w", encoding="utf-8", errors="surrogateescape") as fh:
        fh.writelines(lines)


def main():
    if len(sys.argv) < 3:
        raise SystemExit(__doc__)
    command, path = sys.argv[1], sys.argv[2]
    if command == "list":
        for n, col, index in sites(path):
            print(f"{n}:{col}:{index}")
    elif command == "apply":
        if len(sys.argv) < 4:
            raise SystemExit("apply needs a site: line:col:opindex")
        apply(path, sys.argv[3])
    else:
        raise SystemExit(f"unknown command {command!r} — expected list or apply")


if __name__ == "__main__":
    main()
