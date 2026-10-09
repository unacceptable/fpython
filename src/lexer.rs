//! Indentation-aware tokenizer for fpython (a superset of Python 3.14 syntax).

use crate::diag::{Diag, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Name(String),
    Int(i64),
    BigInt(String),
    Float(f64),
    Imag(f64),
    /// String literal: prefix (lowercased, e.g. "", "r", "b", "f", "rb"), decoded/raw body.
    Str { prefix: String, body: String, raw_src: String },
    Op(&'static str),
    Newline,
    Indent,
    Dedent,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// Line on which the token ends (differs from span.line for multi-line strings).
    pub end_line: usize,
}

#[derive(Debug, Clone)]
pub struct Comment {
    pub line: usize,
    pub text: String,
    /// True when the comment follows code on the same line.
    pub trailing: bool,
}

pub const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

const OPS: &[&str] = &[
    "**=", "//=", ">>=", "<<=", "...", "->", ":=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "@=", "==", "!=", "<=", ">=", "**", "//", "<<", ">>", "+", "-", "*", "/", "%", "@", "&", "|",
    "^", "~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":", ".", ";", "=", "!",
];

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
    /// Lines (1-based) that are blank in the source.
    pub blank_lines: Vec<usize>,
}

pub fn lex(src: &str, file: &str) -> Result<Lexed, Diag> {
    let mut lx = Lexer {
        src: src.as_bytes(),
        text: src,
        pos: 0,
        line: 1,
        col: 1,
        file: file.to_string(),
        tokens: Vec::new(),
        comments: Vec::new(),
        blank_lines: Vec::new(),
        indents: vec![0],
        depth: 0,
        line_has_code: false,
    };
    lx.run()?;
    Ok(Lexed { tokens: lx.tokens, comments: lx.comments, blank_lines: lx.blank_lines })
}

struct Lexer<'a> {
    src: &'a [u8],
    text: &'a str,
    pos: usize,
    line: usize,
    col: usize,
    file: String,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    blank_lines: Vec<usize>,
    indents: Vec<usize>,
    depth: usize,
    line_has_code: bool,
}

impl<'a> Lexer<'a> {
    fn span(&self, line: usize, col: usize, start: usize) -> Span {
        Span { line, col, start, end: self.pos }
    }

    fn err(&self, msg: impl Into<String>) -> Diag {
        Diag::new(&self.file, Span { line: self.line, col: self.col, start: self.pos, end: self.pos }, msg)
    }

    fn peek(&self) -> u8 {
        *self.src.get(self.pos).unwrap_or(&0)
    }

    fn peek_at(&self, n: usize) -> u8 {
        *self.src.get(self.pos + n).unwrap_or(&0)
    }

    fn bump(&mut self) -> u8 {
        let c = self.src[self.pos];
        self.pos += 1;
        if c == b'\n' {
            self.line += 1;
            self.col = 1;
        } else if (c & 0xC0) != 0x80 {
            self.col += 1;
        }
        c
    }

    fn push(&mut self, tok: Tok, line: usize, col: usize, start: usize) {
        let span = self.span(line, col, start);
        let end_line = self.line - if self.pos > 0 && self.src[self.pos - 1] == b'\n' { 1 } else { 0 };
        self.tokens.push(Token { tok, span, end_line: end_line.max(line) });
    }

    fn run(&mut self) -> Result<(), Diag> {
        let mut at_line_start = true;
        loop {
            if at_line_start && self.depth == 0 {
                // Measure indentation.
                let mut width = 0usize;
                let start = self.pos;
                while self.pos < self.src.len() {
                    match self.peek() {
                        b' ' => width += 1,
                        b'\t' => width = (width / 8 + 1) * 8,
                        b'\x0c' => width = 0,
                        _ => break,
                    }
                    self.bump();
                }
                let c = self.peek();
                if c == b'\n' || c == b'\r' {
                    self.blank_lines.push(self.line);
                    if c == b'\r' {
                        self.bump();
                    }
                    if self.peek() == b'\n' {
                        self.bump();
                    }
                    continue;
                }
                if c == b'#' {
                    self.lex_comment(false);
                    continue;
                }
                if self.pos >= self.src.len() {
                    break;
                }
                let _ = start;
                let cur = *self.indents.last().unwrap();
                if width > cur {
                    self.indents.push(width);
                    self.push(Tok::Indent, self.line, 1, self.pos);
                } else if width < cur {
                    while width < *self.indents.last().unwrap() {
                        self.indents.pop();
                        self.push(Tok::Dedent, self.line, 1, self.pos);
                    }
                    if width != *self.indents.last().unwrap() {
                        return Err(self.err("unindent does not match any outer indentation level"));
                    }
                }
                at_line_start = false;
                self.line_has_code = false;
            }
            if self.pos >= self.src.len() {
                break;
            }
            let c = self.peek();
            match c {
                b' ' | b'\t' | b'\x0c' => {
                    self.bump();
                }
                b'\r' => {
                    self.bump();
                }
                b'\n' => {
                    let (l, co, s) = (self.line, self.col, self.pos);
                    self.bump();
                    if self.depth == 0 {
                        self.push(Tok::Newline, l, co, s);
                        at_line_start = true;
                    }
                }
                b'#' => self.lex_comment(true),
                b'\\' if self.peek_at(1) == b'\n' || (self.peek_at(1) == b'\r' && self.peek_at(2) == b'\n') => {
                    self.bump();
                    if self.peek() == b'\r' {
                        self.bump();
                    }
                    self.bump();
                }
                b'0'..=b'9' => self.lex_number()?,
                b'.' if self.peek_at(1).is_ascii_digit() => self.lex_number()?,
                b'"' | b'\'' => self.lex_string(String::new())?,
                _ if c == b'_' || c.is_ascii_alphabetic() || c >= 0x80 => {
                    let (l, co, s) = (self.line, self.col, self.pos);
                    while self.pos < self.src.len() {
                        let d = self.peek();
                        if d == b'_' || d.is_ascii_alphanumeric() || d >= 0x80 {
                            self.bump();
                        } else {
                            break;
                        }
                    }
                    let word = &self.text[s..self.pos];
                    let lower = word.to_ascii_lowercase();
                    if (self.peek() == b'"' || self.peek() == b'\'')
                        && word.len() <= 2
                        && lower.chars().all(|ch| matches!(ch, 'r' | 'b' | 'f' | 'u' | 't'))
                    {
                        let prefix = lower.replace('u', "");
                        self.pos = s;
                        self.col = co;
                        self.line = l;
                        for _ in 0..word.len() {
                            self.bump();
                        }
                        self.lex_string_at(prefix, l, co, s)?;
                    } else {
                        self.push(Tok::Name(word.to_string()), l, co, s);
                    }
                }
                _ => {
                    let (l, co, s) = (self.line, self.col, self.pos);
                    let rest = &self.text[self.pos..];
                    let op = OPS.iter().find(|o| rest.starts_with(**o));
                    match op {
                        Some(op) => {
                            for _ in 0..op.len() {
                                self.bump();
                            }
                            match *op {
                                "(" | "[" | "{" => self.depth += 1,
                                ")" | "]" | "}" => self.depth = self.depth.saturating_sub(1),
                                _ => {}
                            }
                            self.push(Tok::Op(op), l, co, s);
                        }
                        None => {
                            return Err(self.err(format!("unexpected character {:?}", rest.chars().next().unwrap())))
                        }
                    }
                }
            }
            if !at_line_start {
                self.line_has_code = true;
            }
        }
        if let Some(last) = self.tokens.last() {
            if !matches!(last.tok, Tok::Newline | Tok::Dedent) {
                self.push(Tok::Newline, self.line, self.col, self.pos);
            }
        }
        while self.indents.len() > 1 {
            self.indents.pop();
            self.push(Tok::Dedent, self.line, 1, self.pos);
        }
        self.push(Tok::Eof, self.line, self.col, self.pos);
        Ok(())
    }

    fn lex_comment(&mut self, maybe_trailing: bool) {
        let line = self.line;
        let s = self.pos;
        while self.pos < self.src.len() && self.peek() != b'\n' && self.peek() != b'\r' {
            self.bump();
        }
        let text = self.text[s..self.pos].trim_end().to_string();
        let trailing = maybe_trailing
            && self.tokens.last().map(|t| t.span.line == line && !matches!(t.tok, Tok::Indent | Tok::Dedent)).unwrap_or(false);
        self.comments.push(Comment { line, text, trailing });
    }

    fn lex_number(&mut self) -> Result<(), Diag> {
        let (l, co, s) = (self.line, self.col, self.pos);
        if self.peek() == b'0' && matches!(self.peek_at(1), b'x' | b'X' | b'o' | b'O' | b'b' | b'B') {
            let radix = match self.peek_at(1) {
                b'x' | b'X' => 16,
                b'o' | b'O' => 8,
                _ => 2,
            };
            self.bump();
            self.bump();
            let ds = self.pos;
            while self.peek().is_ascii_hexdigit() || self.peek() == b'_' {
                self.bump();
            }
            let digits: String = self.text[ds..self.pos].chars().filter(|c| *c != '_').collect();
            let tok = match i64::from_str_radix(&digits, radix) {
                Ok(v) => Tok::Int(v),
                Err(_) => Tok::BigInt(self.text[s..self.pos].to_ascii_lowercase().replace('_', "")),
            };
            self.push(tok, l, co, s);
            return Ok(());
        }
        let mut is_float = false;
        while self.peek().is_ascii_digit() || self.peek() == b'_' {
            self.bump();
        }
        if self.peek() == b'.' && self.peek_at(1) != b'.' {
            is_float = true;
            self.bump();
            while self.peek().is_ascii_digit() || self.peek() == b'_' {
                self.bump();
            }
        }
        if matches!(self.peek(), b'e' | b'E')
            && (self.peek_at(1).is_ascii_digit() || (matches!(self.peek_at(1), b'+' | b'-') && self.peek_at(2).is_ascii_digit()))
        {
            is_float = true;
            self.bump();
            if matches!(self.peek(), b'+' | b'-') {
                self.bump();
            }
            while self.peek().is_ascii_digit() {
                self.bump();
            }
        }
        let text: String = self.text[s..self.pos].chars().filter(|c| *c != '_').collect();
        if matches!(self.peek(), b'j' | b'J') {
            self.bump();
            let v: f64 = text.parse().map_err(|_| self.err("invalid imaginary literal"))?;
            self.push(Tok::Imag(v), l, co, s);
            return Ok(());
        }
        let tok = if is_float {
            Tok::Float(text.parse().map_err(|_| self.err("invalid float literal"))?)
        } else {
            match text.parse::<i64>() {
                Ok(v) => Tok::Int(v),
                Err(_) => Tok::BigInt(text),
            }
        };
        self.push(tok, l, co, s);
        Ok(())
    }

    fn lex_string(&mut self, prefix: String) -> Result<(), Diag> {
        let (l, co, s) = (self.line, self.col, self.pos);
        self.lex_string_at(prefix, l, co, s)
    }

    fn lex_string_at(&mut self, prefix: String, l: usize, co: usize, s: usize) -> Result<(), Diag> {
        let q = self.bump();
        let triple = self.peek() == q && self.peek_at(1) == q;
        if triple {
            self.bump();
            self.bump();
        }
        let raw = prefix.contains('r');
        let fmt = prefix.contains('f') || prefix.contains('t');
        let body_start = self.pos;
        let mut out = String::new();
        let mut fdepth = 0usize;
        loop {
            if self.pos >= self.src.len() {
                return Err(Diag::new(&self.file, Span { line: l, col: co, start: s, end: self.pos }, "unterminated string literal"));
            }
            let c = self.peek();
            if fmt {
                if c == b'{' {
                    if fdepth == 0 && self.peek_at(1) == b'{' {
                        out.push_str("{{");
                        self.bump();
                        self.bump();
                        continue;
                    }
                    fdepth += 1;
                } else if c == b'}' && fdepth > 0 {
                    fdepth -= 1;
                } else if fdepth > 0 && (c == b'"' || c == b'\'') {
                    // PEP 701: nested string inside a replacement field.
                    let nq = self.bump();
                    out.push(nq as char);
                    while self.pos < self.src.len() && self.peek() != nq && self.peek() != b'\n' {
                        let st = self.pos;
                        self.bump();
                        while self.pos < self.src.len() && (self.src[self.pos] & 0xC0) == 0x80 {
                            self.bump();
                        }
                        out.push_str(&self.text[st..self.pos]);
                    }
                    if self.pos < self.src.len() && self.peek() == nq {
                        self.bump();
                        out.push(nq as char);
                    }
                    continue;
                }
            }
            if c == q {
                if triple {
                    if self.peek_at(1) == q && self.peek_at(2) == q {
                        self.bump();
                        self.bump();
                        self.bump();
                        break;
                    }
                } else {
                    self.bump();
                    break;
                }
            }
            if c == b'\n' && !triple && fdepth == 0 {
                return Err(self.err("unterminated string literal (newline in single-quoted string)"));
            }
            if c == b'\\' {
                if raw || fmt {
                    // Keep escapes verbatim; f-strings are decoded per literal segment by the parser.
                    out.push(self.bump() as char);
                    if self.pos < self.src.len() {
                        let start = self.pos;
                        self.bump();
                        while self.pos < self.src.len() && (self.src[self.pos] & 0xC0) == 0x80 {
                            self.bump();
                        }
                        out.push_str(&self.text[start..self.pos]);
                    }
                    continue;
                }
                self.bump();
                let e = if self.pos < self.src.len() { self.peek() } else { 0 };
                self.decode_escape(e, &mut out, prefix.contains('b'))?;
                continue;
            }
            let start = self.pos;
            self.bump();
            while self.pos < self.src.len() && (self.src[self.pos] & 0xC0) == 0x80 {
                self.bump();
            }
            out.push_str(&self.text[start..self.pos]);
        }
        let _ = body_start;
        let raw_src = self.text[s..self.pos].to_string();
        self.push(Tok::Str { prefix, body: out, raw_src }, l, co, s);
        Ok(())
    }

    fn decode_escape(&mut self, e: u8, out: &mut String, bytes: bool) -> Result<(), Diag> {
        match e {
            b'\n' => {
                self.bump();
            }
            b'n' => { self.bump(); out.push('\n') }
            b't' => { self.bump(); out.push('\t') }
            b'r' => { self.bump(); out.push('\r') }
            b'0'..=b'7' => {
                let mut v = 0u32;
                let mut n = 0;
                while n < 3 && matches!(self.peek(), b'0'..=b'7') {
                    v = v * 8 + (self.bump() - b'0') as u32;
                    n += 1;
                }
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            b'a' => { self.bump(); out.push('\x07') }
            b'b' => { self.bump(); out.push('\x08') }
            b'f' => { self.bump(); out.push('\x0c') }
            b'v' => { self.bump(); out.push('\x0b') }
            b'\\' => { self.bump(); out.push('\\') }
            b'\'' => { self.bump(); out.push('\'') }
            b'"' => { self.bump(); out.push('"') }
            b'x' | b'u' | b'U' if !(bytes && e != b'x') => {
                self.bump();
                let n = match e { b'x' => 2, b'u' => 4, _ => 8 };
                let s = self.pos;
                for _ in 0..n {
                    if !self.peek().is_ascii_hexdigit() {
                        return Err(self.err("truncated escape sequence"));
                    }
                    self.bump();
                }
                let v = u32::from_str_radix(&self.text[s..self.pos], 16).unwrap();
                if v > 0x10FFFF {
                    return Err(self.err("invalid unicode escape"));
                }
                // Lone surrogates cannot be represented in UTF-8 strings; map to U+FFFD.
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            b'N' => return Err(self.err("\\N{...} escapes are not supported yet")),
            _ => out.push('\\'),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexer_errors() {
        let cases = [
            ("if x:\n        a\n    b\n", "unindent does not match"),
            ("x = $\n", "unexpected character"),
            ("s = '''abc\n", "unterminated"),
            ("s = 'abc\n", "newline in single-quoted string"),
            ("s = '\\x4'\n", "truncated escape"),
            ("s = '\\u12'\n", "escape"),
            ("s = '\\N{BULLET}'\n", "\\N{...}"),
        ];
        let mut bad = Vec::new();
        for (src, want) in cases {
            match lex(src, "t.fpy") {
                Ok(_) => bad.push(format!("{:?}: lexed", src)),
                Err(d) if !d.msg.contains(want) => bad.push(format!("{:?}: got {:?}, want {:?}", src, d.msg, want)),
                _ => {}
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn whitespace_forms() {
        // CRLF line endings, form feeds, backslash continuations (LF and CRLF), tabs
        let l = lex("x = 1 + \\\n    2\r\ny = 3 + \\\r\n 4\r\n\x0cz = 5\nif z:\n\tw = 1\n", "t").unwrap();
        let names: Vec<String> = l.tokens.iter().filter_map(|t| if let Tok::Name(n) = &t.tok { Some(n.clone()) } else { None }).collect();
        assert_eq!(names, vec!["x", "y", "z", "if", "z", "w"]);
        assert!(lex("x = 99999999999999999999999999\n", "t").unwrap().tokens.iter().any(|t| matches!(t.tok, Tok::BigInt(_))));
        let s = lex("s = '\\a\\v\\101\\\nend' f'{{x}}' f'{x:{w}}'\n", "t").unwrap();
        assert!(s.tokens.iter().any(|t| matches!(&t.tok, Tok::Str { body: v, .. } if v.contains('\x07') && v.contains('\x0b') && v.contains('A'))));
    }
}
