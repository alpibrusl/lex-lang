//! Spec DSL parser. Hand-rolled recursive-descent.

use crate::ast::*;
use lex_syntax::token::{lex, Token, TokenKind};

#[derive(Debug, thiserror::Error)]
#[error("spec parse error at byte {pos}: {msg}")]
pub struct SpecParseError {
    pub pos: usize,
    pub msg: String,
}

pub fn parse_spec(src: &str) -> Result<Spec, SpecParseError> {
    let toks = lex(src).map_err(|e| SpecParseError {
        pos: e.span.start, msg: format!("lex: {}", e.snippet),
    })?;
    let mut p = Parser { toks, idx: 0 };
    let spec = p.parse_spec()?;
    p.skip_newlines();
    if !p.at_eof() {
        return Err(p.err("trailing input after spec"));
    }
    Ok(spec)
}

struct Parser {
    toks: Vec<Token>,
    idx: usize,
}

impl Parser {
    fn at_eof(&self) -> bool { self.idx >= self.toks.len() }

    fn peek(&self) -> Option<&TokenKind> { self.toks.get(self.idx).map(|t| &t.kind) }

    fn bump(&mut self) -> Option<Token> {
        let t = self.toks.get(self.idx).cloned();
        if t.is_some() { self.idx += 1; }
        t
    }

    fn pos(&self) -> usize {
        self.toks.get(self.idx).map(|t| t.span.start).unwrap_or(0)
    }

    fn err(&self, m: impl Into<String>) -> SpecParseError {
        SpecParseError { pos: self.pos(), msg: m.into() }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(TokenKind::Newline) | Some(TokenKind::Semi)) {
            self.idx += 1;
        }
    }

    fn eat(&mut self, k: &TokenKind) -> bool {
        self.skip_newlines();
        if let Some(cur) = self.peek() {
            if std::mem::discriminant(cur) == std::mem::discriminant(k) {
                self.bump();
                return true;
            }
        }
        false
    }

    fn expect_ident(&mut self, ctx: &str) -> Result<String, SpecParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Ident(_)) => match self.bump().unwrap().kind {
                TokenKind::Ident(n) => Ok(n), _ => unreachable!(),
            },
            other => Err(self.err(format!("expected ident {ctx}, got {other:?}"))),
        }
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), SpecParseError> {
        // We don't have a `spec`/`forall`/`where` keyword in the lexer;
        // they come through as Ident.
        let id = self.expect_ident(&format!("(keyword `{kw}`)"))?;
        if id != kw {
            return Err(self.err(format!("expected `{kw}`, got `{id}`")));
        }
        Ok(())
    }

    fn parse_spec(&mut self) -> Result<Spec, SpecParseError> {
        // `spec <name> { forall ... : body }`
        self.expect_keyword("spec")?;
        let name = self.expect_ident("for spec name")?;
        if !self.eat(&TokenKind::LBrace) {
            return Err(self.err("expected `{` after spec name"));
        }
        self.expect_keyword("forall")?;

        let mut quantifiers = Vec::new();
        loop {
            quantifiers.push(self.parse_quantifier()?);
            self.skip_newlines();
            if self.eat(&TokenKind::Comma) {
                continue;
            }
            break;
        }

        // Optional `where <constraint>` applies to the *last* quantifier.
        if let Some(TokenKind::Ident(n)) = self.peek() {
            if n == "where" {
                self.bump();
                let c = self.parse_expr()?;
                if let Some(last) = quantifiers.last_mut() {
                    last.constraint = Some(c);
                }
            }
        }

        // Body separator: `:` or `=>`. We accept either.
        if !self.eat(&TokenKind::Colon) && !self.eat(&TokenKind::FatArrow) {
            return Err(self.err("expected `:` or `=>` before spec body"));
        }

        let body = self.parse_body_block()?;
        if !self.eat(&TokenKind::RBrace) {
            return Err(self.err("expected `}` to close spec"));
        }
        Ok(Spec { name, quantifiers, body })
    }

    fn parse_quantifier(&mut self) -> Result<Quantifier, SpecParseError> {
        let name = self.expect_ident("for quantifier var")?;
        if !self.eat(&TokenKind::ColonColon) {
            return Err(self.err("expected `::` after quantifier var"));
        }
        let ty_name = self.expect_ident("for quantifier type")?;
        let ty = match ty_name.as_str() {
            "Int" => SpecType::Int,
            "Float" => SpecType::Float,
            "Bool" => SpecType::Bool,
            "Str" => SpecType::Str,
            other => return Err(self.err(format!("unknown spec type `{other}`"))),
        };
        Ok(Quantifier { name, ty, constraint: None })
    }

    /// The body may be a sequence of `let` bindings followed by a single
    /// expression, all inside the spec's outer `{ ... }`.
    fn parse_body_block(&mut self) -> Result<SpecExpr, SpecParseError> {
        // Optional sequence of lets, then the final expression.
        self.skip_newlines();
        let mut lets: Vec<(String, SpecExpr)> = Vec::new();
        while matches!(self.peek(), Some(TokenKind::Let)) {
            self.bump(); // 'let'
            let name = self.expect_ident("after `let`")?;
            // Allow `:: Type` annotation; ignored here.
            if self.eat(&TokenKind::ColonColon) {
                let _ = self.expect_ident("after `::`")?;
            }
            if !self.eat(&TokenKind::ColonEq) {
                return Err(self.err("expected `:=` in let"));
            }
            let value = self.parse_expr()?;
            lets.push((name, value));
            self.skip_newlines();
        }
        let mut body = self.parse_expr()?;
        // Wrap in nested Let from inside out.
        for (name, value) in lets.into_iter().rev() {
            body = SpecExpr::Let { name, value: Box::new(value), body: Box::new(body) };
        }
        Ok(body)
    }

    // ---- expressions ----

    fn parse_expr(&mut self) -> Result<SpecExpr, SpecParseError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<SpecExpr, SpecParseError> {
        let mut lhs = self.parse_and()?;
        while self.peek_kw("or") {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = SpecExpr::BinOp { op: SpecOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<SpecExpr, SpecParseError> {
        let mut lhs = self.parse_cmp()?;
        while self.peek_kw("and") {
            self.bump();
            let rhs = self.parse_cmp()?;
            lhs = SpecExpr::BinOp { op: SpecOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> Result<SpecExpr, SpecParseError> {
        let lhs = self.parse_add()?;
        let op = match self.peek() {
            Some(TokenKind::EqEq) => Some(SpecOp::Eq),
            Some(TokenKind::BangEq) => Some(SpecOp::Neq),
            Some(TokenKind::Lt) => Some(SpecOp::Lt),
            Some(TokenKind::LtEq) => Some(SpecOp::Le),
            Some(TokenKind::Gt) => Some(SpecOp::Gt),
            Some(TokenKind::GtEq) => Some(SpecOp::Ge),
            _ => None,
        };
        if let Some(op) = op {
            self.bump();
            let rhs = self.parse_add()?;
            return Ok(SpecExpr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) });
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<SpecExpr, SpecParseError> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek() {
                Some(TokenKind::Plus) => SpecOp::Add,
                Some(TokenKind::Minus) => SpecOp::Sub,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_mul()?;
            lhs = SpecExpr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<SpecExpr, SpecParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(TokenKind::Star) => SpecOp::Mul,
                Some(TokenKind::Slash) => SpecOp::Div,
                Some(TokenKind::Percent) => SpecOp::Mod,
                _ => break,
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = SpecExpr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<SpecExpr, SpecParseError> {
        if self.peek_kw("not") {
            self.bump();
            let e = self.parse_unary()?;
            return Ok(SpecExpr::Not { expr: Box::new(e) });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> Result<SpecExpr, SpecParseError> {
        let mut e = self.parse_primary()?;
        while matches!(self.peek(), Some(TokenKind::LParen)) {
            self.bump();
            let mut args = Vec::new();
            self.skip_newlines();
            if !matches!(self.peek(), Some(TokenKind::RParen)) {
                args.push(self.parse_expr()?);
                while self.eat(&TokenKind::Comma) { args.push(self.parse_expr()?); }
            }
            if !self.eat(&TokenKind::RParen) {
                return Err(self.err("expected `)` to close call"));
            }
            let func = match e {
                SpecExpr::Var { name } => name,
                other => return Err(self.err(format!("only ident-callable; got {other:?}"))),
            };
            e = SpecExpr::Call { func, args };
        }
        Ok(e)
    }

    fn parse_primary(&mut self) -> Result<SpecExpr, SpecParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Int(_)) => match self.bump().unwrap().kind {
                TokenKind::Int(n) => Ok(SpecExpr::IntLit { value: n }), _ => unreachable!(),
            },
            Some(TokenKind::Float(_)) => match self.bump().unwrap().kind {
                TokenKind::Float(n) => Ok(SpecExpr::FloatLit { value: n }), _ => unreachable!(),
            },
            Some(TokenKind::Str(_)) => match self.bump().unwrap().kind {
                TokenKind::Str(s) => Ok(SpecExpr::StrLit { value: s }), _ => unreachable!(),
            },
            Some(TokenKind::True) => { self.bump(); Ok(SpecExpr::BoolLit { value: true }) }
            Some(TokenKind::False) => { self.bump(); Ok(SpecExpr::BoolLit { value: false }) }
            Some(TokenKind::Ident(_)) => {
                let name = self.expect_ident("")?;
                Ok(SpecExpr::Var { name })
            }
            Some(TokenKind::LParen) => {
                self.bump();
                let e = self.parse_expr()?;
                if !self.eat(&TokenKind::RParen) {
                    return Err(self.err("expected `)`"));
                }
                Ok(e)
            }
            other => Err(self.err(format!("expected primary expression, got {other:?}"))),
        }
    }

    fn peek_kw(&self, name: &str) -> bool {
        match self.toks.get(self.idx).map(|t| &t.kind) {
            Some(TokenKind::Ident(n)) => n == name,
            Some(TokenKind::And) => name == "and",
            Some(TokenKind::Or) => name == "or",
            Some(TokenKind::Not) => name == "not",
            _ => false,
        }
    }
}
