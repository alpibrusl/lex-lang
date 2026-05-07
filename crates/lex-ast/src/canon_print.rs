//! Pretty-printer for canonical AST. Output is parseable Lex source.
//!
//! This is the inverse-ish of `canonicalize_program`: it does NOT recover
//! `if` or `?` (those are folded away in the canonical form). All branching
//! prints as `match`; `?` would print as its full match expansion.

use crate::canonical::*;
use std::fmt::Write;

pub fn print_stages(stages: &[Stage]) -> String {
    let mut p = Printer::new();
    for (i, s) in stages.iter().enumerate() {
        if i > 0 { p.out.push('\n'); }
        p.stage(s);
    }
    p.out
}

struct Printer { out: String, indent: usize }

impl Printer {
    fn new() -> Self { Self { out: String::new(), indent: 0 } }
    fn nl(&mut self) { self.out.push('\n'); }
    fn pad(&mut self) { for _ in 0..self.indent { self.out.push_str("  "); } }

    fn stage(&mut self, s: &Stage) {
        match s {
            Stage::Import(i) => {
                writeln!(self.out, "import \"{}\" as {}", i.reference, i.alias).unwrap();
            }
            Stage::TypeDecl(td) => self.type_decl(td),
            Stage::FnDecl(fd) => self.fn_decl(fd),
        }
    }

    fn type_decl(&mut self, td: &TypeDecl) {
        write!(self.out, "type {}", td.name).unwrap();
        if !td.params.is_empty() {
            write!(self.out, "[{}]", td.params.join(", ")).unwrap();
        }
        write!(self.out, " = ").unwrap();
        self.ty(&td.definition);
        self.nl();
    }

    fn fn_decl(&mut self, fd: &FnDecl) {
        write!(self.out, "fn {}", fd.name).unwrap();
        if !fd.type_params.is_empty() {
            write!(self.out, "[{}]", fd.type_params.join(", ")).unwrap();
        }
        write!(self.out, "(").unwrap();
        for (i, p) in fd.params.iter().enumerate() {
            if i > 0 { write!(self.out, ", ").unwrap(); }
            write!(self.out, "{} :: ", p.name).unwrap();
            self.ty(&p.ty);
        }
        write!(self.out, ") -> ").unwrap();
        self.effects(&fd.effects);
        self.ty(&fd.return_type);
        write!(self.out, " ").unwrap();
        self.expr_as_block(&fd.body);
        self.nl();
    }

    fn effects(&mut self, effects: &[Effect]) {
        if effects.is_empty() { return; }
        write!(self.out, "[").unwrap();
        for (i, e) in effects.iter().enumerate() {
            if i > 0 { write!(self.out, ", ").unwrap(); }
            write!(self.out, "{}", e.name).unwrap();
            if let Some(arg) = &e.arg {
                match arg {
                    EffectArg::Str { value } => write!(self.out, "(\"{}\")", value).unwrap(),
                    EffectArg::Int { value } => write!(self.out, "({})", value).unwrap(),
                    EffectArg::Ident { value } => write!(self.out, "({})", value).unwrap(),
                }
            }
        }
        write!(self.out, "] ").unwrap();
    }

    fn ty(&mut self, t: &TypeExpr) {
        match t {
            TypeExpr::Named { name, args } => {
                write!(self.out, "{}", name).unwrap();
                if !args.is_empty() {
                    write!(self.out, "[").unwrap();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 { write!(self.out, ", ").unwrap(); }
                        self.ty(a);
                    }
                    write!(self.out, "]").unwrap();
                }
            }
            TypeExpr::Record { fields } => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{} :: ", f.name).unwrap();
                    self.ty(&f.ty);
                }
                write!(self.out, " }}").unwrap();
            }
            TypeExpr::Tuple { items } => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.ty(it);
                }
                write!(self.out, ")").unwrap();
            }
            TypeExpr::Function { params, effects, ret } => {
                write!(self.out, "(").unwrap();
                for (i, p) in params.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.ty(p);
                }
                write!(self.out, ") -> ").unwrap();
                self.effects(effects);
                self.ty(ret);
            }
            TypeExpr::Union { variants } => {
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 { write!(self.out, " | ").unwrap(); }
                    write!(self.out, "{}", v.name).unwrap();
                    if let Some(p) = &v.payload {
                        write!(self.out, "(").unwrap();
                        self.ty(p);
                        write!(self.out, ")").unwrap();
                    }
                }
            }
            TypeExpr::Refined { base, binding, predicate } => {
                self.ty(base);
                write!(self.out, "{{{} | ", binding).unwrap();
                self.expr(predicate);
                write!(self.out, "}}").unwrap();
            }
        }
    }

    /// Print a CExpr as a `{ ... }` block, even if it's not a Block — we wrap it.
    fn expr_as_block(&mut self, e: &CExpr) {
        write!(self.out, "{{").unwrap();
        self.indent += 1;
        self.print_block_contents(e);
        self.indent -= 1;
        self.nl();
        self.pad();
        write!(self.out, "}}").unwrap();
    }

    /// Emit the contents of a block — i.e. flatten Lets and Block.statements
    /// onto separate lines, ending with the result expression.
    fn print_block_contents(&mut self, e: &CExpr) {
        match e {
            CExpr::Let { name, ty, value, body } => {
                self.nl();
                self.pad();
                write!(self.out, "let {}", name).unwrap();
                if let Some(ty) = ty {
                    write!(self.out, " :: ").unwrap();
                    self.ty(ty);
                }
                write!(self.out, " := ").unwrap();
                self.expr(value);
                self.print_block_contents(body);
            }
            CExpr::Block { statements, result } => {
                for s in statements {
                    self.nl();
                    self.pad();
                    self.expr(s);
                }
                self.print_block_contents(result);
            }
            other => {
                self.nl();
                self.pad();
                self.expr(other);
            }
        }
    }

    fn expr(&mut self, e: &CExpr) {
        match e {
            CExpr::Literal { value } => self.lit(value),
            CExpr::Var { name } => { write!(self.out, "{}", name).unwrap(); }
            CExpr::Call { callee, args } => {
                self.expr(callee);
                write!(self.out, "(").unwrap();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(a);
                }
                write!(self.out, ")").unwrap();
            }
            CExpr::Let { .. } | CExpr::Block { .. } => {
                self.expr_as_block(e);
            }
            CExpr::Match { scrutinee, arms } => {
                write!(self.out, "match ").unwrap();
                self.expr(scrutinee);
                write!(self.out, " {{").unwrap();
                self.indent += 1;
                for arm in arms {
                    self.nl();
                    self.pad();
                    self.pat(&arm.pattern);
                    write!(self.out, " => ").unwrap();
                    self.expr(&arm.body);
                    write!(self.out, ",").unwrap();
                }
                self.indent -= 1;
                self.nl();
                self.pad();
                write!(self.out, "}}").unwrap();
            }
            CExpr::Constructor { name, args } => {
                write!(self.out, "{}", name).unwrap();
                if !args.is_empty() {
                    write!(self.out, "(").unwrap();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 { write!(self.out, ", ").unwrap(); }
                        self.expr(a);
                    }
                    write!(self.out, ")").unwrap();
                }
            }
            CExpr::RecordLit { fields } => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{}: ", f.name).unwrap();
                    self.expr(&f.value);
                }
                write!(self.out, " }}").unwrap();
            }
            CExpr::TupleLit { items } => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(it);
                }
                write!(self.out, ")").unwrap();
            }
            CExpr::ListLit { items } => {
                write!(self.out, "[").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(it);
                }
                write!(self.out, "]").unwrap();
            }
            CExpr::FieldAccess { value, field } => {
                self.expr(value);
                write!(self.out, ".{}", field).unwrap();
            }
            CExpr::Lambda { params, return_type, effects, body } => {
                write!(self.out, "fn (").unwrap();
                for (i, p) in params.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{} :: ", p.name).unwrap();
                    self.ty(&p.ty);
                }
                write!(self.out, ") -> ").unwrap();
                self.effects(effects);
                self.ty(return_type);
                write!(self.out, " ").unwrap();
                self.expr_as_block(body);
            }
            CExpr::BinOp { op, lhs, rhs } => {
                write!(self.out, "(").unwrap();
                self.expr(lhs);
                write!(self.out, " {} ", op).unwrap();
                self.expr(rhs);
                write!(self.out, ")").unwrap();
            }
            CExpr::UnaryOp { op, expr } => {
                if op == "not" {
                    write!(self.out, "(not ").unwrap();
                } else {
                    write!(self.out, "({}", op).unwrap();
                }
                self.expr(expr);
                write!(self.out, ")").unwrap();
            }
            CExpr::Return { value } => {
                // No source-level form for an early return; we approximate by
                // printing the value (M3+ rejects programs that need this).
                self.expr(value);
            }
        }
    }

    fn lit(&mut self, l: &CLit) {
        match l {
            CLit::Int { value } => write!(self.out, "{}", value).unwrap(),
            CLit::Float { value } => write!(self.out, "{}", value).unwrap(),
            CLit::Str { value } => write!(self.out, "\"{}\"", escape(value)).unwrap(),
            CLit::Bytes { value } => {
                // Hex-encoded bytes; round-trip via raw byte string.
                write!(self.out, "b\"").unwrap();
                let bytes = decode_hex(value);
                for b in bytes {
                    if b.is_ascii() && (b as char).is_ascii_graphic() && b != b'"' && b != b'\\' {
                        self.out.push(b as char);
                    } else {
                        write!(self.out, "\\x{:02x}", b).unwrap();
                    }
                }
                write!(self.out, "\"").unwrap();
            }
            CLit::Bool { value } => write!(self.out, "{}", value).unwrap(),
            CLit::Unit => write!(self.out, "()").unwrap(),
        }
    }

    fn pat(&mut self, p: &Pattern) {
        match p {
            Pattern::PLiteral { value } => self.lit(value),
            Pattern::PVar { name } => { write!(self.out, "{}", name).unwrap(); }
            Pattern::PWild => { write!(self.out, "_").unwrap(); }
            Pattern::PConstructor { name, args } => {
                write!(self.out, "{}", name).unwrap();
                if !args.is_empty() {
                    write!(self.out, "(").unwrap();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 { write!(self.out, ", ").unwrap(); }
                        self.pat(a);
                    }
                    write!(self.out, ")").unwrap();
                }
            }
            Pattern::PRecord { fields } => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{}: ", f.name).unwrap();
                    self.pat(&f.pattern);
                }
                write!(self.out, " }}").unwrap();
            }
            Pattern::PTuple { items } => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.pat(it);
                }
                write!(self.out, ")").unwrap();
            }
        }
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn decode_hex(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        let h = (parse_hex(bytes[i]) << 4) | parse_hex(bytes[i + 1]);
        out.push(h);
        i += 2;
    }
    out
}

fn parse_hex(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}
