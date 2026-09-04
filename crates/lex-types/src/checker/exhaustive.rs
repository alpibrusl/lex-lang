//! Match exhaustiveness (#766)
//!
//! A pattern-matrix usefulness check in the style of Maranget's
//! "Warnings for pattern matching". `missing_patterns` takes the arms as
//! a matrix (one row per arm, one column per value being matched) and
//! either proves every value of the column types is covered, or returns
//! witness rows — concrete value shapes no arm accepts — rendered as
//! Lex pattern text for the `missing` field of the error.
//!
//! The column type decides the constructor signature:
//!   * a union type    → its variants (payload becomes one sub-column);
//!   * `Bool`          → `true` / `false`;
//!   * `Unit`          → `()`;
//!   * a tuple         → one constructor with one sub-column per item;
//!   * a record        → one constructor with one sub-column per field
//!                       (record aliases are unfolded first);
//!   * `Never`         → no values, so any matrix is exhaustive;
//!   * anything else   → opaque (`Int`, `Str`, `List[..]`, functions,
//!                       unresolved type variables): only a wildcard or
//!                       a binder covers it.
//!
//! Rows are owned pattern clones. The matrices are tiny (one row per
//! arm) and specialisation needs to synthesise patterns (a wildcard for
//! an uninspected payload, a tuple for a multi-arg constructor), so
//! borrowing would buy nothing.
//!
//! Invoked from `Checker::check_expr` (see `super`) for every `match`.

use super::*;

/// Upper bound on the witnesses reported for one `match`. The default
/// matrix can multiply out (each missing variant × each witness of the
/// remaining columns); past this many the extra rows say nothing new.
pub(super) const MAX_MISSING_WITNESSES: usize = 8;

/// Alias-unfolding bound in `signature_of`; see there.
pub(super) const MAX_ALIAS_UNFOLDS: usize = 64;

/// One column's constructor signature, as `missing_patterns` sees it.
pub(super) enum Signature {
    /// Named constructors, each with an optional payload column type.
    Ctors(Vec<(String, Option<Ty>)>),
    /// A single product constructor whose sub-columns are `fields`;
    /// `render` turns the witnessed sub-columns back into pattern text.
    Product { fields: Vec<(String, Ty)>, kind: ProductKind },
    /// No inhabitants: vacuously exhaustive.
    Never,
    /// Not inspectable by any pattern except a wildcard or binder.
    Opaque,
}

#[derive(Clone, Copy)]
pub(super) enum ProductKind { Tuple, Record }

pub(super) fn is_wild(p: &a::Pattern) -> bool {
    matches!(p, a::Pattern::PWild | a::Pattern::PVar { .. })
}

/// Render a witnessed constructor application as pattern text.
pub(super) fn render_ctor(name: &str, payload: Option<&str>) -> String {
    match payload {
        None => name.to_string(),
        // A tuple payload already carries its own parentheses
        // (`Pair(_, false)` rather than `Pair((_, false))`).
        Some(p) if p.len() > 2 && p.starts_with('(') && p.ends_with(')') => format!("{name}{p}"),
        Some(p) => format!("{name}({p})"),
    }
}

impl Checker {
    /// Classify a column type into the constructor signature its
    /// patterns are checked against.
    pub(super) fn signature_of(&self, ty: &Ty) -> Signature {
        let mut ty = self.u.resolve(ty);
        // Unfold alias chains iteratively. A well-formed program's
        // chains are short; the bound only stops a cyclic alias from
        // looping, in which case the type is treated as opaque.
        let mut unfolds = 0;
        while let Ty::Con(name, args) = &ty {
            let Some(td) = self.type_env.types.get(name) else {
                return Signature::Opaque;
            };
            if td.params.len() != args.len() {
                return Signature::Opaque;
            }
            let mut subst = IndexMap::new();
            for (i, a) in args.iter().enumerate() {
                subst.insert(i as u32, a.clone());
            }
            match &td.kind {
                TypeDefKind::Union(variants) => {
                    return Signature::Ctors(
                        variants.iter().map(|(v, payload)| {
                            (v.clone(), payload.as_ref().map(|p| subst_vars(p, &subst, &IndexMap::new())))
                        }).collect(),
                    );
                }
                TypeDefKind::Alias(inner) => {
                    unfolds += 1;
                    if unfolds > MAX_ALIAS_UNFOLDS {
                        return Signature::Opaque;
                    }
                    ty = self.u.resolve(&subst_vars(inner, &subst, &IndexMap::new()));
                }
                TypeDefKind::Opaque => return Signature::Opaque,
            }
        }
        match ty {
            Ty::Never => Signature::Never,
            Ty::Prim(Prim::Bool) => Signature::Ctors(vec![
                ("true".into(), None),
                ("false".into(), None),
            ]),
            Ty::Unit => Signature::Ctors(vec![("()".into(), None)]),
            Ty::Tuple(items) => Signature::Product {
                fields: items.into_iter().enumerate().map(|(i, t)| (i.to_string(), t)).collect(),
                kind: ProductKind::Tuple,
            },
            Ty::Record(fs) => Signature::Product {
                fields: fs.into_iter().collect(),
                kind: ProductKind::Record,
            },
            _ => Signature::Opaque,
        }
    }

    /// The constructor a pattern commits to at its head, if any. A
    /// wildcard or binder commits to none: it accepts every
    /// constructor but names none, so it never makes a signature
    /// complete on its own. That distinction is what keeps the
    /// usefulness recursion finite on recursive types: a column is
    /// only ever specialised because some arm wrote a constructor
    /// there, and arms have finite depth.
    pub(super) fn head_ctor(p: &a::Pattern) -> Option<&str> {
        match p {
            a::Pattern::PConstructor { name, .. } => Some(name.as_str()),
            a::Pattern::PLiteral { value: a::CLit::Bool { value } } => Some(if *value { "true" } else { "false" }),
            a::Pattern::PLiteral { value: a::CLit::Unit } => Some("()"),
            a::Pattern::PTuple { items } if items.is_empty() => Some("()"),
            _ => None,
        }
    }

    /// The payload sub-column of a constructor pattern, normalised to
    /// exactly one pattern: a multi-argument constructor (`Pair(a, b)`)
    /// matches a tuple payload, and an argument-less use of a payload
    /// constructor (`Some`) inspects nothing.
    pub(super) fn ctor_payload_pattern(args: &[a::Pattern]) -> a::Pattern {
        match args {
            [] => a::Pattern::PWild,
            [one] => one.clone(),
            many => a::Pattern::PTuple { items: many.to_vec() },
        }
    }

    /// Does `head` pick constructor `name`? Returns the sub-column
    /// patterns to push in its place (empty for a payload-less
    /// constructor, one pattern otherwise).
    pub(super) fn specialize_ctor(head: &a::Pattern, name: &str, has_payload: bool) -> Option<Vec<a::Pattern>> {
        if is_wild(head) {
            return Some(if has_payload { vec![a::Pattern::PWild] } else { vec![] });
        }
        match head {
            a::Pattern::PConstructor { name: n, args } if n == name => Some(
                if has_payload { vec![Self::ctor_payload_pattern(args)] } else { vec![] },
            ),
            a::Pattern::PLiteral { value: a::CLit::Bool { value } } if name == if *value { "true" } else { "false" } => {
                Some(vec![])
            }
            a::Pattern::PLiteral { value: a::CLit::Unit } if name == "()" => Some(vec![]),
            a::Pattern::PTuple { items } if items.is_empty() && name == "()" => Some(vec![]),
            _ => None,
        }
    }

    /// Does `head` accept the product? Returns one sub-pattern per
    /// field, in `fields` order, with `_` for fields it leaves
    /// unconstrained.
    pub(super) fn specialize_product(head: &a::Pattern, fields: &[(String, Ty)], kind: ProductKind) -> Option<Vec<a::Pattern>> {
        if is_wild(head) {
            return Some(fields.iter().map(|_| a::Pattern::PWild).collect());
        }
        match (kind, head) {
            (ProductKind::Tuple, a::Pattern::PTuple { items }) if items.len() == fields.len() => Some(items.clone()),
            (ProductKind::Record, a::Pattern::PRecord { fields: pfs }) => Some(
                fields.iter().map(|(name, _)| {
                    pfs.iter().find(|f| &f.name == name).map(|f| f.pattern.clone()).unwrap_or(a::Pattern::PWild)
                }).collect(),
            ),
            _ => None,
        }
    }

    /// `None` when the rows cover every value of `tys`; otherwise the
    /// witness rows (one rendered pattern per column) that no row
    /// accepts, at most `MAX_MISSING_WITNESSES` of them.
    pub(super) fn missing_patterns(&self, rows: &[Vec<a::Pattern>], tys: &[Ty]) -> Option<Vec<Vec<String>>> {
        let Some((head_ty, rest_tys)) = tys.split_first() else {
            // No columns left: the empty row is covered iff some row
            // survived specialisation this far.
            return if rows.is_empty() { Some(vec![vec![]]) } else { None };
        };
        let sig = self.signature_of(head_ty);
        if matches!(sig, Signature::Never) {
            return None;
        }
        if rows.is_empty() {
            return Some(vec![vec!["_".to_string(); tys.len()]]);
        }
        // Default matrix: the rows whose head accepts anything, with
        // the head column dropped. Used whenever the heads present do
        // not form a complete signature.
        let default_rows = |rows: &[Vec<a::Pattern>]| -> Vec<Vec<a::Pattern>> {
            rows.iter().filter(|r| is_wild(&r[0])).map(|r| r[1..].to_vec()).collect()
        };
        match sig {
            Signature::Never => None,
            Signature::Opaque => {
                let ws = self.missing_patterns(&default_rows(rows), rest_tys)?;
                Some(ws.into_iter().map(|mut w| { w.insert(0, "_".into()); w }).collect())
            }
            Signature::Product { fields, kind } => {
                let ws: Vec<Vec<String>> = if rows.iter().all(|r| is_wild(&r[0])) {
                    // No arm looks inside the product, so splitting
                    // it into fields cannot change the answer, and on
                    // a recursive type it would never bottom out. The
                    // default matrix decides; the fields render as `_`.
                    self.missing_patterns(&default_rows(rows), rest_tys)?.into_iter().map(|w| {
                        let mut row = vec!["_".to_string(); fields.len()];
                        row.extend(w);
                        row
                    }).collect()
                } else {
                    let spec: Vec<Vec<a::Pattern>> = rows.iter().filter_map(|r| {
                        let mut sub = Self::specialize_product(&r[0], &fields, kind)?;
                        sub.extend_from_slice(&r[1..]);
                        Some(sub)
                    }).collect();
                    let mut sub_tys: Vec<Ty> = fields.iter().map(|(_, t)| t.clone()).collect();
                    sub_tys.extend_from_slice(rest_tys);
                    self.missing_patterns(&spec, &sub_tys)?
                };
                Some(ws.into_iter().map(|w| {
                    let (head, rest) = w.split_at(fields.len());
                    let rendered = match kind {
                        ProductKind::Tuple => format!("({})", head.join(", ")),
                        ProductKind::Record => {
                            let shown: Vec<String> = fields.iter().zip(head)
                                .filter(|(_, p)| p.as_str() != "_")
                                .map(|((name, _), p)| format!("{name}: {p}"))
                                .collect();
                            if shown.is_empty() { "{ .. }".to_string() } else { format!("{{ {} }}", shown.join(", ")) }
                        }
                    };
                    let mut out = vec![rendered];
                    out.extend_from_slice(rest);
                    out
                }).collect())
            }
            Signature::Ctors(variants) => {
                let present: Vec<&str> = rows.iter().filter_map(|r| Self::head_ctor(&r[0])).collect();
                let missing: Vec<&(String, Option<Ty>)> = variants.iter().filter(|(name, _)| {
                    !present.iter().any(|p| p == name)
                }).collect();
                if missing.is_empty() {
                    // Complete signature: every variant is named by
                    // some arm. Check each variant's specialised
                    // matrix (wildcard rows join each one with a `_`
                    // payload); the first one with a hole yields the
                    // witnesses.
                    for (name, payload) in &variants {
                        let spec: Vec<Vec<a::Pattern>> = rows.iter().filter_map(|r| {
                            let mut sub = Self::specialize_ctor(&r[0], name, payload.is_some())?;
                            sub.extend_from_slice(&r[1..]);
                            Some(sub)
                        }).collect();
                        let mut sub_tys: Vec<Ty> = payload.iter().cloned().collect();
                        sub_tys.extend_from_slice(rest_tys);
                        let n = sub_tys.len() - rest_tys.len();
                        if let Some(ws) = self.missing_patterns(&spec, &sub_tys) {
                            return Some(ws.into_iter().map(|w| {
                                let (head, rest) = w.split_at(n);
                                let mut out = vec![render_ctor(name, head.first().map(String::as_str))];
                                out.extend_from_slice(rest);
                                out
                            }).collect());
                        }
                    }
                    None
                } else {
                    // Incomplete signature: any value built from a
                    // variant no arm names can only be caught by a
                    // wildcard row, so the default matrix decides.
                    let ws = self.missing_patterns(&default_rows(rows), rest_tys)?;
                    let mut out = Vec::new();
                    for (name, payload) in missing {
                        for w in &ws {
                            let mut row = vec![render_ctor(name, payload.as_ref().map(|_| "_"))];
                            row.extend(w.iter().cloned());
                            out.push(row);
                            if out.len() >= MAX_MISSING_WITNESSES {
                                return Some(out);
                            }
                        }
                    }
                    Some(out)
                }
            }
        }
    }
}
