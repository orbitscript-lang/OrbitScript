//! Tokens.

use orbit_hir::{Span, StrId};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenKind {
    /// An identifier, interned in the chunk's string pool. `_` and the contextual keyword
    /// `scope` are identifiers too; the parser recognises them.
    Ident(StrId),
    Int(i64),
    Float(f64),
    /// A string literal's decoded bytes (`"..."` or a raw string), interned.
    Str(StrId),
    /// An `f"..."` template; the index is into [`crate::lexer::Lexer::take_template`].
    Template(u32),
    /// A loop label, `'name`, without the quote.
    Label(StrId),
    Keyword(Keyword),
    Punct(Punct),
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    As,
    Break,
    Const,
    Continue,
    Else,
    Enum,
    Extern,
    False,
    Fn,
    For,
    Global,
    If,
    Impl,
    In,
    Let,
    Loop,
    Match,
    Mut,
    Nil,
    Pub,
    Return,
    SelfType,
    SelfValue,
    Struct,
    True,
    Use,
    While,
}

/// Every keyword's spelling, for tools such as completion.
pub const KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "else", "enum", "extern", "false", "fn", "for", "global",
    "if", "impl", "in", "let", "loop", "match", "mut", "nil", "pub", "return", "Self", "self",
    "struct", "true", "use", "while",
];

/// Words that are keywords only in one position and ordinary names elsewhere: `type` at the
/// start of an item followed by a name (`type Id = int;`), and `scope` at the start of a
/// statement (`scope close(f);`).
pub const CONTEXTUAL_KEYWORDS: &[&str] = &["type", "scope"];

impl Keyword {
    pub fn from_bytes(s: &[u8]) -> Option<Keyword> {
        use Keyword::*;
        Some(match s {
            b"as" => As,
            b"break" => Break,
            b"const" => Const,
            b"continue" => Continue,
            b"else" => Else,
            b"enum" => Enum,
            b"extern" => Extern,
            b"false" => False,
            b"fn" => Fn,
            b"for" => For,
            b"global" => Global,
            b"if" => If,
            b"impl" => Impl,
            b"in" => In,
            b"let" => Let,
            b"loop" => Loop,
            b"match" => Match,
            b"mut" => Mut,
            b"nil" => Nil,
            b"pub" => Pub,
            b"return" => Return,
            b"Self" => SelfType,
            b"self" => SelfValue,
            b"struct" => Struct,
            b"true" => True,
            b"use" => Use,
            b"while" => While,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            As => "as",
            Break => "break",
            Const => "const",
            Continue => "continue",
            Else => "else",
            Enum => "enum",
            Extern => "extern",
            False => "false",
            Fn => "fn",
            For => "for",
            Global => "global",
            If => "if",
            Impl => "impl",
            In => "in",
            Let => "let",
            Loop => "loop",
            Match => "match",
            Mut => "mut",
            Nil => "nil",
            Pub => "pub",
            Return => "return",
            SelfType => "Self",
            SelfValue => "self",
            Struct => "struct",
            True => "true",
            Use => "use",
            While => "while",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Punct {
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    /// `#{`, which opens a general table.
    HashBrace,
    Comma,
    Semi,
    Colon,
    ColonColon,
    Dot,
    DotDot,
    DotDotEq,
    DotDotDot,
    Assign,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Plus,
    Minus,
    Star,
    StarStar,
    Slash,
    Backslash,
    Percent,
    Tilde,
    Bang,
    Amp,
    AmpAmp,
    Pipe,
    PipePipe,
    Caret,
    Shl,
    Shr,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    Arrow,
    FatArrow,
    Question,
    Hash,
}

impl Punct {
    pub fn as_str(self) -> &'static str {
        use Punct::*;
        match self {
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            LBrace => "{",
            RBrace => "}",
            HashBrace => "#{",
            Comma => ",",
            Semi => ";",
            Colon => ":",
            ColonColon => "::",
            Dot => ".",
            DotDot => "..",
            DotDotEq => "..=",
            DotDotDot => "...",
            Assign => "=",
            EqEq => "==",
            NotEq => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            Plus => "+",
            Minus => "-",
            Star => "*",
            StarStar => "**",
            Slash => "/",
            Backslash => "\\",
            Percent => "%",
            Tilde => "~",
            Bang => "!",
            Amp => "&",
            AmpAmp => "&&",
            Pipe => "|",
            PipePipe => "||",
            Caret => "^",
            Shl => "<<",
            Shr => ">>",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            AmpEq => "&=",
            PipeEq => "|=",
            CaretEq => "^=",
            ShlEq => "<<=",
            ShrEq => ">>=",
            Arrow => "->",
            FatArrow => "=>",
            Question => "?",
            Hash => "#",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_lists_match_the_lexer() {
        for &k in KEYWORDS {
            let kw = Keyword::from_bytes(k.as_bytes()).expect(k);
            assert_eq!(kw.as_str(), k);
        }
        for &k in CONTEXTUAL_KEYWORDS {
            assert!(Keyword::from_bytes(k.as_bytes()).is_none(), "{k}");
        }
        // Every `Keyword` is listed: the enum's variants are in alphabetical order, and
        // `as_str` is exhaustive, so a new variant missing from `KEYWORDS` fails here.
        let all = [
            Keyword::As,
            Keyword::Break,
            Keyword::Const,
            Keyword::Continue,
            Keyword::Else,
            Keyword::Enum,
            Keyword::Extern,
            Keyword::False,
            Keyword::Fn,
            Keyword::For,
            Keyword::Global,
            Keyword::If,
            Keyword::Impl,
            Keyword::In,
            Keyword::Let,
            Keyword::Loop,
            Keyword::Match,
            Keyword::Mut,
            Keyword::Nil,
            Keyword::Pub,
            Keyword::Return,
            Keyword::SelfType,
            Keyword::SelfValue,
            Keyword::Struct,
            Keyword::True,
            Keyword::Use,
            Keyword::While,
        ];
        for kw in all {
            // A compile error here means a new keyword: add it to `all` and `KEYWORDS`.
            match kw {
                Keyword::As
                | Keyword::Break
                | Keyword::Const
                | Keyword::Continue
                | Keyword::Else
                | Keyword::Enum
                | Keyword::Extern
                | Keyword::False
                | Keyword::Fn
                | Keyword::For
                | Keyword::Global
                | Keyword::If
                | Keyword::Impl
                | Keyword::In
                | Keyword::Let
                | Keyword::Loop
                | Keyword::Match
                | Keyword::Mut
                | Keyword::Nil
                | Keyword::Pub
                | Keyword::Return
                | Keyword::SelfType
                | Keyword::SelfValue
                | Keyword::Struct
                | Keyword::True
                | Keyword::Use
                | Keyword::While => {}
            }
            assert!(KEYWORDS.contains(&kw.as_str()), "{}", kw.as_str());
        }
        assert_eq!(KEYWORDS.len(), all.len());
    }
}
