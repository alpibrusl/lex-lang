use logos::Logos;
use std::ops::Range;

#[derive(Logos, Debug, Clone, PartialEq)]
#[logos(skip r"[ \t\r\f]+")]
#[logos(skip(r"#[^\n]*", allow_greedy = true))]
pub enum TokenKind {
    // keywords
    #[token("fn")]      Fn,
    #[token("let")]     Let,
    #[token("type")]    Type,
    #[token("match")]   Match,
    #[token("if")]      If,
    #[token("else")]    Else,
    #[token("return")]  Return,
    #[token("import")]  Import,
    #[token("as")]      As,
    #[token("true")]    True,
    #[token("false")]   False,
    #[token("and")]     And,
    #[token("or")]      Or,
    #[token("not")]     Not,

    // multi-char operators (longer first to win the match race)
    #[token("|>")] Pipe,
    #[token("->")] Arrow,
    #[token("=>")] FatArrow,
    #[token(":=")] ColonEq,
    #[token("::")] ColonColon,
    #[token("==")] EqEq,
    #[token("!=")] BangEq,
    #[token("<=")] LtEq,
    #[token(">=")] GtEq,

    // single-char operators
    #[token("+")] Plus,
    #[token("-")] Minus,
    #[token("*")] Star,
    #[token("/")] Slash,
    #[token("%")] Percent,
    #[token("<")] Lt,
    #[token(">")] Gt,
    #[token(".")] Dot,
    #[token(",")] Comma,
    #[token(";")] Semi,
    #[token(":")] Colon,
    #[token("?")] Question,
    #[token("(")] LParen,
    #[token(")")] RParen,
    #[token("{")] LBrace,
    #[token("}")] RBrace,
    #[token("[")] LBracket,
    #[token("]")] RBracket,
    #[token("=")] Eq,
    #[token("|")] Bar,
    #[token("_")] Underscore,
    #[token("\n")] Newline,

    // literals
    #[regex(r"[0-9][0-9_]*\.[0-9][0-9_]*", |lex| lex.slice().replace('_', "").parse::<f64>().ok())]
    Float(f64),

    #[regex(r"[0-9][0-9_]*", |lex| lex.slice().replace('_', "").parse::<i64>().ok(), priority = 3)]
    Int(i64),

    #[regex(r#""([^"\\]|\\.)*""#, |lex| unescape(&lex.slice()[1..lex.slice().len()-1]))]
    Str(String),

    #[regex(r#"b"([^"\\]|\\.)*""#, |lex| unescape(&lex.slice()[2..lex.slice().len()-1]).map(|s| s.into_bytes()))]
    Bytes(Vec<u8>),

    // Identifier. Two alternatives so a bare `_` keeps lexing as
    // the discard token (used by `match _ => ...` and the new
    // `let _ := ...`) while `_name` is recognized as a real
    // identifier (#200). Logos picks the longer match: for `_`
    // alone only Underscore matches (Ident requires ≥2 chars on
    // the underscore branch); for `_x` the Ident branch wins.
    #[regex(r"[a-zA-Z][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    #[regex(r"_[a-zA-Z0-9_]+", |lex| lex.slice().to_string())]
    Ident(String),
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                '0' => out.push('\0'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Range<usize>,
}

pub fn lex(src: &str) -> Result<Vec<Token>, LexError> {
    let mut toks = Vec::new();
    let mut lx = TokenKind::lexer(src);
    while let Some(res) = lx.next() {
        match res {
            Ok(kind) => toks.push(Token { kind, span: lx.span() }),
            Err(_) => {
                return Err(LexError {
                    span: lx.span(),
                    snippet: lx.slice().to_string(),
                });
            }
        }
    }
    Ok(toks)
}

#[derive(Debug, thiserror::Error)]
#[error("unrecognized token `{snippet}` at {span:?}")]
pub struct LexError {
    pub span: Range<usize>,
    pub snippet: String,
}
