//! Type environment: type-decl info and value-binding scopes.

use crate::types::*;
use indexmap::IndexMap;

#[derive(Debug, Clone)]
pub struct TypeDef {
    pub params: Vec<String>,
    pub kind: TypeDefKind,
}

#[derive(Debug, Clone)]
pub enum TypeDefKind {
    /// A union: variant name → optional payload.
    Union(IndexMap<String, Option<Ty>>),
    /// A record alias: `type Foo = { x :: Int }` etc.
    Alias(Ty),
    /// Built-in opaque (Map, Set, ...).
    Opaque,
}

#[derive(Debug, Clone, Default)]
pub struct TypeEnv {
    /// Type-name → definition.
    pub types: IndexMap<String, TypeDef>,
    /// Constructor name → owning type-name.
    pub ctor_to_type: IndexMap<String, String>,
}

impl TypeEnv {
    pub fn new_with_builtins() -> Self {
        let mut e = TypeEnv::default();
        // Result[T, E] = Ok(T) | Err(E)
        let mut r_variants = IndexMap::new();
        r_variants.insert("Ok".into(), Some(Ty::Var(0))); // T
        r_variants.insert("Err".into(), Some(Ty::Var(1))); // E
        e.types.insert("Result".into(), TypeDef {
            params: vec!["T".into(), "E".into()],
            kind: TypeDefKind::Union(r_variants),
        });
        e.ctor_to_type.insert("Ok".into(), "Result".into());
        e.ctor_to_type.insert("Err".into(), "Result".into());

        // Option[T] = Some(T) | None
        let mut o_variants = IndexMap::new();
        o_variants.insert("Some".into(), Some(Ty::Var(0))); // T
        o_variants.insert("None".into(), None);
        e.types.insert("Option".into(), TypeDef {
            params: vec!["T".into()],
            kind: TypeDefKind::Union(o_variants),
        });
        e.ctor_to_type.insert("Some".into(), "Option".into());
        e.ctor_to_type.insert("None".into(), "Option".into());

        // Nil = Unit (alias)
        e.types.insert("Nil".into(), TypeDef {
            params: vec![],
            kind: TypeDefKind::Alias(Ty::Unit),
        });

        // Map, Set: opaque-ish. We just register the names so they parse as Cons.
        e.types.insert("Map".into(), TypeDef { params: vec!["K".into(), "V".into()], kind: TypeDefKind::Opaque });
        e.types.insert("Set".into(), TypeDef { params: vec!["T".into()], kind: TypeDefKind::Opaque });

        // Matrix = { rows :: Int, cols :: Int, data :: List[Float] }.
        // Used by std.math; runtime values are the F64Array fast lane,
        // not a real record. The alias makes math.* signatures readable
        // (`:: Matrix` instead of an inline record) and lets call sites
        // unify nominally. Field access via `m.rows` would type-check
        // but fail at runtime — use `math.rows / math.cols / math.get`.
        let mut mat_fields = IndexMap::new();
        mat_fields.insert("rows".into(), Ty::int());
        mat_fields.insert("cols".into(), Ty::int());
        mat_fields.insert("data".into(), Ty::List(Box::new(Ty::float())));
        e.types.insert("Matrix".into(), TypeDef {
            params: vec![],
            kind: TypeDefKind::Alias(Ty::Record(mat_fields)),
        });

        e
    }

    pub fn add_user_type(&mut self, name: &str, decl: lex_ast::TypeDecl) -> Result<(), String> {
        match &decl.definition {
            lex_ast::TypeExpr::Union { variants } => {
                let mut vmap = IndexMap::new();
                for v in variants {
                    let payload = v.payload.as_ref().map(|p| ty_from_canon(p, &decl.params));
                    vmap.insert(v.name.clone(), payload);
                    self.ctor_to_type.insert(v.name.clone(), name.to_string());
                }
                self.types.insert(name.to_string(), TypeDef {
                    params: decl.params.clone(),
                    kind: TypeDefKind::Union(vmap),
                });
            }
            other => {
                let ty = ty_from_canon(other, &decl.params);
                self.types.insert(name.to_string(), TypeDef {
                    params: decl.params.clone(),
                    kind: TypeDefKind::Alias(ty),
                });
            }
        }
        Ok(())
    }
}

/// Convert canonical TypeExpr to internal Ty, treating type params as
/// fresh-numbered Vars (0..n in declaration order). When instantiating, we
/// substitute these out.
pub fn ty_from_canon(t: &lex_ast::TypeExpr, params: &[String]) -> Ty {
    match t {
        lex_ast::TypeExpr::Named { name, args } => {
            // type param?
            if let Some(idx) = params.iter().position(|p| p == name) {
                if !args.is_empty() {
                    // Type params don't take args.
                    return Ty::Con(name.clone(), args.iter().map(|a| ty_from_canon(a, params)).collect());
                }
                return Ty::Var(idx as u32);
            }
            // Primitives.
            match name.as_str() {
                "Int" => return Ty::int(),
                "Float" => return Ty::float(),
                "Bool" => return Ty::bool(),
                "Str" => return Ty::str(),
                "Bytes" => return Ty::bytes(),
                "Unit" | "Nil" => return Ty::Unit,
                "Never" => return Ty::Never,
                "List" if args.len() == 1 => return Ty::List(Box::new(ty_from_canon(&args[0], params))),
                _ => {}
            }
            Ty::Con(name.clone(), args.iter().map(|a| ty_from_canon(a, params)).collect())
        }
        lex_ast::TypeExpr::Record { fields } => {
            let mut m = IndexMap::new();
            for f in fields { m.insert(f.name.clone(), ty_from_canon(&f.ty, params)); }
            Ty::Record(m)
        }
        lex_ast::TypeExpr::Tuple { items } => Ty::Tuple(items.iter().map(|t| ty_from_canon(t, params)).collect()),
        lex_ast::TypeExpr::Function { params: ps, effects, ret } => {
            let effs = EffectSet({
                let mut s = std::collections::BTreeSet::new();
                for e in effects { s.insert(e.name.clone()); }
                s
            });
            Ty::Function {
                params: ps.iter().map(|t| ty_from_canon(t, params)).collect(),
                effects: effs,
                ret: Box::new(ty_from_canon(ret, params)),
            }
        }
        lex_ast::TypeExpr::Union { .. } => {
            // Unions on the RHS of type-decls; not in arbitrary positions.
            Ty::Unit
        }
    }
}
