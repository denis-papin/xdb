//! Hand-written lexer for the PL/SQL subset.
//!
//! Identifiers and keywords are case-insensitive and stored in lower case.
//! Keywords are not distinguished from identifiers here: the parser decides from
//! context. Comments (`--` and `/* */`) are skipped.

use crate::CompileError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tok {
    /// Identifier or keyword, lower case.
    Ident(String),
    Int(i64),
    Str(String),
    LParen,
    RParen,
    Comma,
    Semi,
    Dot,
    DotDot,
    Assign, // :=
    Plus,
    Minus,
    Star,
    Slash,
    Concat, // ||
    Eq,
    Ne, // <> or !=
    Lt,
    Le,
    Gt,
    Ge,
    Percent,
    Eof,
}

impl Tok {
    pub fn describe(&self) -> String {
        match self {
            Tok::Ident(s) => format!("'{s}'"),
            Tok::Int(i) => format!("number {i}"),
            Tok::Str(s) => format!("string '{s}'"),
            Tok::LParen => "'('".into(),
            Tok::RParen => "')'".into(),
            Tok::Comma => "','".into(),
            Tok::Semi => "';'".into(),
            Tok::Dot => "'.'".into(),
            Tok::DotDot => "'..'".into(),
            Tok::Assign => "':='".into(),
            Tok::Plus => "'+'".into(),
            Tok::Minus => "'-'".into(),
            Tok::Star => "'*'".into(),
            Tok::Slash => "'/'".into(),
            Tok::Concat => "'||'".into(),
            Tok::Eq => "'='".into(),
            Tok::Ne => "'<>'".into(),
            Tok::Lt => "'<'".into(),
            Tok::Le => "'<='".into(),
            Tok::Gt => "'>'".into(),
            Tok::Ge => "'>='".into(),
            Tok::Percent => "'%'".into(),
            Tok::Eof => "end of input".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
}

fn err(line: u32, col: u32, msg: impl Into<String>) -> CompileError {
    CompileError { line, col, msg: msg.into() }
}

/// Splits `src` into tokens. The result always ends with a `Tok::Eof` token.
pub fn lex(src: &str) -> Result<Vec<Token>, CompileError> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1u32;
    let mut col = 1u32;

    // Advance one char, tracking line/col.
    macro_rules! bump {
        () => {{
            if chars[i] == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            i += 1;
        }};
    }

    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let (tl, tc) = (line, col);

        if c.is_whitespace() {
            bump!();
            continue;
        }
        if c == '-' && next == Some('-') {
            while i < chars.len() && chars[i] != '\n' {
                bump!();
            }
            continue;
        }
        if c == '/' && next == Some('*') {
            bump!();
            bump!();
            loop {
                if i >= chars.len() {
                    return Err(err(tl, tc, "unterminated /* comment"));
                }
                if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    bump!();
                    bump!();
                    break;
                }
                bump!();
            }
            continue;
        }

        let push = |out: &mut Vec<Token>, tok: Tok| out.push(Token { tok, line: tl, col: tc });

        if c.is_ascii_alphabetic() {
            let mut s = String::new();
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '_' || chars[i] == '$' || chars[i] == '#')
            {
                s.push(chars[i].to_ascii_lowercase());
                bump!();
            }
            push(&mut out, Tok::Ident(s));
            continue;
        }
        if c.is_ascii_digit() {
            let mut s = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                s.push(chars[i]);
                bump!();
            }
            if i < chars.len() && chars[i] == '.' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                return Err(err(tl, tc, "decimal literals are not supported (NUMBER is an integer in this POC)"));
            }
            if i < chars.len() && (chars[i].is_ascii_alphabetic() || chars[i] == '_') {
                return Err(err(line, col, format!("unexpected character '{}' after number", chars[i])));
            }
            let v: i64 = s
                .parse()
                .map_err(|_| err(tl, tc, format!("integer literal {s} is out of range")))?;
            push(&mut out, Tok::Int(v));
            continue;
        }
        if c == '\'' {
            bump!();
            let mut s = String::new();
            loop {
                if i >= chars.len() {
                    return Err(err(tl, tc, "unterminated string literal"));
                }
                if chars[i] == '\'' {
                    if chars.get(i + 1) == Some(&'\'') {
                        s.push('\'');
                        bump!();
                        bump!();
                        continue;
                    }
                    bump!();
                    break;
                }
                s.push(chars[i]);
                bump!();
            }
            push(&mut out, Tok::Str(s));
            continue;
        }
        if c == '"' {
            return Err(err(tl, tc, "quoted identifiers are not supported"));
        }

        let two = |a: char, b: char| c == a && next == Some(b);
        let (tok, len) = if two(':', '=') {
            (Tok::Assign, 2)
        } else if two('.', '.') {
            (Tok::DotDot, 2)
        } else if two('|', '|') {
            (Tok::Concat, 2)
        } else if two('<', '>') || two('!', '=') || two('^', '=') {
            (Tok::Ne, 2)
        } else if two('<', '=') {
            (Tok::Le, 2)
        } else if two('>', '=') {
            (Tok::Ge, 2)
        } else if two('<', '<') {
            return Err(err(tl, tc, "labels (<<label>>) are not supported"));
        } else {
            let t = match c {
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                ',' => Tok::Comma,
                ';' => Tok::Semi,
                '.' => Tok::Dot,
                '+' => Tok::Plus,
                '-' => Tok::Minus,
                '*' => Tok::Star,
                '/' => Tok::Slash,
                '=' => Tok::Eq,
                '<' => Tok::Lt,
                '>' => Tok::Gt,
                '%' => Tok::Percent,
                ':' => return Err(err(tl, tc, "bind variables (:name) are not supported")),
                other => return Err(err(tl, tc, format!("unexpected character '{other}'"))),
            };
            (t, 1)
        };
        for _ in 0..len {
            bump!();
        }
        push(&mut out, tok);
    }
    out.push(Token { tok: Tok::Eof, line, col });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn idents_lowercased_and_symbols() {
        assert_eq!(
            toks("V_Bal := v_bal + 1;"),
            vec![
                Tok::Ident("v_bal".into()),
                Tok::Assign,
                Tok::Ident("v_bal".into()),
                Tok::Plus,
                Tok::Int(1),
                Tok::Semi,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn strings_with_escape() {
        assert_eq!(toks("'it''s'"), vec![Tok::Str("it's".into()), Tok::Eof]);
        assert_eq!(toks("'Mixed Case'"), vec![Tok::Str("Mixed Case".into()), Tok::Eof]);
    }

    #[test]
    fn comments_skipped() {
        assert_eq!(
            toks("a -- comment\n /* multi\nline */ b"),
            vec![Tok::Ident("a".into()), Tok::Ident("b".into()), Tok::Eof]
        );
    }

    #[test]
    fn range_and_comparisons() {
        assert_eq!(
            toks("1..10 <> != <= >= < > || %"),
            vec![
                Tok::Int(1),
                Tok::DotDot,
                Tok::Int(10),
                Tok::Ne,
                Tok::Ne,
                Tok::Le,
                Tok::Ge,
                Tok::Lt,
                Tok::Gt,
                Tok::Concat,
                Tok::Percent,
                Tok::Eof
            ]
        );
    }

    #[test]
    fn positions() {
        let t = lex("a\n  bb").unwrap();
        assert_eq!((t[0].line, t[0].col), (1, 1));
        assert_eq!((t[1].line, t[1].col), (2, 3));
    }

    #[test]
    fn errors() {
        let e = lex("x := 'abc").unwrap_err();
        assert_eq!((e.line, e.col), (1, 6));
        assert!(lex("/* never closed").is_err());
        let e = lex("\n\n x := 1.5;").unwrap_err();
        assert_eq!(e.line, 3);
        assert!(lex("99999999999999999999").is_err());
        assert!(lex("a @ b").is_err());
    }
}
