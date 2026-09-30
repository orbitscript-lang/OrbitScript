//! Tokens.

use orbit_hir::{Span, StrId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenKind {
    /// An identifier, interned in the chunk's string pool.
    Name(StrId),
    Int(i64),
    Float(f64),
    /// A string literal's decoded bytes, interned in the chunk's string pool.
    Str(StrId),
    Keyword(Keyword),
    Punct(Punct),
    /// A byte that starts no token (`@`, `$`, control characters). The parser reports it as
    /// `unexpected symbol`, as Lua does.
    Other(u8),
    Eof,
}

/// Lua 5.5's reserved words. `global`, the 23rd keyword, is contextual in the default 5.5 build
/// (it stays usable as a name), so the lexer returns it as a `Name` and the parser recognises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
}

impl Keyword {
    pub fn from_bytes(s: &[u8]) -> Option<Keyword> {
        use Keyword::*;
        Some(match s {
            b"and" => And,
            b"break" => Break,
            b"do" => Do,
            b"else" => Else,
            b"elseif" => Elseif,
            b"end" => End,
            b"false" => False,
            b"for" => For,
            b"function" => Function,
            b"goto" => Goto,
            b"if" => If,
            b"in" => In,
            b"local" => Local,
            b"nil" => Nil,
            b"not" => Not,
            b"or" => Or,
            b"repeat" => Repeat,
            b"return" => Return,
            b"then" => Then,
            b"true" => True,
            b"until" => Until,
            b"while" => While,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            And => "and",
            Break => "break",
            Do => "do",
            Else => "else",
            Elseif => "elseif",
            End => "end",
            False => "false",
            For => "for",
            Function => "function",
            Goto => "goto",
            If => "if",
            In => "in",
            Local => "local",
            Nil => "nil",
            Not => "not",
            Or => "or",
            Repeat => "repeat",
            Return => "return",
            Then => "then",
            True => "true",
            Until => "until",
            While => "while",
        }
    }
}

/// Every operator and separator, including those the first-light subset rejects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punct {
    Plus,
    Minus,
    Star,
    Slash,
    SlashSlash,
    Percent,
    Caret,
    Hash,
    Amp,
    Tilde,
    Pipe,
    Shl,
    Shr,
    EqEq,
    NotEq,
    LtEq,
    GtEq,
    Lt,
    Gt,
    Assign,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    ColonColon,
    Semi,
    Colon,
    Comma,
    Dot,
    DotDot,
    Ellipsis,
}

impl Punct {
    pub fn as_str(self) -> &'static str {
        use Punct::*;
        match self {
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            SlashSlash => "//",
            Percent => "%",
            Caret => "^",
            Hash => "#",
            Amp => "&",
            Tilde => "~",
            Pipe => "|",
            Shl => "<<",
            Shr => ">>",
            EqEq => "==",
            NotEq => "~=",
            LtEq => "<=",
            GtEq => ">=",
            Lt => "<",
            Gt => ">",
            Assign => "=",
            LParen => "(",
            RParen => ")",
            LBrace => "{",
            RBrace => "}",
            LBracket => "[",
            RBracket => "]",
            ColonColon => "::",
            Semi => ";",
            Colon => ":",
            Comma => ",",
            Dot => ".",
            DotDot => "..",
            Ellipsis => "...",
        }
    }
}
