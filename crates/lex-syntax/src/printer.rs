//! Pretty-printer for the syntax tree. Designed so
//! `parse(text) → print → parse` round-trips on the canonical AST.

use crate::syntax::*;
use std::fmt::Write;

pub fn print_program(program: &Program) -> String {
    let mut p = Printer::new();
    p.program(program);
    p.out
}

struct Printer {
    out: String,
    indent: usize,
}

impl Printer {
    fn new() -> Self { Self { out: String::new(), indent: 0 } }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
    }

    fn nl(&mut self) {
        self.out.push('\n');
    }

    fn program(&mut self, p: &Program) {
        for (i, item) in p.items.iter().enumerate() {
            if i > 0 { self.nl(); }
            self.item(item);
        }
        if !p.items.is_empty() {
            self.nl();
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Import(i) => {
                writeln!(self.out, "import \"{}\" as {}", i.reference, i.alias).unwrap();
            }
            Item::TypeDecl(td) => self.type_decl(td),
            Item::FnDecl(fd) => self.fn_decl(fd),
        }
    }

    fn type_decl(&mut self, td: &TypeDecl) {
        write!(self.out, "type {}", td.name).unwrap();
        if !td.params.is_empty() {
            write!(self.out, "[{}]", td.params.join(", ")).unwrap();
        }
        write!(self.out, " = ").unwrap();
        self.type_expr(&td.definition);
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
            self.type_expr(&p.ty);
        }
        write!(self.out, ") -> ").unwrap();
        self.effects(&fd.effects);
        self.type_expr(&fd.return_type);
        write!(self.out, " ").unwrap();
        self.block(&fd.body);
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
                    EffectArg::Str(s) => write!(self.out, "(\"{}\")", s).unwrap(),
                    EffectArg::Int(n) => write!(self.out, "({})", n).unwrap(),
                    EffectArg::Ident(s) => write!(self.out, "({})", s).unwrap(),
                }
            }
        }
        write!(self.out, "] ").unwrap();
    }

    fn type_expr(&mut self, t: &TypeExpr) {
        match t {
            TypeExpr::Named { name, args } => {
                write!(self.out, "{}", name).unwrap();
                if !args.is_empty() {
                    write!(self.out, "[").unwrap();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 { write!(self.out, ", ").unwrap(); }
                        self.type_expr(a);
                    }
                    write!(self.out, "]").unwrap();
                }
            }
            TypeExpr::Record(fs) => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fs.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{} :: ", f.name).unwrap();
                    self.type_expr(&f.ty);
                }
                write!(self.out, " }}").unwrap();
            }
            TypeExpr::Tuple(items) => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.type_expr(it);
                }
                write!(self.out, ")").unwrap();
            }
            TypeExpr::Function { params, effects, ret } => {
                write!(self.out, "(").unwrap();
                for (i, p) in params.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.type_expr(p);
                }
                write!(self.out, ") -> ").unwrap();
                self.effects(effects);
                self.type_expr(ret);
            }
            TypeExpr::Union(variants) => {
                for (i, v) in variants.iter().enumerate() {
                    if i > 0 { write!(self.out, " | ").unwrap(); }
                    write!(self.out, "{}", v.name).unwrap();
                    if let Some(payload) = &v.payload {
                        write!(self.out, "(").unwrap();
                        self.type_expr(payload);
                        write!(self.out, ")").unwrap();
                    }
                }
            }
            TypeExpr::Refined { base, binding, predicate } => {
                self.type_expr(base);
                write!(self.out, "{{{} | ", binding).unwrap();
                self.expr(predicate);
                write!(self.out, "}}").unwrap();
            }
        }
    }

    fn block(&mut self, b: &Block) {
        write!(self.out, "{{").unwrap();
        self.indent += 1;
        for stmt in &b.statements {
            self.nl();
            self.write_indent();
            self.statement(stmt);
        }
        self.nl();
        self.write_indent();
        self.expr(&b.result);
        self.indent -= 1;
        self.nl();
        self.write_indent();
        write!(self.out, "}}").unwrap();
    }

    fn statement(&mut self, s: &Statement) {
        match s {
            Statement::Let { name, ty, value } => {
                write!(self.out, "let {}", name).unwrap();
                if let Some(ty) = ty {
                    write!(self.out, " :: ").unwrap();
                    self.type_expr(ty);
                }
                write!(self.out, " := ").unwrap();
                self.expr(value);
            }
            Statement::Expr(e) => self.expr(e),
        }
    }

    fn expr(&mut self, e: &Expr) {
        self.expr_prec(e, 0);
    }

    fn expr_prec(&mut self, e: &Expr, parent_prec: u8) {
        match e {
            Expr::Lit(l) => self.literal(l),
            Expr::Var(n) => { write!(self.out, "{}", n).unwrap(); }
            Expr::Block(b) => self.block(b),
            Expr::Call { callee, args } => {
                self.expr_prec(callee, 100);
                write!(self.out, "(").unwrap();
                for (i, a) in args.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(a);
                }
                write!(self.out, ")").unwrap();
            }
            Expr::Pipe { left, right } => {
                if parent_prec > 0 { write!(self.out, "(").unwrap(); }
                self.expr_prec(left, 1);
                write!(self.out, " |> ").unwrap();
                self.expr_prec(right, 1);
                if parent_prec > 0 { write!(self.out, ")").unwrap(); }
            }
            Expr::Try(inner) => {
                self.expr_prec(inner, 100);
                write!(self.out, "?").unwrap();
            }
            Expr::Field { value, field } => {
                self.expr_prec(value, 100);
                write!(self.out, ".{}", field).unwrap();
            }
            Expr::BinOp { op, lhs, rhs } => {
                let prec = op.precedence() + 10;
                if parent_prec > prec { write!(self.out, "(").unwrap(); }
                self.expr_prec(lhs, prec);
                write!(self.out, " {} ", op.as_str()).unwrap();
                self.expr_prec(rhs, prec + 1);
                if parent_prec > prec { write!(self.out, ")").unwrap(); }
            }
            Expr::UnaryOp { op, expr } => {
                let s = match op { UnaryOp::Neg => "-", UnaryOp::Not => "not " };
                write!(self.out, "{}", s).unwrap();
                self.expr_prec(expr, 100);
            }
            Expr::If { cond, then_block, else_block } => {
                write!(self.out, "if ").unwrap();
                self.expr(cond);
                write!(self.out, " ").unwrap();
                self.block(then_block);
                write!(self.out, " else ").unwrap();
                self.block(else_block);
            }
            Expr::Match { scrutinee, arms } => {
                write!(self.out, "match ").unwrap();
                self.expr(scrutinee);
                write!(self.out, " {{").unwrap();
                self.indent += 1;
                for arm in arms {
                    self.nl();
                    self.write_indent();
                    self.pattern(&arm.pattern);
                    write!(self.out, " => ").unwrap();
                    self.expr(&arm.body);
                    write!(self.out, ",").unwrap();
                }
                self.indent -= 1;
                self.nl();
                self.write_indent();
                write!(self.out, "}}").unwrap();
            }
            Expr::RecordLit(fields) => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{}: ", f.name).unwrap();
                    self.expr(&f.value);
                }
                write!(self.out, " }}").unwrap();
            }
            Expr::TupleLit(items) => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(it);
                }
                write!(self.out, ")").unwrap();
            }
            Expr::ListLit(items) => {
                write!(self.out, "[").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.expr(it);
                }
                write!(self.out, "]").unwrap();
            }
            Expr::Constructor { name, args } => {
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
            Expr::Lambda(l) => {
                write!(self.out, "fn (").unwrap();
                for (i, p) in l.params.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{} :: ", p.name).unwrap();
                    self.type_expr(&p.ty);
                }
                write!(self.out, ") -> ").unwrap();
                self.effects(&l.effects);
                self.type_expr(&l.return_type);
                write!(self.out, " ").unwrap();
                self.block(&l.body);
            }
        }
    }

    fn literal(&mut self, l: &Literal) {
        match l {
            Literal::Int(n) => write!(self.out, "{}", n).unwrap(),
            Literal::Float(n) => write!(self.out, "{}", format_float(*n)).unwrap(),
            Literal::Str(s) => write!(self.out, "\"{}\"", escape(s)).unwrap(),
            Literal::Bytes(b) => {
                write!(self.out, "b\"").unwrap();
                for &c in b {
                    if c.is_ascii() && (c as char).is_ascii_graphic() && c != b'"' && c != b'\\' {
                        self.out.push(c as char);
                    } else {
                        write!(self.out, "\\x{:02x}", c).unwrap();
                    }
                }
                write!(self.out, "\"").unwrap();
            }
            Literal::Bool(b) => write!(self.out, "{}", b).unwrap(),
            Literal::Unit => write!(self.out, "()").unwrap(),
        }
    }

    fn pattern(&mut self, p: &Pattern) {
        match p {
            Pattern::Lit(l) => self.literal(l),
            Pattern::Var(n) => { write!(self.out, "{}", n).unwrap(); }
            Pattern::Wild => { write!(self.out, "_").unwrap(); }
            Pattern::Constructor { name, args } => {
                write!(self.out, "{}", name).unwrap();
                if !args.is_empty() {
                    write!(self.out, "(").unwrap();
                    for (i, a) in args.iter().enumerate() {
                        if i > 0 { write!(self.out, ", ").unwrap(); }
                        self.pattern(a);
                    }
                    write!(self.out, ")").unwrap();
                }
            }
            Pattern::Record { fields, rest: _ } => {
                write!(self.out, "{{ ").unwrap();
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    write!(self.out, "{}", f.name).unwrap();
                    if let Some(p) = &f.pattern {
                        write!(self.out, ": ").unwrap();
                        self.pattern(p);
                    }
                }
                write!(self.out, " }}").unwrap();
            }
            Pattern::Tuple(items) => {
                write!(self.out, "(").unwrap();
                for (i, it) in items.iter().enumerate() {
                    if i > 0 { write!(self.out, ", ").unwrap(); }
                    self.pattern(it);
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

fn format_float(n: f64) -> String {
    if n.is_finite() && n == n.trunc() {
        format!("{:.1}", n)
    } else {
        format!("{}", n)
    }
}
