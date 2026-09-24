#!/usr/bin/env python3
"""Terminal recordings of each runnable docs example (#1042 part 2).

BEST-EFFORT / SOFT-FAIL, by design. The hard correctness gate is
verify-docs-examples.py (type-check + real run); this script's job is
purely a nicer artifact — a self-hosted asciicast of the same commands
that script already proved work. If `asciinema` isn't installed, or one
recording fails for an environment reason (a port race, a slow CI
runner), that's logged as a warning and the recording is skipped — never
a reason to fail the docs build. See the calling workflow step, which
also runs with `continue-on-error` for defense in depth.

Recordings are regenerated fresh on every push — same as site/api-docs.json
and site/verify-results.json — and are never committed to the repo.

Reads `site/runnable-examples.json` (written by verify-docs-examples.py:
the exact `{file, run}` pairs it already proved type-check *and* run
cleanly) so this script never re-derives doc-comment parsing and can
never record a command that verification didn't just validate.

For a one-shot example (agent_dispatcher.lex) the recording is just
`asciinema rec --command "<the real Run: command>"`. For a server example
(chat_app / gateway_app / inbox_app / weather_app) a plain recording of
the raw `lex run ... main` command would just show a server hanging
forever with nothing else happening — so instead this script generates a
tiny wrapper shell script per example (mirroring the narrated style of
`examples/agent_merge/demo.sh` / `examples/manifesto_effects/demo.sh`,
already in this repo) that starts the server, echoes + replays the same
documented `curl` commands verify-docs-examples.py just used to prove it
live, and shuts the server down — and records *that* script's real
execution.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from docs_examples_lib import extract_curl_commands, guess_host_port, is_server_example, load_docs  # noqa: E402


def log(msg):
    print(f"[record-docs-examples] {msg}", flush=True)


def slug_for(file):
    s = file
    if s.startswith("examples/"):
        s = s[len("examples/") :]
    if s.endswith(".lex"):
        s = s[: -len(".lex")]
    return s.replace("/", "-")


def find_asciinema():
    path = shutil.which("asciinema")
    if not path:
        return None
    try:
        out = subprocess.run([path, "--version"], capture_output=True, text=True, timeout=10)
        log(f"using {out.stdout.strip() or out.stderr.strip()}")
        return path
    except Exception as e:  # pragma: no cover
        log(f"WARNING: found asciinema at {path} but it did not run cleanly: {e}")
        return None


WS_PROBE_SCRIPT = '''\
import base64, os, socket, sys
host, port, path = sys.argv[1], int(sys.argv[2]), sys.argv[3]
key = base64.b64encode(os.urandom(16)).decode()
req = (f"GET {path} HTTP/1.1\\r\\nHost: {host}:{port}\\r\\nUpgrade: websocket\\r\\n"
       f"Connection: Upgrade\\r\\nSec-WebSocket-Key: {key}\\r\\nSec-WebSocket-Version: 13\\r\\n\\r\\n")
s = socket.create_connection((host, port), timeout=5)
s.settimeout(5)
s.sendall(req.encode())
resp = b""
while b"\\r\\n\\r\\n" not in resp:
    resp += s.recv(4096)
print(resp.split(b"\\r\\n",1)[0].decode())
payload = b"hello from the recorded demo"
mask = os.urandom(4)
masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
s.sendall(bytes([0x81, 0x80 | len(payload)]) + mask + masked)
frame = s.recv(4096)
# WS text frames: 2-byte header (unmasked, server->client) + payload
plen = frame[1] & 0x7F
print("broadcast frame received back:", frame[2:2+plen].decode(errors="replace"))
s.close()
'''


def build_server_wrapper(path_out, file, run_cmd, curl_cmds, host, port, ws_probe_path=None, ws_path="/"):
    """A small, narrated shell script — same spirit as the repo's existing
    examples/*/demo.sh scripts — that starts the server, shows + replays
    its documented curl commands, then shuts it down. This is what
    actually gets recorded for a server-style example."""
    lines = [
        "#!/usr/bin/env bash",
        "set -uo pipefail",
        "cd \"$(git rev-parse --show-toplevel 2>/dev/null || pwd)\"",
        "BOLD=$'\\033[1m'; CYAN=$'\\033[36m'; GREEN=$'\\033[32m'; RESET=$'\\033[0m'",
        f'echo "${{BOLD}}${{CYAN}}$ {run_cmd}${{RESET}}"',
        f"{run_cmd} &",
        "SERVER_PID=$!",
        f'for i in $(seq 1 30); do (echo > /dev/tcp/{host}/{port}) >/dev/null 2>&1 && break; sleep 0.5; done',
    ]
    for c in curl_cmds:
        lines.append("echo")
        lines.append(f'echo "${{BOLD}}${{CYAN}}$ {c}${{RESET}}"')
        lines.append(c)
    if ws_probe_path:
        probe_cmd = f"python3 {ws_probe_path} {host} {port} {ws_path}"
        lines.append("echo")
        lines.append(f'echo "${{BOLD}}${{CYAN}}$ {probe_cmd}   # minimal RFC 6455 client — no curl-testable client is documented for a WS server${{RESET}}"')
        lines.append(probe_cmd)
    lines += [
        "echo",
        f'echo "${{GREEN}}${{BOLD}}done — shutting the server down.${{RESET}}"',
        "kill $SERVER_PID 2>/dev/null",
        "wait $SERVER_PID 2>/dev/null",
        "exit 0",
    ]
    with open(path_out, "w", encoding="utf-8") as fh:
        fh.write("\n".join(lines) + "\n")
    os.chmod(path_out, 0o755)


def record_one(asciinema, file, run_cmd, doc, out_dir, tmp_dir):
    slug = slug_for(file)
    cast_path = os.path.join(out_dir, f"{slug}.cast")
    curl_cmds = extract_curl_commands(doc)

    if not is_server_example(file):
        record_cmd = run_cmd
    else:
        host, port = guess_host_port(curl_cmds, doc)
        if port is None:
            log(f"SKIP {file}: server example but couldn't determine host:port for the wrapper")
            return False
        ws_probe_path, ws_path = None, "/"
        if not curl_cmds and re.search(r"\bnet\.serve_ws\w*\(", open(file, encoding="utf-8").read()):
            ws_probe_path = os.path.join(tmp_dir, "ws_probe.py")
            with open(ws_probe_path, "w", encoding="utf-8") as fh:
                fh.write(WS_PROBE_SCRIPT)
            path_m = re.search(r"ws://[\w.\-]+:\d+(/\S*)", doc)
            ws_path = path_m.group(1) if path_m else "/"
        wrapper_path = os.path.join(tmp_dir, f"{slug}.sh")
        build_server_wrapper(wrapper_path, file, run_cmd, curl_cmds, host, port, ws_probe_path, ws_path)
        record_cmd = f"bash {wrapper_path}"

    log(f"recording {file} -> {cast_path}")
    try:
        proc = subprocess.run(
            [
                asciinema, "rec",
                "--command", record_cmd,
                "--overwrite",
                "--title", f"lex-lang: {file}",
                cast_path,
            ],
            capture_output=True, text=True, timeout=90,
        )
    except subprocess.TimeoutExpired:
        log(f"WARNING: recording {file} timed out — skipping (soft-fail, see module doc)")
        return False

    if proc.returncode != 0 or not os.path.isfile(cast_path) or os.path.getsize(cast_path) < 32:
        log(f"WARNING: recording {file} did not produce a usable .cast — skipping (soft-fail)")
        if proc.stdout:
            log("  stdout: " + proc.stdout[-1000:])
        if proc.stderr:
            log("  stderr: " + proc.stderr[-1000:])
        return False

    log(f"OK {file}: {os.path.getsize(cast_path)} bytes")
    return True


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runnable", required=True, help="site/runnable-examples.json from verify-docs-examples.py")
    ap.add_argument("--api-docs", required=True)
    ap.add_argument("--out-dir", required=True, help="site/recordings")
    args = ap.parse_args()

    os.makedirs(args.out_dir, exist_ok=True)
    # Kept as a sibling of --out-dir, not inside it, so it never gets
    # mistaken for a published recording (sitegen/verify.lex only ever
    # reads the exact filenames named in manifest.json, but this keeps
    # `site/recordings/` itself clean for anyone poking around the build).
    tmp_dir = os.path.join(os.path.dirname(os.path.abspath(args.out_dir)), ".recording-wrappers")
    os.makedirs(tmp_dir, exist_ok=True)

    asciinema = find_asciinema()
    if not asciinema:
        log("WARNING: asciinema not found on PATH — skipping ALL recordings. "
            "This is a soft-fail per this script's own doc: it does not fail the docs build.")
        # Still write an empty manifest so the generator can render "no
        # recording available" honestly instead of guessing.
        write_manifest(args.out_dir, {})
        return 0

    with open(args.runnable, encoding="utf-8") as fh:
        runnable = json.load(fh)
    docs = load_docs(args.api_docs)

    made = {}
    for entry in runnable:
        file, run_cmd = entry["file"], entry["run"]
        try:
            ok = record_one(asciinema, file, run_cmd, docs.get(file, ""), args.out_dir, tmp_dir)
        except Exception as e:  # deliberately broad — a recording failure is never fatal here
            log(f"WARNING: recording {file} raised {e!r} — skipping (soft-fail)")
            ok = False
        made[file] = slug_for(file) + ".cast" if ok else None

    write_manifest(args.out_dir, made)
    n_ok = sum(1 for v in made.values() if v)
    log(f"{n_ok}/{len(runnable)} recordings produced")
    return 0  # always 0 — see module doc


def write_manifest(out_dir, made):
    path = os.path.join(out_dir, "manifest.json")
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(made, fh, indent=2)
        fh.write("\n")


if __name__ == "__main__":
    sys.exit(main())
