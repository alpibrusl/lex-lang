# Effect-row polymorphism (`[base | E]`)

*Added in 0.10 (Unreleased). Additive — existing code keeps working with no
changes. This guide is for **adopting** the feature: what it is, when to
reach for it, and how to migrate the patterns it replaces.*

---

## TL;DR

A function's effect annotation can now end with an **open-row tail** that
names a type parameter:

```lex
fn run[E](f :: (Int) -> [io | E] Int, x :: Int) -> [io | E] Int {
  f(x)
}
```

`E` reads as *"plus any further effects."* It generalizes at the definition
and binds, per call site, to whatever effects the caller actually supplies.
Pass a `[io]` callback and `run` needs `[io]`; pass a `[io, time]` callback
and `run` needs `[io, time]` — the extra effect is **propagated and
enforced**, never dropped.

This is the same mechanism the stdlib higher-order functions (`list.map`,
`fold`, `filter`, …) have always used internally; now you can write it too.

---

## Do I have to change anything?

**No.** The feature is purely additive:

- Closed rows behave exactly as before — `[io]` still unifies by **equality**
  and rejects a `[io, time]` argument. The effect wall is unchanged.
- The row tail is serialized only when present, so existing signatures and
  their content hashes (`lex hash`) are byte-identical. Nothing in the
  content-addressed store re-hashes.
- Using the new syntax requires `lex >= 0.10`; until you bump your pin,
  nothing in your code can use it, and nothing breaks.

Reach for it only where it removes real friction (below).

---

## When to adopt it

### Pattern 1 — a generic helper that had to fix a concrete effect row

Because closed rows unify by equality, a helper that takes a callback had to
commit to one exact effect row. A caller whose callback needed *one more*
effect didn't fit — so you either duplicated the helper per effect set or
widened the row for everyone (over-declaring, and forcing every caller to
request effects they don't use under `--allow-effects`).

```diff
- # Callback row is fixed: only [io] callbacks fit. A [io, time] callback
- # fails to unify, so `retry` can't be reused for it.
- fn retry(f :: (Int) -> [io] Int, x :: Int, n :: Int) -> [io] Int { ... }
+ # Row-polymorphic: works for any callback whose effects are `io` plus E.
+ fn retry[E](f :: (Int) -> [io | E] Int, x :: Int, n :: Int) -> [io | E] Int { ... }
```

The body is unchanged — only the signature gains `[E]` and the `| E` tails.

### Pattern 2 — a framework that hard-coded effects (e.g. a server)

A generic server whose handler row was a fixed concrete list couldn't serve a
handler that produces a *domain* effect outside that list (`sense` /
`actuate` for a robot, a custom capability effect, …). The workaround was to
hand-roll the server loop in the downstream package so the handler could
honestly declare the extra effect.

With row polymorphism the framework forwards the handler's effects instead of
naming them:

```lex
import "std.net" as net

# E is whatever extra effects the dispatch handler declares — the framework
# doesn't name sense/actuate/etc.; it just passes them through.
fn run_http[E](
  port :: Int,
  dispatch :: (Request) -> [io, net, sql, concurrent, random,
                            fs_read, fs_write, time, crypto, llm, proc | E] Response,
) -> [io, net, sql, concurrent, random,
      fs_read, fs_write, time, crypto, llm, proc | E] Nil {
  let handler := fn (req :: Request) -> [io, net, sql, concurrent, random,
                                         fs_read, fs_write, time, crypto, llm, proc | E] Response {
    dispatch(req)
  }
  net.serve_fn(port, handler)
}
```

A caller passing a dispatch that declares `actuate` makes `run_http` require
`actuate` — and `lex run` still gates it via `--allow-effects actuate`. The
authority boundary is intact; the framework just stopped hard-coding it.

---

## The three positions where `| E` works

```lex
# 1. Parameter — the callback's extra effects.
fn apply[E](f :: (Int) -> [io | E] Int, x :: Int) -> [io | E] Int { f(x) }

# 2. The function's own row — propagate to the caller.
fn step[E](f :: () -> [io | E] Nil) -> [io | E] Nil { f() }

# 3. A closure — its `| E` joins the enclosing function's row variable, so
#    effects flow through the closure (e.g. into net.serve_fn) instead of
#    being dropped at the lambda boundary.
fn wrap[E](f :: (Int) -> [io | E] Int) -> (Int) -> [io | E] Int {
  fn (x :: Int) -> [io | E] Int { f(x) }
}
```

---

## Rules & gotchas

- **`E` must be a declared type parameter.** Put it in the `[...]` list:
  `fn f[E](...)`. An unbound name in a tail position is a `lex check` error.
- **One tail variable per row.** `[io | E]` is "io plus E"; you don't chain
  multiple tails. `[| E]` (empty concrete base, fully open) is allowed.
- **Closures must declare the open row they produce.** If a lambda's body
  calls a row-polymorphic parameter (producing an open row) but the lambda
  declares a *closed* row, `lex check` rejects it rather than silently
  dropping the extra effects — annotate the lambda with the matching `| E`.
- **Don't reach for it on closed-row code.** If a function's effects are
  fixed and known, leave it closed — equality checking is the stronger
  guarantee. Use `| E` only where a caller legitimately contributes effects
  you can't (and shouldn't) name.
- **It's row polymorphism over *effects*, not records.** A nominal record
  field (e.g. a generic `Skill[E].handle`) is **not** covered — that needs
  effect-rows-as-type-arguments and is future work. Model the
  polymorphism on the *function* that produces/consumes the value instead
  (Pattern 2 above).

---

## Adoption checklist

1. Bump your `lex` pin to `>= 0.10`.
2. Find helpers/frameworks that fixed a concrete callback/handler effect row
   purely to satisfy unification (often visible as duplicated handlers, an
   over-wide row, or a hand-rolled loop that exists only to declare an extra
   effect).
3. Add a type parameter `[E]` and replace the fixed tail with `| E` on the
   parameter, the function's own row, and any closure that forwards those
   effects.
4. Run `lex check` — closed-row call sites are unaffected; open-row call
   sites now concretize `E` to the caller's real effects, which `lex run
   --allow-effects` still gates.

See `CHANGELOG.md` (Unreleased → Added) for the summary, and
`crates/lex-types/tests/effect_row_polymorphism.rs` for executable examples
including the wall-preservation cases.
