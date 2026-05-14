# Canonicalization contract

Closes [#412](https://github.com/alpibrusl/lex-lang/issues/412).

The promise "one canonical AST per meaning" underwrites a lot of lex's
identity story: `SigId`s, the typed-transform VCS in `lex-vcs`, the
content-addressed `body_hash` on closures, and any tooling agents build
on top of those hashes. This doc says **exactly** which edits preserve a
hash and which break it, so callers don't have to guess.

There are two distinct hash boundaries in the codebase:

| Hash | Pre-image | Lives in |
|---|---|---|
| **SigId / Stage hash** | Canonicalized AST (post-desugar) serialised as compact JSON | `crates/lex-ast/` |
| **OpId** | Typed VCS `Operation` struct serialised as compact JSON | `crates/lex-vcs/src/canonical.rs` |
| **`body_hash`** | Compiled bytecode (arity + locals + ops), **not** the AST | `crates/lex-bytecode/src/program.rs` |

The rules differ per layer — `body_hash` is *not* an AST hash, and the
OpId form has its own rule set on top of the AST rules. Sections 1-3
cover the AST layer (the one users edit), §4 covers `body_hash`, §5
covers OpId, §6 covers stability.

---

## 1. Edits that preserve the SigId

Source-level rewrites in this list produce the **same** canonical AST,
so the SigId is unchanged. Rules live in
`crates/lex-ast/src/canonicalize.rs:1-12` (module header) with
implementations cross-referenced below.

- **Whitespace, indentation, line breaks** — stripped by the parser
  before the canonicalizer ever runs (`lex-syntax`).
- **Comments** — stripped by the parser.
- **Redundant parentheses** — stripped at parse-time (`(x + y)`,
  `((f x))`).
- **Record-literal field order.** `{ a: 1, b: 2 }` and `{ b: 2, a: 1 }`
  hash identically — fields are sorted alphabetically
  (`canonicalize.rs:86-92`, `:284-293`).
- **Record-type field order.** Same rule for type expressions
  (`canonicalize.rs:284-293`).
- **Union variant declaration order.** Variants in a `type T = A | B | C`
  declaration sort alphabetically (`canonicalize.rs:115-125`).
- **Effect-list order.** `[fs_read, net]` and `[net, fs_read]` hash the
  same — effects sort alphabetically by name
  (`canonicalize.rs:99-104`, `:328-332`).
- **`if` / `else` → `Match` desugaring.** `if c { a } else { b }` becomes
  `Match(c, [Arm(true, a), Arm(false, b)])`. As a side effect, an `if`
  and a hand-written `match` with the same arm shapes hash equally
  (`canonicalize.rs:267-279`).
- **`?` → `Match` desugaring.** `e?` expands to the two-arm match from
  spec §3.10 (`canonicalize.rs:226-253`).
- **Pipe operator.** `x |> f` becomes `Call(f, [x])`;
  `x |> f(args)` becomes `Call(f, [x, args...])`
  (`canonicalize.rs:207-224`).
- **Float-literal text form.** `1.0`, `1.00`, `1e0` all canonicalise to
  the shortest round-trip form via Ryu (`canonicalize.rs:390-408`).
- **Dead branches with literal scrutinees.** `match true { true => a, false => b }`
  folds to `a` (`canonicalize/dead_branch.rs`).

## 2. Edits that change the SigId

Anything **not** in §1 changes the hash. The most common surprises:

- **Function name.** Renaming `fn foo()` to `fn bar()` rotates the
  SigId (test: `lex-ast/tests/canonical.rs:109-114`).
- **Parameter names.** `fn add(x, y)` and `fn add(a, b)` are different
  functions. There is no alpha-renaming pass — local-variable identity
  is part of the canonical form.
- **Top-level declaration order.** Function/type order in a module is
  preserved as written. The module header at `canonicalize.rs:11-12`
  flags this as a TODO (spec §5.3 rule 3) but it's not implemented; act
  as if it never will be.
- **Operator commutativity.** `x + y` and `y + x` hash differently.
  Lex has no algebraic normalisation pass, and won't — a user-defined
  `+` on a custom type may not commute, so the canonicalizer can't
  assume it does even for built-in `Int`.
- **`let`-chain order.** `let x = a; let y = b; …` and
  `let y = b; let x = a; …` hash differently even when the two RHS
  expressions are independent. Effect ordering may be observable
  (`[fs_write]`, `[net]`, …), so reordering would change semantics in
  general.
- **Variable shadowing.** Rebinding a name preserves the source-level
  binding structure.
- **Eta-expansion / eta-reduction.** No `fn(x) -> f(x)` ↔ `f` rewrite.
- **Constant folding.** `1 + 1` is `BinOp("+", 1, 1)` in the canonical
  AST, not `2`. Constant folding happens at bytecode-emit time (or
  later), after the SigId is fixed.
- **Type annotations.** Adding or removing a redundant `:: Int`
  annotation can change the canonical form (the AST records what the
  user wrote).

If you need a property that lives in §2 but want it in §1, that's a
canonicalizer extension — file an issue against `lex-ast` with the
specific rewrite and its semantics-preservation argument.

## 3. The intentional gap on commutativity and let-reordering

The audit note that prompted this doc called out that `x + y` vs `y + x`
and `let`-reordering are "philosophically thorny" — they are, and the
canonicalizer deliberately punts on both:

- **Commutativity** isn't normalised because `+` on a user-defined
  type isn't guaranteed to commute. The canonicalizer doesn't have
  access to type information (it runs before type-check), so it can't
  conditionally normalise per-type.
- **`let`-chain reordering** isn't normalised because the effect
  signature of each RHS may differ. Two adjacent `let`s could be
  `[fs_write]` and `[net]`; reordering them changes the observable
  effect interleaving. Again, the canonicalizer runs before
  type/effect-check and can't safely decide.

The trade-off is conscious: the canonicalizer normalises only
rewrites that are obviously semantics-preserving at the syntax layer.
Anything that needs type or effect information stays as written.

## 4. `body_hash` on `Value::Closure`

`body_hash` is **not** an AST hash. It lives in
`crates/lex-bytecode/src/program.rs:41-128`
(`Body::compute_body_hash`) and is the first 16 bytes of SHA-256 over:

1. `arity` (u16 LE)
2. `locals_count` (u16 LE)
3. `code.len()` (u64 LE)
4. Each `Op` serialised via `serde_json::to_vec`

Things that are **not** part of the pre-image:

- **Capture types.** Deliberately excluded
  (`program.rs:95-105`). Two closures that capture the same value at
  the same source position but differ in inferred capture type still
  collide. This is intentional — the runtime uses `body_hash` only to
  compare bodies, not capture environments, and including types would
  break `flow.sequential` identity across monomorphisations.
- **Constant-pool indices.** The hash hashes `Op` JSON which contains
  pool indices. Two programs that emit the same closure body but
  number their constant pools differently will hash differently. This
  is acceptable in-process (same compilation) but means `body_hash` is
  **not** stable across recompilations — it's a runtime equality
  helper, not a content address.

Use SigId for content-addressed AST identity. Use `body_hash` only for
in-process closure equality (`Value::PartialEq`, `value.rs:97-99`,
and the `flow.sequential` HOF use case from #222).

## 5. OpId — the VCS layer

`OpId` is a separate hash boundary. The V1 canonical form for OpIds is
documented authoritatively in
`crates/lex-vcs/src/canonical.rs:1-51`. Briefly:

- Compact JSON, no pretty-printing.
- Field order from struct/enum declaration.
- `BTreeSet`/`BTreeMap` for unordered collections.
- `parents` vectors sorted + deduped before hashing.
- Empty `parents` arrays emitted (differs from on-disk JSON).
- Optional fields with `skip_serializing_if = "Option::is_none"`.
- SHA-256, lowercase hex, 64 chars.

Any change to those rules rewrites every existing OpId. The module
header is the source of truth.

## 6. Stability across versions

There is no formal versioning scheme on the canonical form today.
Issue [#244](https://github.com/alpibrusl/lex-lang/issues/244) tracks
that work for the VCS canonical form; until it lands, treat every rule
in §1, §4, and §5 as load-bearing:

- Changing a rule in §1 (the AST layer) rewrites every SigId. Existing
  signature attestations stop matching.
- Changing a rule in §4 (`body_hash`) breaks `Value::PartialEq` on
  closures across processes, which would surprise `flow.sequential`
  callers.
- Changing a rule in §5 (OpId) rewrites every store on disk.

Practical guidance for callers building on these hashes:

- **Pin a lex version** if your tooling keys on SigIds or OpIds across
  runs. Pre-1.0, hash-breaking changes are allowed at any minor.
- **Don't persist `body_hash` to disk.** It's a runtime equality
  helper.
- **OpIds are stable within a major** once #244 lands; until then,
  treat them as stable within a patch version only.
- **New canonicalizer normalisations are non-breaking only in one
  direction.** Adding a rule that maps two previously-distinct ASTs to
  the same hash is a semver-major change (it breaks the
  "this SigId means *that* exact source" invariant). Removing a rule
  is also semver-major (it splits one SigId into two). There is no
  safe additive change to §1.

When in doubt, ask in the PR. The canonicalizer is small and
contributor-readable; the hash-stability constraint is the part that's
load-bearing.
