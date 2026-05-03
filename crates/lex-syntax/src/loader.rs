//! Multi-file loader: resolves `import "./..."`, `import "../..."`, and
//! `import "/abs/..."` statements relative to the importer, recursively
//! parses, and produces a single [`Program`] with all stages merged.
//!
//! Names that are local to an imported file are mangled with the alias
//! path so they don't collide with the importer's names. Stdlib imports
//! (`import "std.foo" as bar`) pass through unchanged.
//!
//! ## Mangling
//!
//! Each loaded file has an "alias path" — empty for the root file, `f`
//! for `import "./X" as f` from the root, `f.g` for `import "./Y" as g`
//! inside X, etc. Within a file at alias path `P`:
//!
//! - `fn foo` declared in this file becomes `<P>.foo` (just `foo` at root).
//! - `type T` declared in this file becomes `<P>.T`.
//! - References to a locally-declared name get mangled, **unless** the
//!   name is shadowed by a binder (let, fn param, lambda param, or
//!   pattern binder) in scope.
//! - `m.foo` where `m` is a path-import alias is rewritten to the
//!   imported file's alias-path-qualified name.
//! - `m.foo` where `m` is a stdlib alias is unchanged.
//!
//! Variant constructors are **not** mangled — they live in a global
//! namespace, and a collision between two imported types' constructors
//! surfaces later as a type-check error. Same for record field names.
//!
//! ## Limitations (tracked separately)
//!
//! Mangling means the SigId / StageId of an imported function depends
//! on the alias chain the importer chose. `lex blame` across imports
//! is not stable yet — see the future-work tracker for store-native
//! imports (`import "stage:..."`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::syntax::*;
use crate::{parse_source, SyntaxError};

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parse {path}: {source}")]
    Syntax {
        path: String,
        #[source]
        source: SyntaxError,
    },
    #[error("import cycle: {chain}")]
    Cycle { chain: String },
    #[error("import \"{reference}\" from {importer}: file not found")]
    NotFound { importer: String, reference: String },
    #[error("local imports (`./`, `../`, `/`) require a base path; cannot resolve from a string source")]
    LocalImportInStringSource,
}

/// Load a multi-file Lex program, expanding local imports relative to
/// the entry path. Stdlib imports (`std.*`) pass through unchanged.
pub fn load_program(entry: &Path) -> Result<Program, LoadError> {
    let mut state = LoaderState {
        in_progress: Vec::new(),
    };
    state.load(entry, "")
}

/// Load a Lex program from a string source. Local-path imports are
/// rejected up-front since there's no base path to resolve from.
pub fn load_program_from_str(src: &str) -> Result<Program, LoadError> {
    let prog = parse_source(src).map_err(|source| LoadError::Syntax {
        path: "<input>".into(),
        source,
    })?;
    for item in &prog.items {
        if let Item::Import(imp) = item {
            if is_path_import(&imp.reference) {
                return Err(LoadError::LocalImportInStringSource);
            }
        }
    }
    Ok(prog)
}

struct LoaderState {
    in_progress: Vec<PathBuf>,
}

impl LoaderState {
    fn load(&mut self, path: &Path, alias_path: &str) -> Result<Program, LoadError> {
        let canonical = path.canonicalize().map_err(|source| LoadError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if self.in_progress.contains(&canonical) {
            let mut chain: Vec<String> = self
                .in_progress
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            chain.push(canonical.display().to_string());
            return Err(LoadError::Cycle {
                chain: chain.join(" -> "),
            });
        }
        self.in_progress.push(canonical.clone());

        let src = std::fs::read_to_string(&canonical).map_err(|source| LoadError::Io {
            path: canonical.display().to_string(),
            source,
        })?;
        let prog = parse_source(&src).map_err(|source| LoadError::Syntax {
            path: canonical.display().to_string(),
            source,
        })?;

        let local_names: HashSet<String> = prog
            .items
            .iter()
            .filter_map(|item| match item {
                Item::FnDecl(fd) => Some(fd.name.clone()),
                Item::TypeDecl(td) => Some(td.name.clone()),
                _ => None,
            })
            .collect();

        let mut path_imports: HashMap<String, String> = HashMap::new();
        let mut merged_children: Vec<Item> = Vec::new();
        let mut std_imports: Vec<Item> = Vec::new();
        let mut my_items: Vec<Item> = Vec::new();

        for item in prog.items {
            match item {
                Item::Import(ref imp) if is_path_import(&imp.reference) => {
                    let resolved = resolve_import(&canonical, &imp.reference)?;
                    let child_alias_path = if alias_path.is_empty() {
                        imp.alias.clone()
                    } else {
                        format!("{alias_path}.{}", imp.alias)
                    };
                    path_imports.insert(imp.alias.clone(), child_alias_path.clone());
                    let child_prog = self.load(&resolved, &child_alias_path)?;
                    merged_children.extend(child_prog.items);
                }
                Item::Import(_) => std_imports.push(item),
                _ => my_items.push(item),
            }
        }

        let mangler = Mangler {
            alias_path: alias_path.to_string(),
            local_names: &local_names,
            path_imports: &path_imports,
        };
        let mangled: Vec<Item> = my_items
            .into_iter()
            .map(|i| mangler.mangle_item(i))
            .collect();

        self.in_progress.pop();

        // Output order: std imports first (deduped against children's),
        // then merged children's items, then this file's items.
        let mut out: Vec<Item> = Vec::new();
        for s in std_imports {
            if !merged_children.iter().any(|m| m == &s) {
                out.push(s);
            }
        }
        out.extend(merged_children);
        out.extend(mangled);
        Ok(Program { items: out })
    }
}

fn is_path_import(reference: &str) -> bool {
    reference.starts_with("./") || reference.starts_with("../") || reference.starts_with('/')
}

fn resolve_import(importer: &Path, reference: &str) -> Result<PathBuf, LoadError> {
    let importer_dir = importer.parent().unwrap_or_else(|| Path::new("."));
    let mut resolved: PathBuf = if reference.starts_with('/') {
        PathBuf::from(reference)
    } else {
        importer_dir.join(reference)
    };
    if resolved.extension().is_none() {
        resolved.set_extension("lex");
    }
    if !resolved.exists() {
        return Err(LoadError::NotFound {
            importer: importer.display().to_string(),
            reference: reference.to_string(),
        });
    }
    Ok(resolved)
}

struct Mangler<'a> {
    alias_path: String,
    local_names: &'a HashSet<String>,
    /// Map from local alias to the imported file's alias path.
    path_imports: &'a HashMap<String, String>,
}

impl<'a> Mangler<'a> {
    fn qualify(&self, name: &str) -> String {
        if self.alias_path.is_empty() {
            name.to_string()
        } else {
            format!("{}.{}", self.alias_path, name)
        }
    }

    fn mangle_item(&self, item: Item) -> Item {
        match item {
            Item::Import(imp) => Item::Import(imp),
            Item::TypeDecl(td) => Item::TypeDecl(self.mangle_type_decl(td)),
            Item::FnDecl(fd) => Item::FnDecl(self.mangle_fn_decl(fd)),
        }
    }

    fn mangle_type_decl(&self, td: TypeDecl) -> TypeDecl {
        TypeDecl {
            name: self.qualify(&td.name),
            params: td.params,
            definition: self.mangle_type_expr(td.definition),
        }
    }

    fn mangle_fn_decl(&self, fd: FnDecl) -> FnDecl {
        let mut shadow = HashSet::new();
        for p in &fd.params {
            shadow.insert(p.name.clone());
        }
        FnDecl {
            name: self.qualify(&fd.name),
            type_params: fd.type_params,
            params: fd
                .params
                .into_iter()
                .map(|p| Param {
                    name: p.name,
                    ty: self.mangle_type_expr(p.ty),
                })
                .collect(),
            effects: fd.effects,
            return_type: self.mangle_type_expr(fd.return_type),
            body: self.mangle_block(fd.body, &shadow),
        }
    }

    fn mangle_type_expr(&self, te: TypeExpr) -> TypeExpr {
        match te {
            TypeExpr::Named { name, args } => TypeExpr::Named {
                name: self.rewrite_type_name(&name),
                args: args.into_iter().map(|a| self.mangle_type_expr(a)).collect(),
            },
            TypeExpr::Record(fields) => TypeExpr::Record(
                fields
                    .into_iter()
                    .map(|f| TypeField {
                        name: f.name,
                        ty: self.mangle_type_expr(f.ty),
                    })
                    .collect(),
            ),
            TypeExpr::Tuple(items) => {
                TypeExpr::Tuple(items.into_iter().map(|t| self.mangle_type_expr(t)).collect())
            }
            TypeExpr::Function {
                params,
                effects,
                ret,
            } => TypeExpr::Function {
                params: params
                    .into_iter()
                    .map(|t| self.mangle_type_expr(t))
                    .collect(),
                effects,
                ret: Box::new(self.mangle_type_expr(*ret)),
            },
            TypeExpr::Union(variants) => TypeExpr::Union(
                variants
                    .into_iter()
                    .map(|v| UnionVariant {
                        name: v.name,
                        payload: v.payload.map(|t| self.mangle_type_expr(t)),
                    })
                    .collect(),
            ),
        }
    }

    /// Rewrite a possibly-qualified type name to its mangled form.
    fn rewrite_type_name(&self, name: &str) -> String {
        if let Some((alias, rest)) = name.split_once('.') {
            if let Some(child) = self.path_imports.get(alias) {
                return format!("{child}.{rest}");
            }
            return name.to_string();
        }
        if self.local_names.contains(name) {
            return self.qualify(name);
        }
        name.to_string()
    }

    fn mangle_block(&self, b: Block, shadow: &HashSet<String>) -> Block {
        let mut shadow = shadow.clone();
        let statements = b
            .statements
            .into_iter()
            .map(|s| match s {
                Statement::Let { name, ty, value } => {
                    let value = self.mangle_expr(value, &shadow);
                    let ty = ty.map(|t| self.mangle_type_expr(t));
                    shadow.insert(name.clone());
                    Statement::Let { name, ty, value }
                }
                Statement::Expr(e) => Statement::Expr(self.mangle_expr(e, &shadow)),
            })
            .collect();
        let result = Box::new(self.mangle_expr(*b.result, &shadow));
        Block { statements, result }
    }

    fn mangle_expr(&self, e: Expr, shadow: &HashSet<String>) -> Expr {
        match e {
            Expr::Lit(_) => e,
            Expr::Var(name) => {
                if !shadow.contains(&name) && self.local_names.contains(&name) {
                    Expr::Var(self.qualify(&name))
                } else {
                    Expr::Var(name)
                }
            }
            Expr::Block(b) => Expr::Block(self.mangle_block(b, shadow)),
            Expr::Call { callee, args } => {
                let mangled_args: Vec<Expr> = args
                    .into_iter()
                    .map(|a| self.mangle_expr(a, shadow))
                    .collect();
                if let Expr::Field { value, field } = (*callee).clone() {
                    if let Expr::Var(alias) = *value {
                        if !shadow.contains(&alias) {
                            if let Some(child) = self.path_imports.get(&alias) {
                                return Expr::Call {
                                    callee: Box::new(Expr::Var(format!("{child}.{field}"))),
                                    args: mangled_args,
                                };
                            }
                        }
                    }
                }
                Expr::Call {
                    callee: Box::new(self.mangle_expr(*callee, shadow)),
                    args: mangled_args,
                }
            }
            Expr::Pipe { left, right } => Expr::Pipe {
                left: Box::new(self.mangle_expr(*left, shadow)),
                right: Box::new(self.mangle_expr(*right, shadow)),
            },
            Expr::Try(inner) => Expr::Try(Box::new(self.mangle_expr(*inner, shadow))),
            Expr::Field { value, field } => {
                if let Expr::Var(alias) = (*value).clone() {
                    if !shadow.contains(&alias) {
                        if let Some(child) = self.path_imports.get(&alias) {
                            return Expr::Var(format!("{child}.{field}"));
                        }
                    }
                }
                Expr::Field {
                    value: Box::new(self.mangle_expr(*value, shadow)),
                    field,
                }
            }
            Expr::BinOp { op, lhs, rhs } => Expr::BinOp {
                op,
                lhs: Box::new(self.mangle_expr(*lhs, shadow)),
                rhs: Box::new(self.mangle_expr(*rhs, shadow)),
            },
            Expr::UnaryOp { op, expr } => Expr::UnaryOp {
                op,
                expr: Box::new(self.mangle_expr(*expr, shadow)),
            },
            Expr::If {
                cond,
                then_block,
                else_block,
            } => Expr::If {
                cond: Box::new(self.mangle_expr(*cond, shadow)),
                then_block: self.mangle_block(then_block, shadow),
                else_block: self.mangle_block(else_block, shadow),
            },
            Expr::Match { scrutinee, arms } => Expr::Match {
                scrutinee: Box::new(self.mangle_expr(*scrutinee, shadow)),
                arms: arms
                    .into_iter()
                    .map(|a| {
                        let mut arm_shadow = shadow.clone();
                        collect_pattern_binders(&a.pattern, &mut arm_shadow);
                        Arm {
                            pattern: self.mangle_pattern(a.pattern),
                            body: self.mangle_expr(a.body, &arm_shadow),
                        }
                    })
                    .collect(),
            },
            Expr::RecordLit(fields) => Expr::RecordLit(
                fields
                    .into_iter()
                    .map(|f| RecordLitField {
                        name: f.name,
                        value: self.mangle_expr(f.value, shadow),
                    })
                    .collect(),
            ),
            Expr::TupleLit(items) => Expr::TupleLit(
                items
                    .into_iter()
                    .map(|i| self.mangle_expr(i, shadow))
                    .collect(),
            ),
            Expr::ListLit(items) => Expr::ListLit(
                items
                    .into_iter()
                    .map(|i| self.mangle_expr(i, shadow))
                    .collect(),
            ),
            Expr::Constructor { name, args } => Expr::Constructor {
                name,
                args: args
                    .into_iter()
                    .map(|a| self.mangle_expr(a, shadow))
                    .collect(),
            },
            Expr::Lambda(lambda) => {
                let mut lam_shadow = shadow.clone();
                for p in &lambda.params {
                    lam_shadow.insert(p.name.clone());
                }
                Expr::Lambda(Box::new(Lambda {
                    params: lambda
                        .params
                        .into_iter()
                        .map(|p| Param {
                            name: p.name,
                            ty: self.mangle_type_expr(p.ty),
                        })
                        .collect(),
                    return_type: self.mangle_type_expr(lambda.return_type),
                    effects: lambda.effects,
                    body: self.mangle_block(lambda.body, &lam_shadow),
                }))
            }
        }
    }

    fn mangle_pattern(&self, p: Pattern) -> Pattern {
        match p {
            Pattern::Constructor { name, args } => Pattern::Constructor {
                name,
                args: args.into_iter().map(|a| self.mangle_pattern(a)).collect(),
            },
            Pattern::Record { fields, rest } => Pattern::Record {
                fields: fields
                    .into_iter()
                    .map(|f| RecordPatField {
                        name: f.name,
                        pattern: f.pattern.map(|p| self.mangle_pattern(p)),
                    })
                    .collect(),
                rest,
            },
            Pattern::Tuple(items) => {
                Pattern::Tuple(items.into_iter().map(|p| self.mangle_pattern(p)).collect())
            }
            Pattern::Lit(_) | Pattern::Var(_) | Pattern::Wild => p,
        }
    }
}

fn collect_pattern_binders(p: &Pattern, out: &mut HashSet<String>) {
    match p {
        Pattern::Var(name) => {
            out.insert(name.clone());
        }
        Pattern::Constructor { args, .. } => {
            for a in args {
                collect_pattern_binders(a, out);
            }
        }
        Pattern::Record { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => collect_pattern_binders(p, out),
                    // `{ name }` shorthand binds `name`.
                    None => {
                        out.insert(f.name.clone());
                    }
                }
            }
        }
        Pattern::Tuple(items) => {
            for p in items {
                collect_pattern_binders(p, out);
            }
        }
        Pattern::Lit(_) | Pattern::Wild => {}
    }
}
