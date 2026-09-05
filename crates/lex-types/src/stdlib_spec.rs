//! Declarative stdlib builtin catalogue (#778).
//!
//! One [`BuiltinDef`] per builtin is the single source for everything
//! the toolchain knows about it: the type-checker's module scope
//! ([`module_record`]), the runtime's purity answer and dispatch table
//! (`lex-runtime` keys its implementations by the same `(module, name)`
//! and a test there asserts the two sets agree), and the generated
//! stdlib reference in `docs/AGENT.md` (`lex docs --stdlib-spec`).
//!
//! Signatures are written in Lex type syntax and parsed by the real
//! parser, so the catalogue cannot drift from what `lex check` accepts.
//! Type variables are single lowercase letters (`a`, `b`); an open
//! effect row is written `[| E]` exactly as in user code, and the same
//! `E` on a closure parameter and on the result ties the two rows
//! together (`list.map`'s closure effects flow to the call).
//!
//! Modules migrate here one at a time; `builtins::module_scope` still
//! holds the hand-written signatures for the rest. Adding a builtin to a
//! migrated module means one entry here plus one implementation in the
//! runtime table, and nothing else.

use crate::env::ty_from_canon;
use crate::types::{EffectSet, Ty};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Which index convention a builtin's integer positions use. Recorded
/// so the stdlib reference states it and a runtime test checks it;
/// it is documentation, not a semantic switch (#778).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexConvention {
    /// No integer positions in the signature.
    None,
    /// Positions and lengths count UTF-8 bytes.
    Byte,
    /// Positions and lengths count Unicode scalar values.
    Codepoint,
}

/// How a builtin is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinKind {
    /// Effect-free; dispatched through the runtime's pure table with
    /// owned arguments.
    Pure,
    /// Effect-free but lowered by the compiler / VM (the list
    /// higher-order functions); the runtime table has no entry.
    VmNative,
    /// Dispatched through the effect handler under the runtime policy.
    Effect,
}

#[derive(Debug, Clone, Copy)]
pub struct BuiltinDef {
    pub module: &'static str,
    pub name: &'static str,
    /// Lex type syntax, e.g. `(Str, Str, Int) -> Option[Int]`.
    pub ty: &'static str,
    pub kind: BuiltinKind,
    pub index: IndexConvention,
    /// One or two sentences for the reference; states edge cases.
    pub doc: &'static str,
    /// Cost as a function of the inputs, when it is not obvious.
    pub complexity: Option<&'static str>,
}

const fn pure(
    module: &'static str,
    name: &'static str,
    ty: &'static str,
    index: IndexConvention,
    doc: &'static str,
    complexity: Option<&'static str>,
) -> BuiltinDef {
    BuiltinDef { module, name, ty, kind: BuiltinKind::Pure, index, doc, complexity }
}

const fn native(
    module: &'static str,
    name: &'static str,
    ty: &'static str,
    doc: &'static str,
    complexity: Option<&'static str>,
) -> BuiltinDef {
    BuiltinDef {
        module,
        name,
        ty,
        kind: BuiltinKind::VmNative,
        index: IndexConvention::None,
        doc,
        complexity,
    }
}

use IndexConvention::{Byte, Codepoint, None as NoIndex};

/// Every declared builtin, in the order the stdlib reference lists them
/// (module order, then declaration order within a module).
pub const BUILTINS: &[BuiltinDef] = &[
    // ── std.str ─────────────────────────────────────────────────────
    pure("str", "is_empty", "(Str) -> Bool", NoIndex,
        "`true` when the string has no bytes.", None),
    pure("str", "to_int", "(Str) -> Option[Int]", NoIndex,
        "Parse a decimal integer (optional leading `-`); `None` on any other input.", None),
    pure("str", "to_float", "(Str) -> Option[Float]", NoIndex,
        "Parse a float literal; `None` on any other input.", None),
    pure("str", "concat", "(Str, Str) -> Str", NoIndex,
        "Concatenate two strings; `a + b` is the same operation.", None),
    pure("str", "len", "(Str) -> Int", Byte,
        "Length in UTF-8 bytes, not characters: `str.len(\"é\")` is 2.", Some("O(1)")),
    pure("str", "char_at", "(Str, Int) -> Str", Byte,
        "The byte at a byte index as a one-character string for ASCII bytes; `\"\"` for a non-ASCII byte or an index out of range. Never fails.",
        Some("O(1)")),
    pure("str", "split", "(Str, Str) -> List[Str]", NoIndex,
        "Split on a separator; an empty separator splits into characters.", None),
    pure("str", "join", "(List[Str], Str) -> Str", NoIndex,
        "Join the elements with a separator; fails if an element is not a `Str`.", None),
    pure("str", "starts_with", "(Str, Str) -> Bool", NoIndex,
        "`true` when the first string begins with the second.", None),
    pure("str", "ends_with", "(Str, Str) -> Bool", NoIndex,
        "`true` when the first string ends with the second.", None),
    pure("str", "contains", "(Str, Str) -> Bool", NoIndex,
        "`true` when the second string occurs anywhere in the first.", None),
    pure("str", "cmp", "(Str, Str) -> Int", NoIndex,
        "Three-way byte-order comparison: `-1`, `0` or `1`. Use the comparison operators for a `Bool` (#440).", None),
    pure("str", "replace", "(Str, Str, Str) -> Str", NoIndex,
        "Replace every non-overlapping occurrence of the second string with the third.", None),
    pure("str", "trim", "(Str) -> Str", NoIndex,
        "Strip leading and trailing Unicode whitespace.", None),
    pure("str", "to_upper", "(Str) -> Str", NoIndex,
        "Unicode uppercase.", None),
    pure("str", "to_lower", "(Str) -> Str", NoIndex,
        "Unicode lowercase.", None),
    pure("str", "strip_prefix", "(Str, Str) -> Option[Str]", NoIndex,
        "The remainder after a prefix, or `None` when the prefix is absent.", None),
    pure("str", "strip_suffix", "(Str, Str) -> Option[Str]", NoIndex,
        "The remainder before a suffix, or `None` when the suffix is absent.", None),
    pure("str", "slice", "(Str, Int, Int) -> Str", Codepoint,
        "Half-open range of codepoint indices `[lo, hi)`; indices clamp to the codepoint count and a reversed range fails (#620).",
        Some("O(distance from the previous slice or find on the same string) (#764)")),
    pure("str", "is_ascii", "(Str) -> Bool", NoIndex,
        "`true` when every byte is below 128; one native pass (#768).", Some("O(len)")),
    pure("str", "find", "(Str, Str, Int) -> Option[Int]", Codepoint,
        "Codepoint index of the first occurrence of the needle at or after `from`; `from` clamps to the string and an empty needle matches at `from` (#764).",
        Some("O(distance scanned)")),
    pure("str", "find_any", "(Str, Str, Int) -> Option[Int]", Codepoint,
        "Codepoint index of the first character at or after `from` that occurs in the set string (#764).",
        Some("O(distance scanned)")),
    // ── std.list ────────────────────────────────────────────────────
    native("list", "map", "(List[a], (a) -> [| E] b) -> [| E] List[b]",
        "Apply the closure to every element; the closure's effects flow to the call.", None),
    native("list", "par_map", "(List[a], (a) -> [| E] b) -> [| E] List[b]",
        "`map` on a worker pool capped by `LEX_PAR_MAX_CONCURRENCY` (#305).", None),
    native("list", "sort_by", "(List[a], (a) -> [| E] b) -> [| E] List[a]",
        "Stable sort by the key the closure derives; `Int`, `Float` and `Str` keys order natively, other shapes keep their input order (#338).",
        Some("O(n log n)")),
    native("list", "filter", "(List[a], (a) -> [| E] Bool) -> [| E] List[a]",
        "Keep the elements the closure accepts.", None),
    native("list", "fold", "(List[a], b, (b, a) -> [| E] b) -> [| E] b",
        "Left fold from the initial accumulator.", None),
    pure("list", "len", "(List[a]) -> Int", NoIndex,
        "Number of elements.", Some("O(1)")),
    pure("list", "is_empty", "(List[a]) -> Bool", NoIndex,
        "`true` when the list has no elements.", Some("O(1)")),
    pure("list", "range", "(Int, Int) -> List[Int]", NoIndex,
        "Integers from `lo` up to but excluding `hi`; empty when `hi <= lo`.", None),
    pure("list", "head", "(List[a]) -> Option[a]", NoIndex,
        "The first element, or `None` for an empty list.", Some("O(1)")),
    pure("list", "tail", "(List[a]) -> List[a]", NoIndex,
        "Every element but the first; empty for an empty list.", Some("O(1) when the list is uniquely owned, otherwise O(n) (#774)")),
    pure("list", "concat", "(List[a], List[a]) -> List[a]", NoIndex,
        "The first list followed by the second.", None),
    pure("list", "reverse", "(List[a]) -> List[a]", NoIndex,
        "Elements in reverse order.", None),
    pure("list", "cons", "(a, List[a]) -> List[a]", NoIndex,
        "Prepend one element (#334).", Some("amortised O(1)")),
    pure("list", "enumerate", "(List[a]) -> List[(Int, a)]", NoIndex,
        "Pair every element with its zero-based index.", None),
];

/// Modules whose signatures come from this catalogue rather than from
/// the hand-written tables in `builtins::module_scope`.
pub fn declared_modules() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for d in BUILTINS {
        if !out.contains(&d.module) {
            out.push(d.module);
        }
    }
    out
}

pub fn is_declared_module(module: &str) -> bool {
    BUILTINS.iter().any(|d| d.module == module)
}

/// The definitions of one module, in declaration order.
pub fn defs_for(module: &str) -> Vec<&'static BuiltinDef> {
    BUILTINS.iter().filter(|d| d.module == module).collect()
}

pub fn lookup(module: &str, name: &str) -> Option<&'static BuiltinDef> {
    BUILTINS.iter().find(|d| d.module == module && d.name == name)
}

/// Effect-row variable ids handed to declared builtins. Kept clear of
/// the type-variable ids (`0..`) so a module scheme never has a type
/// variable and a row variable sharing an id, and unique per builtin
/// so two higher-order functions in one module never share a row.
const EFF_VAR_BASE: u32 = 1000;

/// Parse one signature into a [`Ty`]. `eff_var` is the row-variable
/// id to use for the definition's open row, if it has one.
pub fn parse_signature(def: &BuiltinDef, eff_var: u32) -> Result<Ty, String> {
    let src = format!("type Sig = {}\n", def.ty);
    let program = lex_syntax::parse_source(&src)
        .map_err(|e| format!("{}.{}: cannot parse `{}`: {e:?}", def.module, def.name, def.ty))?;
    let stages = lex_ast::canonicalize_program(&program);
    let te = stages
        .iter()
        .find_map(|s| match s {
            lex_ast::Stage::TypeDecl(td) => Some(&td.definition),
            _ => None,
        })
        .ok_or_else(|| format!("{}.{}: no type in `{}`", def.module, def.name, def.ty))?;
    if !matches!(te, lex_ast::TypeExpr::Function { .. }) {
        return Err(format!("{}.{}: `{}` is not a function type", def.module, def.name, def.ty));
    }
    let mut params: Vec<String> = Vec::new();
    collect_type_vars(te, &mut params);
    let first_row = params.len();
    collect_row_vars(te, &mut params);
    let mut ty = ty_from_canon(te, &params);
    if params.len() > first_row + 1 {
        return Err(format!(
            "{}.{}: `{}` names more than one effect row variable",
            def.module, def.name, def.ty
        ));
    }
    if params.len() == first_row + 1 {
        remap_eff_var(&mut ty, first_row as u32, eff_var);
    }
    Ok(ty)
}

/// Single lowercase letters are type variables; everything else is a
/// named type.
fn is_type_var(name: &str) -> bool {
    let mut cs = name.chars();
    matches!((cs.next(), cs.next()), (Some(c), None) if c.is_ascii_lowercase())
}

fn collect_type_vars(te: &lex_ast::TypeExpr, out: &mut Vec<String>) {
    use lex_ast::TypeExpr as T;
    match te {
        T::Named { name, args } => {
            if args.is_empty() && is_type_var(name) && !out.contains(name) {
                out.push(name.clone());
            }
            for a in args {
                collect_type_vars(a, out);
            }
        }
        T::Function { params, ret, .. } => {
            for p in params {
                collect_type_vars(p, out);
            }
            collect_type_vars(ret, out);
        }
        T::Tuple { items } => {
            for i in items {
                collect_type_vars(i, out);
            }
        }
        T::Record { fields } | T::RecordWithSpreads { fields, .. } => {
            for f in fields {
                collect_type_vars(&f.ty, out);
            }
        }
        T::Union { variants } => {
            for v in variants {
                if let Some(p) = &v.payload {
                    collect_type_vars(p, out);
                }
            }
        }
        T::Refined { base, .. } => collect_type_vars(base, out),
    }
}

fn collect_row_vars(te: &lex_ast::TypeExpr, out: &mut Vec<String>) {
    use lex_ast::TypeExpr as T;
    match te {
        T::Function { params, effect_row_var, ret, .. } => {
            if let Some(v) = effect_row_var {
                if !out.contains(v) {
                    out.push(v.clone());
                }
            }
            for p in params {
                collect_row_vars(p, out);
            }
            collect_row_vars(ret, out);
        }
        T::Named { args, .. } => {
            for a in args {
                collect_row_vars(a, out);
            }
        }
        T::Tuple { items } => {
            for i in items {
                collect_row_vars(i, out);
            }
        }
        T::Record { fields } | T::RecordWithSpreads { fields, .. } => {
            for f in fields {
                collect_row_vars(&f.ty, out);
            }
        }
        T::Union { variants } => {
            for v in variants {
                if let Some(p) = &v.payload {
                    collect_row_vars(p, out);
                }
            }
        }
        T::Refined { base, .. } => collect_row_vars(base, out),
    }
}

fn remap_eff_var(ty: &mut Ty, from: u32, to: u32) {
    match ty {
        Ty::Function { params, effects, ret } => {
            if effects.var == Some(from) {
                *effects = EffectSet { concrete: effects.concrete.clone(), var: Some(to) };
            }
            for p in params {
                remap_eff_var(p, from, to);
            }
            remap_eff_var(ret, from, to);
        }
        Ty::List(inner) => remap_eff_var(inner, from, to),
        Ty::Tuple(items) => {
            for i in items {
                remap_eff_var(i, from, to);
            }
        }
        Ty::Record(fields) => {
            for v in fields.values_mut() {
                remap_eff_var(v, from, to);
            }
        }
        Ty::Con(_, args) => {
            for a in args {
                remap_eff_var(a, from, to);
            }
        }
        Ty::Var(_) | Ty::Prim(_) | Ty::Unit | Ty::Never => {}
    }
}

/// The value-level scope of a declared module: a record of its builtins
/// in declaration order, exactly what `builtins::module_scope` returns
/// for the hand-written modules. Parsed once per process.
///
/// Panics if a signature does not parse; the catalogue is checked by
/// `lex-types`' tests, so this is a build-time invariant, not a
/// runtime condition.
pub fn module_record(module: &str) -> Option<Ty> {
    static CACHE: OnceLock<HashMap<&'static str, Ty>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| {
        let mut out = HashMap::new();
        for m in declared_modules() {
            let mut fields = IndexMap::new();
            for (i, def) in defs_for(m).into_iter().enumerate() {
                let ty = parse_signature(def, EFF_VAR_BASE + i as u32)
                    .unwrap_or_else(|e| panic!("stdlib_spec: {e}"));
                fields.insert(def.name.to_string(), ty);
            }
            out.insert(m, Ty::Record(fields));
        }
        out
    });
    cache.get(module).cloned()
}
