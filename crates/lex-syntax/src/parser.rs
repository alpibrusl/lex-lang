//! Recursive-descent parser for Lex. Pratt-style precedence climbing for
//! binary operators; everything else is straightforward LL(1)-with-lookahead.

use crate::syntax::*;
use crate::token::{Token, TokenKind};

pub fn parse(tokens: Vec<Token>) -> Result<Program, ParseError> {
    let mut p = Parser::new(tokens);
    let program = p.parse_program()?;
    p.skip_newlines();
    if !p.at_eof() {
        return Err(p.error("unexpected token after program"));
    }
    Ok(program)
}

#[derive(Debug, thiserror::Error)]
#[error("parse error at byte {pos}: {msg}")]
pub struct ParseError {
    pub pos: usize,
    pub msg: String,
}

struct Parser {
    tokens: Vec<Token>,
    idx: usize,
    /// Recursion depth across `parse_expr`. Capped at `MAX_DEPTH`
    /// to defend against adversarial input like a long sequence of
    /// `[[[{{{...` that would otherwise blow the stack. Found by
    /// the libFuzzer parser target — see `fuzz/fuzz_targets/parser.rs`.
    depth: u32,
}

/// Maximum nesting depth the parser will accept before refusing
/// with a parse error. Real Lex code rarely exceeds 30; 96 leaves
/// generous headroom for legitimate generated code.
///
/// Each `parse_expr` level produces ~4-5 stack frames through the
/// `parse_binary_expr → parse_unary_expr → parse_postfix →
/// parse_primary → ...` chain, so this caps the actual frame
/// count around 400-500 — well below even a 2 MiB test stack.
const MAX_DEPTH: u32 = 96;

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, idx: 0, depth: 0 }
    }

    fn at_eof(&self) -> bool {
        self.idx >= self.tokens.len()
    }

    fn peek(&self) -> Option<&TokenKind> {
        self.tokens.get(self.idx).map(|t| &t.kind)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.idx).cloned();
        if t.is_some() {
            self.idx += 1;
        }
        t
    }

    fn current_pos(&self) -> usize {
        self.tokens
            .get(self.idx)
            .map(|t| t.span.start)
            .unwrap_or_else(|| self.tokens.last().map(|t| t.span.end).unwrap_or(0))
    }

    fn error(&self, msg: impl Into<String>) -> ParseError {
        ParseError { pos: self.current_pos(), msg: msg.into() }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(TokenKind::Newline) | Some(TokenKind::Semi)) {
            self.idx += 1;
        }
    }

    fn expect(&mut self, expected: &TokenKind, ctx: &str) -> Result<Token, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(k) if std::mem::discriminant(k) == std::mem::discriminant(expected) => {
                Ok(self.bump().unwrap())
            }
            Some(other) => Err(self.error(format!(
                "expected {expected:?} {ctx}, got {other:?}"
            ))),
            None => Err(self.error(format!("expected {expected:?} {ctx}, got EOF"))),
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

    fn expect_ident(&mut self, ctx: &str) -> Result<String, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Ident(_)) => match self.bump().unwrap().kind {
                TokenKind::Ident(name) => Ok(name),
                _ => unreachable!(),
            },
            other => Err(self.error(format!("expected identifier {ctx}, got {other:?}"))),
        }
    }

    // --- top level ---

    fn parse_program(&mut self) -> Result<Program, ParseError> {
        let mut items = Vec::new();
        loop {
            self.skip_newlines();
            if self.at_eof() {
                break;
            }
            items.push(self.parse_item()?);
        }
        Ok(Program { items })
    }

    fn parse_item(&mut self) -> Result<Item, ParseError> {
        match self.peek() {
            Some(TokenKind::Import) => self.parse_import().map(Item::Import),
            Some(TokenKind::Type) => self.parse_type_decl().map(Item::TypeDecl),
            Some(TokenKind::Fn) => self.parse_fn_decl().map(Item::FnDecl),
            other => Err(self.error(format!(
                "expected `import`, `type`, or `fn` at top level, got {other:?}"
            ))),
        }
    }

    fn parse_import(&mut self) -> Result<Import, ParseError> {
        self.expect(&TokenKind::Import, "in import")?;
        let reference = match self.bump().map(|t| t.kind) {
            Some(TokenKind::Str(s)) => s,
            other => return Err(self.error(format!("expected string after `import`, got {other:?}"))),
        };
        self.expect(&TokenKind::As, "in import")?;
        let alias = self.expect_ident("for import alias")?;
        Ok(Import { reference, alias })
    }

    fn parse_type_decl(&mut self) -> Result<TypeDecl, ParseError> {
        self.expect(&TokenKind::Type, "in type decl")?;
        let name = self.expect_ident("for type name")?;
        let params = if self.eat(&TokenKind::LBracket) {
            let ps = self.parse_ident_list()?;
            self.expect(&TokenKind::RBracket, "after type params")?;
            ps
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::Eq, "in type decl")?;
        let definition = self.parse_type_decl_rhs()?;
        Ok(TypeDecl { name, params, definition })
    }

    fn parse_ident_list(&mut self) -> Result<Vec<String>, ParseError> {
        let mut out = Vec::new();
        out.push(self.expect_ident("in identifier list")?);
        while self.eat(&TokenKind::Comma) {
            out.push(self.expect_ident("in identifier list")?);
        }
        Ok(out)
    }

    /// `type Foo = Variant1 | Variant2(Payload)` is a union; otherwise a plain type expression.
    fn parse_type_decl_rhs(&mut self) -> Result<TypeExpr, ParseError> {
        let first = self.parse_type_expr()?;
        // Detect union: PascalCase ident (or named type w/ optional payload) followed by `|`.
        if matches!(self.peek_skip_newlines(), Some(TokenKind::Bar)) {
            let mut variants = Vec::new();
            variants.push(type_to_variant(first)?);
            while self.eat(&TokenKind::Bar) {
                let next = self.parse_type_expr()?;
                variants.push(type_to_variant(next)?);
            }
            Ok(TypeExpr::Union(variants))
        } else {
            Ok(first)
        }
    }

    fn peek_skip_newlines(&mut self) -> Option<TokenKind> {
        let saved = self.idx;
        self.skip_newlines();
        let out = self.peek().cloned();
        self.idx = saved;
        out
    }

    fn parse_type_expr(&mut self) -> Result<TypeExpr, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::LBrace) => self.parse_record_type(),
            Some(TokenKind::LParen) => self.parse_paren_type_or_function(),
            Some(TokenKind::Ident(_)) => {
                let name = self.expect_ident("in type expr")?;
                let args = if matches!(self.peek(), Some(TokenKind::LBracket)) {
                    self.bump();
                    let mut args = Vec::new();
                    args.push(self.parse_type_expr()?);
                    while self.eat(&TokenKind::Comma) {
                        args.push(self.parse_type_expr()?);
                    }
                    self.expect(&TokenKind::RBracket, "after type args")?;
                    args
                } else if matches!(self.peek(), Some(TokenKind::LParen)) {
                    // Constructor type with payload: `Name(T)` or `Name(T1, T2)`.
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
                        args.push(self.parse_type_expr()?);
                        while self.eat(&TokenKind::Comma) {
                            args.push(self.parse_type_expr()?);
                        }
                    }
                    self.expect(&TokenKind::RParen, "after constructor payload")?;
                    args
                } else {
                    Vec::new()
                };
                Ok(TypeExpr::Named { name, args })
            }
            other => Err(self.error(format!("expected type expression, got {other:?}"))),
        }
    }

    fn parse_record_type(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect(&TokenKind::LBrace, "in record type")?;
        let mut fields = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) {
            loop {
                self.skip_newlines();
                let name = self.expect_ident("in record field")?;
                self.expect(&TokenKind::ColonColon, "after record field name")?;
                let ty = self.parse_type_expr()?;
                fields.push(TypeField { name, ty });
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) { break; }
                if matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) { break; }
            }
        }
        self.expect(&TokenKind::RBrace, "in record type")?;
        Ok(TypeExpr::Record(fields))
    }

    fn parse_paren_type_or_function(&mut self) -> Result<TypeExpr, ParseError> {
        self.expect(&TokenKind::LParen, "in type")?;
        let mut args = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
            args.push(self.parse_type_expr()?);
            while self.eat(&TokenKind::Comma) {
                args.push(self.parse_type_expr()?);
            }
        }
        self.expect(&TokenKind::RParen, "in type")?;
        // Function type if followed by `->`.
        if matches!(self.peek_skip_newlines(), Some(TokenKind::Arrow)) {
            self.skip_newlines();
            self.bump();
            let effects = self.parse_effects()?;
            let ret = self.parse_type_expr()?;
            Ok(TypeExpr::Function {
                params: args,
                effects,
                ret: Box::new(ret),
            })
        } else if args.len() == 1 {
            // Parenthesized type expression.
            Ok(args.into_iter().next().unwrap())
        } else {
            Ok(TypeExpr::Tuple(args))
        }
    }

    fn parse_fn_decl(&mut self) -> Result<FnDecl, ParseError> {
        self.expect(&TokenKind::Fn, "in fn decl")?;
        let name = self.expect_ident("for function name")?;
        let type_params = if self.eat(&TokenKind::LBracket) {
            let ps = self.parse_ident_list()?;
            self.expect(&TokenKind::RBracket, "after type params")?;
            ps
        } else {
            Vec::new()
        };
        self.expect(&TokenKind::LParen, "before params")?;
        let mut params = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
            params.push(self.parse_param()?);
            while self.eat(&TokenKind::Comma) {
                params.push(self.parse_param()?);
            }
        }
        self.expect(&TokenKind::RParen, "after params")?;
        self.expect(&TokenKind::Arrow, "before return type")?;
        let effects = self.parse_effects()?;
        let return_type = self.parse_type_expr()?;
        let body = self.parse_block()?;
        Ok(FnDecl { name, type_params, params, effects, return_type, body })
    }

    fn parse_param(&mut self) -> Result<Param, ParseError> {
        let name = self.expect_ident("for parameter name")?;
        self.expect(&TokenKind::ColonColon, "after parameter name")?;
        let ty = self.parse_type_expr()?;
        Ok(Param { name, ty })
    }

    fn parse_effects(&mut self) -> Result<Vec<Effect>, ParseError> {
        if !self.eat(&TokenKind::LBracket) {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RBracket)) {
            out.push(self.parse_effect()?);
            while self.eat(&TokenKind::Comma) {
                out.push(self.parse_effect()?);
            }
        }
        self.expect(&TokenKind::RBracket, "after effects")?;
        Ok(out)
    }

    fn parse_effect(&mut self) -> Result<Effect, ParseError> {
        let name = self.expect_ident("for effect name")?;
        let arg = if self.eat(&TokenKind::LParen) {
            let arg = match self.bump().map(|t| t.kind) {
                Some(TokenKind::Str(s)) => EffectArg::Str(s),
                Some(TokenKind::Int(n)) => EffectArg::Int(n),
                Some(TokenKind::Ident(s)) => EffectArg::Ident(s),
                other => return Err(self.error(format!("invalid effect arg: {other:?}"))),
            };
            self.expect(&TokenKind::RParen, "after effect arg")?;
            Some(arg)
        } else {
            None
        };
        Ok(Effect { name, arg })
    }

    // --- blocks and statements ---

    fn parse_block(&mut self) -> Result<Block, ParseError> {
        self.expect(&TokenKind::LBrace, "before block")?;
        let mut statements = Vec::new();
        let result;
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Some(TokenKind::RBrace)) {
                // Empty block: synthesize Unit literal.
                result = Box::new(Expr::Lit(Literal::Unit));
                break;
            }
            // Try parsing a let; otherwise an expression.
            if matches!(self.peek(), Some(TokenKind::Let)) {
                let stmt = self.parse_let_statement()?;
                statements.push(stmt);
                self.skip_newlines();
                continue;
            }
            let expr = self.parse_expr()?;
            self.skip_newlines();
            // If the next token is `}`, this expression is the block's result.
            if matches!(self.peek(), Some(TokenKind::RBrace)) {
                result = Box::new(expr);
                break;
            }
            statements.push(Statement::Expr(expr));
        }
        self.expect(&TokenKind::RBrace, "to close block")?;
        Ok(Block { statements, result })
    }

    fn parse_let_statement(&mut self) -> Result<Statement, ParseError> {
        self.expect(&TokenKind::Let, "in let")?;
        let name = self.expect_ident("after `let`")?;
        let ty = if self.eat(&TokenKind::ColonColon) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };
        self.expect(&TokenKind::ColonEq, "in let")?;
        let value = self.parse_expr()?;
        Ok(Statement::Let { name, ty, value })
    }

    // --- expressions ---

    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        // Recursion gate: every nested expression — match arms,
        // tuple/list/record/block elements, function args, etc. —
        // enters here, so this is the right place to bound depth.
        // Decrement happens whether the inner call succeeds or fails.
        if self.depth >= MAX_DEPTH {
            return Err(ParseError {
                pos: self.current_pos(),
                msg: format!(
                    "expression nests too deeply (max {MAX_DEPTH}); \
                     malformed or hand-crafted input?"),
            });
        }
        self.depth += 1;
        let r = self.parse_expr_inner();
        self.depth -= 1;
        r
    }

    fn parse_expr_inner(&mut self) -> Result<Expr, ParseError> {
        // Pipes are left-associative and bind less tightly than binary ops.
        let mut left = self.parse_binary_expr(0)?;
        while matches!(self.peek_skip_newlines(), Some(TokenKind::Pipe)) {
            self.skip_newlines();
            self.bump();
            let right = self.parse_binary_expr(0)?;
            left = Expr::Pipe { left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn parse_binary_expr(&mut self, min_prec: u8) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek_binop() {
                Some(op) if op.precedence() >= min_prec => op,
                _ => break,
            };
            self.skip_newlines();
            self.bump();
            let rhs = self.parse_binary_expr(op.precedence() + 1)?;
            lhs = Expr::BinOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn peek_binop(&mut self) -> Option<BinOp> {
        match self.peek_skip_newlines()? {
            TokenKind::Plus => Some(BinOp::Add),
            TokenKind::Minus => Some(BinOp::Sub),
            TokenKind::Star => Some(BinOp::Mul),
            TokenKind::Slash => Some(BinOp::Div),
            TokenKind::Percent => Some(BinOp::Mod),
            TokenKind::EqEq => Some(BinOp::Eq),
            TokenKind::BangEq => Some(BinOp::Neq),
            TokenKind::Lt => Some(BinOp::Lt),
            TokenKind::LtEq => Some(BinOp::Lte),
            TokenKind::Gt => Some(BinOp::Gt),
            TokenKind::GtEq => Some(BinOp::Gte),
            TokenKind::And => Some(BinOp::And),
            TokenKind::Or => Some(BinOp::Or),
            _ => None,
        }
    }

    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Not) => {
                self.bump();
                let inner = self.parse_unary()?;
                Ok(Expr::UnaryOp { op: UnaryOp::Not, expr: Box::new(inner) })
            }
            Some(TokenKind::Minus) => {
                self.bump();
                let inner = self.parse_unary()?;
                Ok(Expr::UnaryOp { op: UnaryOp::Neg, expr: Box::new(inner) })
            }
            _ => self.parse_postfix(),
        }
    }

    fn parse_postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_primary()?;
        loop {
            // Postfix operations don't cross newlines (they bind tightly).
            match self.peek() {
                Some(TokenKind::Dot) => {
                    self.bump();
                    let field = self.expect_ident("after `.`")?;
                    expr = Expr::Field { value: Box::new(expr), field };
                }
                Some(TokenKind::LParen) => {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
                        args.push(self.parse_expr()?);
                        while self.eat(&TokenKind::Comma) {
                            args.push(self.parse_expr()?);
                        }
                    }
                    self.expect(&TokenKind::RParen, "in call")?;
                    expr = Expr::Call { callee: Box::new(expr), args };
                }
                Some(TokenKind::Question) => {
                    self.bump();
                    expr = Expr::Try(Box::new(expr));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Int(_)) => match self.bump().unwrap().kind {
                TokenKind::Int(n) => Ok(Expr::Lit(Literal::Int(n))),
                _ => unreachable!(),
            },
            Some(TokenKind::Float(_)) => match self.bump().unwrap().kind {
                TokenKind::Float(n) => Ok(Expr::Lit(Literal::Float(n))),
                _ => unreachable!(),
            },
            Some(TokenKind::Str(_)) => match self.bump().unwrap().kind {
                TokenKind::Str(s) => Ok(Expr::Lit(Literal::Str(s))),
                _ => unreachable!(),
            },
            Some(TokenKind::Bytes(_)) => match self.bump().unwrap().kind {
                TokenKind::Bytes(b) => Ok(Expr::Lit(Literal::Bytes(b))),
                _ => unreachable!(),
            },
            Some(TokenKind::True) => { self.bump(); Ok(Expr::Lit(Literal::Bool(true))) }
            Some(TokenKind::False) => { self.bump(); Ok(Expr::Lit(Literal::Bool(false))) }
            Some(TokenKind::If) => self.parse_if(),
            Some(TokenKind::Match) => self.parse_match(),
            Some(TokenKind::Fn) => self.parse_lambda(),
            Some(TokenKind::LBrace) => self.parse_brace_expr(),
            Some(TokenKind::LBracket) => self.parse_list_literal(),
            Some(TokenKind::LParen) => self.parse_paren_or_tuple(),
            Some(TokenKind::Ident(_)) => self.parse_ident_or_record(),
            other => Err(self.error(format!("expected expression, got {other:?}"))),
        }
    }

    /// Disambiguate `{` between record literal and block.
    /// Lookahead: `{ Ident :` is a record literal; `{ }` is also a record
    /// (empty block has no use). Anything else is a block.
    fn parse_brace_expr(&mut self) -> Result<Expr, ParseError> {
        // Save position; peek 2-3 tokens past `{` (skipping newlines).
        let saved = self.idx;
        self.bump(); // `{`
        // Skip newlines.
        while matches!(self.peek(), Some(TokenKind::Newline) | Some(TokenKind::Semi)) {
            self.idx += 1;
        }
        let is_record = matches!(self.peek(), Some(TokenKind::RBrace))
            || (matches!(self.peek(), Some(TokenKind::Ident(_)))
                && matches!(self.tokens.get(self.idx + 1).map(|t| &t.kind), Some(TokenKind::Colon) | Some(TokenKind::Comma) | Some(TokenKind::RBrace)));
        self.idx = saved;
        if is_record {
            self.parse_record_literal()
        } else {
            Ok(Expr::Block(self.parse_block()?))
        }
    }

    fn parse_record_literal(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LBrace, "in record literal")?;
        let mut fields = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) {
            loop {
                self.skip_newlines();
                let name = self.expect_ident("in record literal")?;
                let value = if self.eat(&TokenKind::Colon) {
                    self.parse_expr()?
                } else {
                    // shorthand: `{ name }` => `{ name: name }`
                    Expr::Var(name.clone())
                };
                fields.push(RecordLitField { name, value });
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) { break; }
                if matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) { break; }
            }
        }
        self.expect(&TokenKind::RBrace, "after record literal")?;
        Ok(Expr::RecordLit(fields))
    }

    fn parse_if(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::If, "in if")?;
        let cond = self.parse_expr()?;
        let then_block = self.parse_block()?;
        self.expect(&TokenKind::Else, "expected `else`")?;
        let else_block = self.parse_block()?;
        Ok(Expr::If { cond: Box::new(cond), then_block, else_block })
    }

    fn parse_match(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::Match, "in match")?;
        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::LBrace, "before match arms")?;
        let mut arms = Vec::new();
        loop {
            self.skip_newlines();
            if matches!(self.peek(), Some(TokenKind::RBrace)) { break; }
            let pattern = self.parse_pattern()?;
            self.expect(&TokenKind::FatArrow, "in match arm")?;
            let body = self.parse_expr()?;
            arms.push(Arm { pattern, body });
            self.skip_newlines();
            if !self.eat(&TokenKind::Comma) { break; }
        }
        self.expect(&TokenKind::RBrace, "after match arms")?;
        Ok(Expr::Match { scrutinee: Box::new(scrutinee), arms })
    }

    fn parse_lambda(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::Fn, "in lambda")?;
        self.expect(&TokenKind::LParen, "before lambda params")?;
        let mut params = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
            params.push(self.parse_param()?);
            while self.eat(&TokenKind::Comma) {
                params.push(self.parse_param()?);
            }
        }
        self.expect(&TokenKind::RParen, "after lambda params")?;
        self.expect(&TokenKind::Arrow, "before lambda return type")?;
        let effects = self.parse_effects()?;
        let return_type = self.parse_type_expr()?;
        let body = self.parse_block()?;
        Ok(Expr::Lambda(Box::new(Lambda { params, effects, return_type, body })))
    }

    fn parse_list_literal(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LBracket, "before list literal")?;
        let mut items = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RBracket)) {
            items.push(self.parse_expr()?);
            while self.eat(&TokenKind::Comma) {
                if matches!(self.peek_skip_newlines(), Some(TokenKind::RBracket)) { break; }
                items.push(self.parse_expr()?);
            }
        }
        self.expect(&TokenKind::RBracket, "after list literal")?;
        Ok(Expr::ListLit(items))
    }

    fn parse_paren_or_tuple(&mut self) -> Result<Expr, ParseError> {
        self.expect(&TokenKind::LParen, "")?;
        if matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
            self.bump();
            return Ok(Expr::Lit(Literal::Unit));
        }
        let first = self.parse_expr()?;
        if self.eat(&TokenKind::Comma) {
            let mut items = vec![first];
            if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
                items.push(self.parse_expr()?);
                while self.eat(&TokenKind::Comma) {
                    if matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) { break; }
                    items.push(self.parse_expr()?);
                }
            }
            self.expect(&TokenKind::RParen, "after tuple")?;
            Ok(Expr::TupleLit(items))
        } else {
            self.expect(&TokenKind::RParen, "after parenthesized expression")?;
            Ok(first)
        }
    }

    fn parse_ident_or_record(&mut self) -> Result<Expr, ParseError> {
        // Ident is parsed as a Var; later postfix (`(`, `.`, `?`) attach.
        let name = self.expect_ident("")?;
        Ok(Expr::Var(name))
    }

    // --- patterns ---

    fn parse_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.skip_newlines();
        match self.peek() {
            Some(TokenKind::Underscore) => { self.bump(); Ok(Pattern::Wild) }
            Some(TokenKind::Int(_)) => match self.bump().unwrap().kind {
                TokenKind::Int(n) => Ok(Pattern::Lit(Literal::Int(n))),
                _ => unreachable!(),
            },
            Some(TokenKind::Float(_)) => match self.bump().unwrap().kind {
                TokenKind::Float(n) => Ok(Pattern::Lit(Literal::Float(n))),
                _ => unreachable!(),
            },
            Some(TokenKind::Str(_)) => match self.bump().unwrap().kind {
                TokenKind::Str(s) => Ok(Pattern::Lit(Literal::Str(s))),
                _ => unreachable!(),
            },
            Some(TokenKind::True) => { self.bump(); Ok(Pattern::Lit(Literal::Bool(true))) }
            Some(TokenKind::False) => { self.bump(); Ok(Pattern::Lit(Literal::Bool(false))) }
            Some(TokenKind::LBrace) => self.parse_record_pattern(),
            Some(TokenKind::LParen) => self.parse_tuple_pattern(),
            Some(TokenKind::Ident(_)) => {
                let name = self.expect_ident("")?;
                if matches!(self.peek(), Some(TokenKind::LParen)) {
                    self.bump();
                    let mut args = Vec::new();
                    if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
                        args.push(self.parse_pattern()?);
                        while self.eat(&TokenKind::Comma) {
                            args.push(self.parse_pattern()?);
                        }
                    }
                    self.expect(&TokenKind::RParen, "after constructor pattern")?;
                    Ok(Pattern::Constructor { name, args })
                } else {
                    Ok(Pattern::Var(name))
                }
            }
            other => Err(self.error(format!("expected pattern, got {other:?}"))),
        }
    }

    fn parse_record_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.expect(&TokenKind::LBrace, "")?;
        let mut fields = Vec::new();
        let rest = false;
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) {
            loop {
                self.skip_newlines();
                let name = self.expect_ident("in record pattern")?;
                let pattern = if self.eat(&TokenKind::Colon) {
                    Some(self.parse_pattern()?)
                } else {
                    None
                };
                fields.push(RecordPatField { name, pattern });
                self.skip_newlines();
                if !self.eat(&TokenKind::Comma) { break; }
                if matches!(self.peek_skip_newlines(), Some(TokenKind::RBrace)) { break; }
            }
        }
        self.expect(&TokenKind::RBrace, "after record pattern")?;
        Ok(Pattern::Record { fields, rest })
    }

    fn parse_tuple_pattern(&mut self) -> Result<Pattern, ParseError> {
        self.expect(&TokenKind::LParen, "")?;
        let mut items = Vec::new();
        if !matches!(self.peek_skip_newlines(), Some(TokenKind::RParen)) {
            items.push(self.parse_pattern()?);
            while self.eat(&TokenKind::Comma) {
                items.push(self.parse_pattern()?);
            }
        }
        self.expect(&TokenKind::RParen, "after tuple pattern")?;
        if items.len() == 1 {
            Ok(items.into_iter().next().unwrap())
        } else {
            Ok(Pattern::Tuple(items))
        }
    }
}

/// In a union RHS, every leaf must be a `Named` type expression — that is, a
/// PascalCase ident with optional payload via `Variant(payload_type)`.
fn type_to_variant(t: TypeExpr) -> Result<UnionVariant, ParseError> {
    match t {
        TypeExpr::Named { name, args } => {
            let payload = match args.len() {
                0 => None,
                1 => Some(args.into_iter().next().unwrap()),
                _ => Some(TypeExpr::Tuple(args)),
            };
            Ok(UnionVariant { name, payload })
        }
        // `Foo({ field :: T })` parses as Named with one arg = Record. handled above.
        _ => Err(ParseError {
            pos: 0,
            msg: "union variant must be a constructor name".into(),
        }),
    }
}
