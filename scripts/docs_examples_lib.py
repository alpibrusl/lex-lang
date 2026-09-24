"""Shared doc-comment parsing for scripts/verify-docs-examples.py and
scripts/record-docs-examples.py — kept in one place so the two scripts
agree, by construction, on what counts as "a runnable example" and what
its command is. See verify-docs-examples.py's module doc for the full
rationale.
"""
import json
import os
import re

SERVE_CALL_RE = re.compile(
    r"\bnet\.(serve|serve_fn|serve_tls|serve_ws|serve_ws_fn|serve_ws_fn_auth|"
    r"serve_ws_fn_actor|serve_ws_fn_actor_with|serve_routed|serve_with|"
    r"serve_fn_with|serve_routed_with|serve_quic|serve_quic_fn|serve_quic_routed)\s*\("
)


def find_lex_files(src_dir):
    out = []
    for root, _dirs, files in os.walk(src_dir):
        for f in sorted(files):
            if f.endswith(".lex"):
                out.append(os.path.relpath(os.path.join(root, f)))
    return sorted(out)


def load_docs(api_docs_path):
    """file -> doc string, from the `lex --output json docs` artifact."""
    with open(api_docs_path, encoding="utf-8") as fh:
        d = json.load(fh)
    out = {}
    for m in d.get("data", {}).get("modules", []):
        out[m.get("file", "")] = m.get("doc", "") or ""
    return out


def extract_run_command(doc):
    """The `Run:` header's command: consecutive indented lines, joining
    `\\`-continuations, collapsed to one shell command line. `None` if
    there's no `Run:` section or its first line isn't a `lex ` invocation."""
    lines = doc.split("\n")
    for i, line in enumerate(lines):
        if line.strip() != "Run:":
            continue
        cmd_lines = []
        j = i + 1
        while j < len(lines):
            l = lines[j]
            if not l or not l[0].isspace():
                break
            cmd_lines.append(l.strip())
            j += 1
        if not cmd_lines:
            continue
        joined = " ".join(p[:-1].strip() if p.endswith("\\") else p for p in cmd_lines)
        joined = re.sub(r"\s+", " ", joined).strip()
        if joined.startswith("lex "):
            return joined
    return None


def extract_curl_commands(doc):
    """Every `curl ...` invocation shown anywhere in the doc comment,
    joining `\\`-continuations."""
    lines = doc.split("\n")
    cmds = []
    i = 0
    while i < len(lines):
        stripped = lines[i].strip()
        if stripped.startswith("curl "):
            parts = [stripped]
            while parts[-1].endswith("\\") and i + 1 < len(lines):
                i += 1
                parts.append(lines[i].strip())
            joined = " ".join(p[:-1].strip() if p.endswith("\\") else p for p in parts)
            joined = re.sub(r"\s+", " ", joined).strip()
            cmds.append(joined)
        i += 1
    return cmds


def is_server_example(lex_file):
    try:
        src = open(lex_file, encoding="utf-8").read()
    except OSError:
        return False
    return bool(SERVE_CALL_RE.search(src))


def guess_host_port(curl_cmds, doc):
    for c in curl_cmds:
        m = re.search(r"https?://([\w.\-]+):(\d+)", c)
        if m:
            return m.group(1), int(m.group(2))
    m = re.search(r"(?:https?|wss?)://([\w.\-]+):(\d+)", doc)
    if m:
        return m.group(1), int(m.group(2))
    return None, None
