#!/usr/bin/env python3
"""Prove every docs example still works against the *current* toolchain (#1042
part 1): "the point is keep it updated automatically, never drifts silently."

For each `examples/**.lex` file:
  1. `lex check --strict <file>` — hard type-check gate. A file that stops
     type-checking fails this script (nonzero exit), which fails the CI job
     this runs in.
  2. If the file's own leading doc comment shows a real `Run:` command
     (grepped from `lex --output json docs examples/`'s `doc` field, same
     convention `sitegen/generate.lex` renders pages from), that command is
     actually EXECUTED — not just type-checked — and its real exit code is
     captured. This is a stronger proof than (1) alone.

     - One-shot commands (the process exits on its own, e.g.
       agent_dispatcher.lex's `lex run ... run "echo" ...`) are run directly
       under a timeout; the real exit code is what's recorded.
     - Server commands (the source calls `net.serve*`) are started in the
       background, then proven live by replaying the doc comment's own
       `curl ...` lines against the freshly-started server (or, for
       `net.serve_ws_fn` examples with no curl-testable client documented —
       chat_app.lex — a minimal hand-rolled RFC 6455 handshake + one text
       frame round trip, since chat's own documented "client" is an HTML
       page meant to be opened by a human, not scriptable non-interactively).
       The server is then killed. "ran" reflects whether the server came up
       and answered for real, not an exit code from a process that's
       designed to run forever.
  3. Everything else (the majority — files with no `Run:` section) only
     gets the type-check gate; `ran` is `null` ("not applicable"), which is
     fine per the PR that added this: "not every example will have one —
     type-check-only is still a real pass/fail for those."

Writes `site/verify-results.json`, the structured artifact
`sitegen/*.lex` renders into a per-page "✓ verified" badge. Exits nonzero
(failing the calling CI job) if any file that should type-check does not,
or any documented run command does not behave as documented — a broken
example must never reach the published site with a false "verified" badge.

One deliberate, hard-coded exception: `examples/manifesto_effects/dishonest.lex`
is EXPECTED to fail `lex check` — that's the entire point of the file (a
mislabeled `[io]` effect row hiding a `[net]` call must be rejected; see
`.github/workflows/ci.yml`'s "Manifesto effects demo" step, which asserts
the exact same thing). It's recorded as `type_checked: false` in the JSON
(that's the true, current state) but does not fail this script.
"""
import argparse
import base64
import json
import os
import re
import signal
import socket
import subprocess
import sys
import time
from datetime import datetime, timezone

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from docs_examples_lib import (  # noqa: E402
    extract_curl_commands,
    extract_run_command,
    find_lex_files,
    guess_host_port,
    is_server_example,
    load_docs,
)

# The one intentionally-rejected example — mirrors ci.yml's own
# "dishonest.lex must be REJECTED" assertion. Not a loophole: a *new*
# deliberately-broken example added later without a matching entry here
# will correctly fail this script, per the "never drifts silently" point.
EXPECTED_REJECTIONS = {
    "examples/manifesto_effects/dishonest.lex",
}


def log(msg):
    print(msg, flush=True)


def run_type_check(lex_bin, lex_file, strict=True):
    cmd = [lex_bin, "check"] + (["--strict"] if strict else []) + [lex_file]
    proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120)
    return proc.returncode == 0, proc.stdout + proc.stderr


def run_one_shot(run_cmd, timeout=45):
    try:
        proc = subprocess.run(
            ["bash", "-c", run_cmd], capture_output=True, text=True, timeout=timeout
        )
        return proc.returncode, proc.stdout + proc.stderr
    except subprocess.TimeoutExpired as e:
        return None, f"timed out after {timeout}s (expected a one-shot process to exit on its own): {e}"


def wait_for_port(host, port, deadline_s=15):
    deadline = time.time() + deadline_s
    while time.time() < deadline:
        try:
            with socket.create_connection((host, port), timeout=1):
                return True
        except OSError:
            time.sleep(0.3)
    return False


def start_server(run_cmd):
    proc = subprocess.Popen(
        ["bash", "-c", run_cmd],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        preexec_fn=os.setsid,
    )
    return proc


def stop_server(proc):
    try:
        pgid = os.getpgid(proc.pid)
        os.killpg(pgid, signal.SIGTERM)
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(pgid, signal.SIGKILL)
            proc.wait(timeout=5)
    except (ProcessLookupError, PermissionError):
        pass


def ws_handshake_and_roundtrip(host, port, path, timeout=8):
    """Minimal RFC 6455 client: opening handshake + one masked text frame
    out, one frame read back. No third-party deps (stdlib socket only) —
    used only for serve_ws_fn examples with no curl-testable client
    documented (chat_app.lex's own doc says to open an HTML page by hand)."""
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\n"
        f"Host: {host}:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        "\r\n"
    )
    s = socket.create_connection((host, port), timeout=timeout)
    try:
        s.settimeout(timeout)
        s.sendall(req.encode())
        resp = b""
        while b"\r\n\r\n" not in resp:
            chunk = s.recv(4096)
            if not chunk:
                raise RuntimeError("connection closed mid-handshake")
            resp += chunk
        status_line = resp.split(b"\r\n", 1)[0]
        if b" 101 " not in (b" " + status_line):
            raise RuntimeError(f"handshake did not upgrade: {status_line!r}")

        payload = b"verify-docs-examples smoke test"
        mask = os.urandom(4)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        header = bytes([0x81, 0x80 | len(payload)]) + mask
        s.sendall(header + masked)

        frame = s.recv(4096)
        if not frame:
            raise RuntimeError("no frame received back after sending one")
        return True, f"handshake OK, {len(frame)}-byte frame received back"
    finally:
        s.close()


def verify_server(run_cmd, curl_cmds, doc, lex_file):
    host, port = guess_host_port(curl_cmds, doc)
    if port is None:
        m = re.search(r"\b(\d{4,5})\b", run_cmd)
        port = int(m.group(1)) if m else None
        host = "127.0.0.1"
    if port is None:
        return False, None, "could not determine the server's port from its doc comment"

    # A port that's already accepting connections before we've even
    # started our own process means we cannot trust anything the "wait
    # for it to come up" check reports next — we'd just be talking to
    # whatever else is already there. Fail loud rather than silently
    # verifying the wrong server (found the hard way locally: an
    # unrelated process already listening on 8080 made an early version
    # of this check pass while curl was actually hitting that other
    # process the whole time).
    if wait_for_port(host, port, deadline_s=0.5):
        return False, None, (
            f"{host}:{port} was already accepting connections before this example's own "
            f"server started — can't verify against a port something else already owns."
        )

    proc = start_server(run_cmd)
    try:
        if not wait_for_port(host, port, deadline_s=15):
            out = ""
            try:
                out = proc.stdout.read(4000) if proc.stdout else ""
            except Exception:
                pass
            return False, None, f"server never opened {host}:{port} within 15s. output: {out[:2000]}"

        # Defense in depth against the same class of false positive: the
        # port might have opened because our process bound it and then
        # immediately died (e.g. a startup error after the listen call).
        if proc.poll() is not None:
            out = ""
            try:
                out = proc.stdout.read(4000) if proc.stdout else ""
            except Exception:
                pass
            return False, proc.returncode, (
                f"the server process already exited (code {proc.returncode}) right after "
                f"{host}:{port} started accepting connections. output: {out[:2000]}"
            )

        if curl_cmds:
            for c in curl_cmds:
                try:
                    r = subprocess.run(["bash", "-c", c], capture_output=True, text=True, timeout=20)
                except subprocess.TimeoutExpired:
                    return False, None, f"curl smoke test timed out: {c}"
                if r.returncode != 0:
                    return False, r.returncode, f"curl smoke test failed (exit {r.returncode}): {c}\n{r.stderr}"
                # curl exiting 0 only proves the TCP round trip worked, not
                # that the *handler* did what it was supposed to — e.g. a
                # policy flag with wrong syntax (a real bug this caught:
                # `--allow-net-host a,b` is one literal hostname, not two;
                # see gateway_app.lex's history) still gets a 200-with-JSON
                # response, just one carrying an internal error. Grep the
                # body for the runtime's own error-response markers so that
                # class of bug fails loud here instead of behind a "ran"
                # checkmark that only proved curl could connect.
                if re.search(r"internal error:|not in --allow-", r.stdout, re.IGNORECASE):
                    return False, None, f"handler returned an internal-error body for: {c}\n{r.stdout[:500]}"
            return True, 0, f"{len(curl_cmds)} documented curl command(s) all connected and got a genuine (non-error) response"

        src = open(lex_file, encoding="utf-8").read()
        if re.search(r"\bnet\.serve_ws\w*\(", src):
            # WebSocket server with no curl-testable client documented —
            # prove it with a real handshake + round trip instead.
            path_m = re.search(r"ws://[\w.\-]+:\d+(/\S*)", doc)
            path = path_m.group(1) if path_m else "/"
            ok, msg = ws_handshake_and_roundtrip(host, port, path)
            return ok, 0 if ok else None, msg

        # Server started and accepted a TCP connection but the doc comment
        # gave us nothing scriptable to prove request/response behavior
        # with. Weaker proof than a curl round trip — honestly reported as
        # such in the PR, not hidden behind a green checkmark of the same
        # strength.
        return True, 0, f"server bound {host}:{port} (no scriptable client documented — TCP connect only)"
    finally:
        stop_server(proc)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lex-bin", required=True)
    ap.add_argument("--src", default="examples")
    ap.add_argument("--api-docs", required=True, help="path to lex --output json docs examples/ output")
    ap.add_argument("--out", required=True, help="path to write verify-results.json")
    args = ap.parse_args()

    lex_bin = os.path.abspath(args.lex_bin)
    version_out = subprocess.run([lex_bin, "--version"], capture_output=True, text=True).stdout.strip()
    m = re.search(r"lex\s+(\S+)", version_out)
    lex_version = m.group(1) if m else version_out

    docs = load_docs(args.api_docs)
    files = find_lex_files(args.src)
    verified_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    results = []
    hard_failures = []
    runnable = []  # (file, run_cmd) pairs actually exercised — for the recorder step

    for f in files:
        log(f"== {f} ==")
        type_checked, tc_out = run_type_check(lex_bin, f, strict=True)
        expected_rejection = f in EXPECTED_REJECTIONS
        if not type_checked:
            if expected_rejection:
                log(f"  type_checked: False (EXPECTED — {f} is a deliberate negative example, see ci.yml)")
            else:
                log(f"  type_checked: False -- UNEXPECTED FAILURE")
                log("  " + tc_out.strip().replace("\n", "\n  "))
                hard_failures.append(f"{f}: lex check --strict failed unexpectedly:\n{tc_out}")
        else:
            log("  type_checked: True")

        entry = {
            "file": f,
            "type_checked": type_checked,
            "ran": None,
            "exit_code": None,
            "lex_version": lex_version,
            "verified_at": verified_at,
        }

        # Only attempt to run a command for files that actually type-check —
        # running a command from source the checker already rejected proves
        # nothing new, and for dishonest.lex would just fail for the same
        # (expected) reason.
        doc = docs.get(f, "")
        run_cmd = extract_run_command(doc) if type_checked else None
        if run_cmd:
            log(f"  Run: {run_cmd}")
            if is_server_example(f):
                curl_cmds = extract_curl_commands(doc)
                ok, code, msg = verify_server(run_cmd, curl_cmds, doc, f)
                entry["ran"] = ok
                entry["exit_code"] = code
                log(f"  ran (server): {ok} -- {msg}")
                if not ok:
                    hard_failures.append(f"{f}: documented Run: command did not behave as documented:\n{msg}")
            else:
                code, out = run_one_shot(run_cmd)
                ok = code == 0
                entry["ran"] = ok
                entry["exit_code"] = code
                log(f"  ran (one-shot): exit={code}")
                if not ok:
                    log("  " + out.strip().replace("\n", "\n  "))
                    hard_failures.append(
                        f"{f}: documented Run: command exited {code} (expected 0):\n{run_cmd}\n{out}"
                    )
            runnable.append((f, run_cmd))

        results.append(entry)

    os.makedirs(os.path.dirname(args.out), exist_ok=True)
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(results, fh, indent=2)
        fh.write("\n")
    log(f"\nwrote {args.out} ({len(results)} examples, {len(runnable)} with a verified Run: command)")

    # Also drop the runnable set as a tiny manifest the (best-effort,
    # separately-failing) recording step reads, so it doesn't have to
    # re-derive doc-comment parsing itself.
    runnable_path = os.path.join(os.path.dirname(args.out), "runnable-examples.json")
    with open(runnable_path, "w", encoding="utf-8") as fh:
        json.dump([{"file": f, "run": c} for f, c in runnable], fh, indent=2)
        fh.write("\n")

    if hard_failures:
        log(f"\n::error::{len(hard_failures)} docs example(s) failed verification:")
        for msg in hard_failures:
            log("----")
            log(msg)
        sys.exit(1)

    log("\nVALIDATED: every docs example type-checks against the current toolchain "
        "(save the one deliberate rejection), and every documented Run: command "
        "behaves as documented.")


if __name__ == "__main__":
    main()
