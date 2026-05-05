# Design: closures-as-values in record fields (#169)

**Status:** investigated; the gap turns out to be tiny.

## Context

Rubric (formerly OSS Auditor) hit this expressing test suites with
short-circuit semantics:

```lex
type Test = { name :: Str, run :: () -> Result[Unit, Str] }

fn run_until_first_failure(suite :: List[Test]) -> Result[Unit, (Str, Str)] {
  list.fold(suite, Ok(Unit), fn (acc, t) -> Result[Unit, (Str, Str)] {
    match acc {
      Ok(_)  => match t.run() {            # ← need to call the field
        Ok(_)  => Ok(Unit),
        Err(e) => Err((t.name, e)),
      },
      Err(e) => Err(e),                    # short-circuit
    }
  })
}
```

The original issue (#169) framed this as a 1-2 week language change
touching the type checker, bytecode, and VM. **Investigation showed
that's wrong** — the substrate is already in place; the visible
gap is one missing branch in the bytecode compiler's `Var` case.

## What already works

| Layer | Status | Evidence |
|---|---|---|
| Type checker accepts `Ty::Function` as a record field | ✅ | `module_scope` already returns `Ty::Record(fields)` with function-typed fields elsewhere; `FieldAccess` returns the field's declared type as-is. |
| Type-checks `record.field(args)` | ✅ | `check_call`'s callee path resolves `FieldAccess` → field type, then unifies the arg list against the function arrow. |
| Bytecode op for "build a closure value from a fn_id" | ✅ | `Op::MakeClosure { fn_id, capture_count }` already exists for lambdas. |
| Bytecode op for "call a closure value on the stack" | ✅ | `Op::CallClosure { arity, node_id_idx }` already exists. |
| Compile `record.field(args)` to GetField + CallClosure | ✅ | `compile_call`'s `other =>` arm already does `compile_expr(callee); args; CallClosure` — and `FieldAccess` compiles to `GetField`. |
| Lambda stored as record field, then called | ✅ | Captures + dispatch already work. |

Verified by example: `type Test = { name :: Str, run :: () -> Result[Str, Str] }; let t :: Test := { ... }; t.run()` **type-checks today**.

## What's missing

**One case in `compile_expr`.** Today:

```rust
a::CExpr::Var { name } => {
    let i = *self.locals.get(name).unwrap_or_else(|| panic!("unknown local: {name}"));
    self.emit(Op::LoadLocal(i));
}
```

If `name` refers to a **known function** (i.e. it's in `function_names`,
not `locals`), this panics. The user is using the function name as a
*value* — they want a closure, not a call. Today the compiler only
materializes closures from lambda expressions; it doesn't materialize
"the function called `make_pass`" as a closure value.

The fix is one extra arm:

```rust
a::CExpr::Var { name } => {
    if let Some(slot) = self.locals.get(name) {
        self.emit(Op::LoadLocal(*slot));
    } else if let Some(&fn_id) = self.function_names.get(name) {
        // Function name used as a value — wrap as closure with no captures.
        self.emit(Op::MakeClosure { fn_id, capture_count: 0 });
    } else {
        panic!("unknown var: {name}");
    }
}
```

That's it. The runtime already accepts `Value::Closure { fn_id,
captures: vec![] }` and `CallClosure` dispatches it correctly.

## Edge cases

- **Closures that capture locals.** Already work via lambda
  expressions (`fn (x) -> y { ... }` in any position). No change needed.
- **Recursive function-typed fields** (a record holding a closure
  over itself). Out of scope per #169 acceptance criteria. Not
  blocked by this design.
- **Effect rows on stored closures.** The field's declared type
  carries the effect set; calls to the field propagate it. Standard
  effect rule — no special handling.
- **Canonicalization for stage publishing.** Records-with-closures
  are runtime values, not stage data. Stages are FnDecl/TypeDecl/
  Import. So this change doesn't affect the content-addressed
  identity story.
- **Generic record types parameterized over the field's signature.**
  Already supported by the type checker via `Ty::Var`.

## Implementation plan

This PR ships:

1. The 5-line compiler fix above.
2. A typed `unknown_var` error (replacing the generic `panic!` with
   a structured `TypeError` so the user gets a useful message if they
   typo a fn name).
3. Tests:
   - Pass a fn-name-as-value to a record literal; call it via
     `t.field()`; verify the result.
   - Test the rubric scenario: `List[Test]` + `list.fold` + first-
     failure short-circuit.
   - Negative test: typo'd fn name surfaces as `TypeError`, not panic.

Estimated total: **half a day**, not the 1-2 weeks the original
issue scoped. The earlier estimate assumed we'd be designing a new
op family and threading function-pointer types through the type
system; the substrate already does both.

## Out of scope

- Closures-over-self (recursive record-typed-fields). Filed as
  follow-up if it ever blocks anyone.
- Decorators / "wrap this fn in caching" patterns that would need
  closures to also implement traits. Lex doesn't have traits;
  separate problem.

## Decision

Ship the fix in this PR. Issue #169 closes when this lands.
