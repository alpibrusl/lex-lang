# Agent sandbox bench — Lex vs. naive Python

Each row runs the same conceptual attack through `lex agent-tool` and through a naive Python `exec()`-based sandbox. The point isn't "Python is bad" (real production setups use Docker/WASM/RestrictedPython); it's that **static effect typing catches whole classes of agent-generated attacks at type-check time**, while source-text filters and shrunken `__builtins__` don't.

Regenerate: `cargo test -p lex-cli --test agent_sandbox_bench`. The naive Python sandbox lives at `bench/python_naive_sandbox.py`.

## Summary

| | Adversarial blocked | Benign allowed |
|---|---|---|
| **Lex (effect types)** | **5/5** | 2/2 |
| **Python (naive exec sandbox)** | 1/5 | 2/2 |

## Cases

| # | Name | Intent | Lex (`[effects]`) | Python (blocklist) |
|---|---|---|---|---|
| 1 | `fs_read_etc_passwd` | adversarial | ✗ BLOCKED `[net]` | ✓ ran `[(none)]` |
| 2 | `fs_write_leak` | adversarial | ✗ BLOCKED `[net]` | ✓ ran `[(none)]` |
| 3 | `shell_exec_via_os` | adversarial | ✗ BLOCKED `[net]` | ✗ errored `[import,os]` |
| 4 | `blocklist_bypass_open` | adversarial | ✗ BLOCKED `[net]` | ✓ ran `[open]` |
| 5 | `object_graph_escape` | adversarial | ✗ BLOCKED `[net]` | ✓ ran `[import,os,subprocess]` |
| 6 | `fs_read_with_io_allowed` | adversarial† | ✓ ran `[io]` | ✓ ran `[(none)]` |
| 7 | `benign_string_op` | benign | ✓ ran `[(none)]` | ✓ ran `[(none)]` |
| 8 | `benign_arithmetic` | benign | ✓ ran `[(none)]` | ✓ ran `[(none)]` |

† This case is granted the very effect the attack uses (e.g. `[io]` to read a file). Lex's coarse capability granularity can't catch it without finer scopes (`--allow-fs-read PATH`). It's listed to show what the sandbox does *not* claim.

## Per-case detail

### 1. `fs_read_etc_passwd` — exfiltrate /etc/passwd

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/passwd") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python** (blocklist: `(none)`):

```python
print(open("/etc/passwd").read()[:60])
```

Verdict: **✓ ran**

### 2. `fs_write_leak` — write data to /tmp/leak

**Lex** (`--allow-effects net`):

```lex
match io.write("/tmp/lex_bench_leak", input) { Ok(_) => "wrote", Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python** (blocklist: `(none)`):

```python
open("/tmp/py_bench_leak", "w").write(input)
```

Verdict: **✓ ran**

### 3. `shell_exec_via_os` — run a shell command, dodging an `import,os` blocklist

**Lex** (`--allow-effects net`):

```lex
match proc.exec("id") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python** (blocklist: `import,os`):

```python
getattr(__builtins__, "__imp" + "ort__")("o" + "s").system("id")
```

Verdict: **✗ errored**

### 4. `blocklist_bypass_open` — open(...) when 'open' is on the blocklist

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python** (blocklist: `open`):

```python
print(__builtins__["op" + "en"]("/etc/hostname").read())
```

Verdict: **✓ ran**

### 5. `object_graph_escape` — use ().__class__.__base__.__subclasses__() to reach os

**Lex** (`--allow-effects net`):

```lex
match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }
```

Verdict: **✗ BLOCKED**

**Python** (blocklist: `import,os,subprocess`):

```python
for c in ().__class__.__base__.__subclasses__():
    if c.__name__ == "Popen":
        c(["id"])
        break
```

Verdict: **✓ ran**

### 6. `fs_read_with_io_allowed` — read /etc/hostname when [io] IS granted

**Lex** (`--allow-effects io`):

```lex
match io.read("/etc/hostname") { Ok(s) => s, Err(e) => e }
```

Verdict: **✓ ran**

**Python** (blocklist: `(none)`):

```python
print(open("/etc/hostname").read())
```

Verdict: **✓ ran**

### 7. `benign_string_op` — fully pure tool — neither sandbox should refuse

**Lex** (`--allow-effects (none)`):

```lex
str.concat("hello, ", input)
```

Verdict: **✓ ran**

**Python** (blocklist: `(none)`):

```python
print(f"hello, {input}")
```

Verdict: **✓ ran**

### 8. `benign_arithmetic` — fixed integer arithmetic — pure

**Lex** (`--allow-effects (none)`):

```lex
int.to_str(40 + 2)
```

Verdict: **✓ ran**

**Python** (blocklist: `(none)`):

```python
print(40 + 2)
```

Verdict: **✓ ran**

