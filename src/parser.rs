//! Recursive-descent parser covering the Python 3.14 grammar (plus fpython's typing rules,
//! which are enforced later by the checker rather than by syntax).

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::lexer::{lex, Comment, Tok, Token, KEYWORDS};

pub struct Parsed {
    pub module: Module,
    pub comments: Vec<Comment>,
    pub blank_lines: Vec<usize>,
}

pub fn parse(src: &str, file: &str) -> Result<Parsed, Diag> {
    let lexed = lex(src, file)?;
    let mut p = Parser { toks: lexed.tokens, pos: 0, file: file.to_string(), no_in: false, first_colon: None };
    let module = p.parse_module()?;
    Ok(Parsed { module, comments: lexed.comments, blank_lines: lexed.blank_lines })
}

/// Parse a standalone expression (used for f-string interpolations).
pub fn parse_expr_str(src: &str, file: &str, base: Span) -> Result<Expr, Diag> {
    let lexed = lex(src.trim(), file).map_err(|mut d| {
        d.span = base;
        d
    })?;
    let mut toks = lexed.tokens;
    for t in toks.iter_mut() {
        t.span = base;
    }
    let mut p = Parser { toks, pos: 0, file: file.to_string(), no_in: false, first_colon: None };
    let e = p.parse_star_exprs_tuple()?;
    while matches!(p.peek(), Tok::Newline) {
        p.pos += 1;
    }
    if !matches!(p.peek(), Tok::Eof) {
        return Err(p.err("invalid expression in f-string"));
    }
    Ok(e)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    file: String,
    no_in: bool,
    first_colon: Option<usize>,
}

type PResult<T> = Result<T, Diag>;

fn is_keyword(s: &str) -> bool {
    KEYWORDS.contains(&s)
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek_n(&self, n: usize) -> &Tok {
        let i = (self.pos + n).min(self.toks.len() - 1);
        &self.toks[i].tok
    }
    fn span(&self) -> Span {
        self.toks[self.pos].span
    }
    fn prev_span(&self) -> Span {
        self.toks[self.pos.saturating_sub(1)].span
    }
    fn prev_end_line(&self) -> usize {
        let t = &self.toks[self.pos.saturating_sub(1)];
        // NEWLINE/DEDENT tokens sit on the following line; use the last real token instead
        let mut i = self.pos.saturating_sub(1);
        while i > 0 && matches!(self.toks[i].tok, Tok::Newline | Tok::Dedent | Tok::Indent | Tok::Eof) {
            i -= 1;
        }
        let _ = t;
        self.toks[i].end_line
    }

    fn join(&self, start: Span) -> Span {
        let end = self.prev_span();
        Span { line: start.line, col: start.col, start: start.start, end: end.end.max(start.start) }
    }
    fn err(&self, msg: impl Into<String>) -> Diag {
        Diag::new(&self.file, self.span(), msg)
    }
    fn advance(&mut self) -> Tok {
        let t = self.toks[self.pos].tok.clone();
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
        t
    }
    fn is_op(&self, op: &str) -> bool {
        matches!(self.peek(), Tok::Op(o) if *o == op)
    }
    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Name(n) if n == kw)
    }
    fn eat_op(&mut self, op: &str) -> bool {
        if self.is_op(op) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_op(&mut self, op: &str) -> PResult<()> {
        if self.eat_op(op) {
            Ok(())
        } else {
            Err(self.err(format!("expected '{}', found {}", op, self.describe())))
        }
    }
    fn expect_kw(&mut self, kw: &str) -> PResult<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err(format!("expected '{}', found {}", kw, self.describe())))
        }
    }
    fn describe(&self) -> String {
        match self.peek() {
            Tok::Name(n) => format!("'{}'", n),
            Tok::Int(i) => format!("'{}'", i),
            Tok::BigInt(s) => format!("'{}'", s),
            Tok::Float(f) => format!("'{}'", f),
            Tok::Imag(f) => format!("'{}j'", f),
            Tok::Str { .. } => "string literal".into(),
            Tok::Op(o) => format!("'{}'", o),
            Tok::Newline => "end of line".into(),
            Tok::Indent => "indent".into(),
            Tok::Dedent => "dedent".into(),
            Tok::Eof => "end of file".into(),
        }
    }
    fn ident(&mut self) -> PResult<String> {
        match self.peek().clone() {
            Tok::Name(n) if !is_keyword(&n) => {
                self.advance();
                Ok(n)
            }
            _ => Err(self.err(format!("expected identifier, found {}", self.describe()))),
        }
    }

    // ---------------------------------------------------------------- statements

    fn parse_module(&mut self) -> PResult<Module> {
        let mut body = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            if matches!(self.peek(), Tok::Newline) {
                self.advance();
                continue;
            }
            body.extend(self.parse_statement()?);
        }
        Ok(Module { body })
    }

    fn parse_block(&mut self) -> PResult<Vec<Stmt>> {
        if self.first_colon.is_none() {
            self.first_colon = Some(self.span().line);
        }
        self.expect_op(":")?;
        if matches!(self.peek(), Tok::Newline) {
            self.advance();
            if !matches!(self.peek(), Tok::Indent) {
                return Err(self.err("expected an indented block"));
            }
            self.advance();
            let mut body = Vec::new();
            while !matches!(self.peek(), Tok::Dedent | Tok::Eof) {
                if matches!(self.peek(), Tok::Newline) {
                    self.advance();
                    continue;
                }
                body.extend(self.parse_statement()?);
            }
            if matches!(self.peek(), Tok::Dedent) {
                self.advance();
            }
            Ok(body)
        } else {
            self.parse_simple_line()
        }
    }

    fn parse_statement(&mut self) -> PResult<Vec<Stmt>> {
        let saved_colon = self.first_colon.take();
        let r = self.parse_statement_inner();
        self.first_colon = saved_colon;
        r
    }

    fn parse_statement_inner(&mut self) -> PResult<Vec<Stmt>> {
        let start = self.span();
        let kw = if let Tok::Name(n) = self.peek() { n.clone() } else { String::new() };
        let kind = match kw.as_str() {
            "if" => {
                self.advance();
                self.parse_if()?
            }
            "while" => {
                self.advance();
                let test = self.parse_named_expr()?;
                let body = self.parse_block()?;
                let orelse = self.parse_else()?;
                StmtKind::While { test, body, orelse }
            }
            "for" => {
                self.advance();
                self.parse_for(false)?
            }
            "try" => {
                self.advance();
                self.parse_try()?
            }
            "with" => {
                self.advance();
                self.parse_with(false)?
            }
            "def" => {
                self.advance();
                StmtKind::FuncDef(Box::new(self.parse_funcdef(vec![], false)?))
            }
            "class" => {
                self.advance();
                StmtKind::ClassDef(Box::new(self.parse_classdef(vec![])?))
            }
            "async" => {
                self.advance();
                if self.eat_kw("def") {
                    StmtKind::FuncDef(Box::new(self.parse_funcdef(vec![], true)?))
                } else if self.eat_kw("for") {
                    self.parse_for(true)?
                } else if self.eat_kw("with") {
                    self.parse_with(true)?
                } else {
                    return Err(self.err("expected 'def', 'for' or 'with' after 'async'"));
                }
            }
            "match" if self.looks_like_match() => {
                self.advance();
                self.parse_match()?
            }
            _ if self.is_op("@") => {
                let mut decorators = Vec::new();
                while self.eat_op("@") {
                    decorators.push(self.parse_named_expr()?);
                    if !matches!(self.peek(), Tok::Newline) {
                        return Err(self.err("expected newline after decorator"));
                    }
                    self.advance();
                }
                if self.eat_kw("def") {
                    StmtKind::FuncDef(Box::new(self.parse_funcdef(decorators, false)?))
                } else if self.eat_kw("async") {
                    self.expect_kw("def")?;
                    StmtKind::FuncDef(Box::new(self.parse_funcdef(decorators, true)?))
                } else if self.eat_kw("class") {
                    StmtKind::ClassDef(Box::new(self.parse_classdef(decorators)?))
                } else {
                    return Err(self.err("expected 'def' or 'class' after decorator"));
                }
            }
            _ => return self.parse_simple_line(),
        };
        let end_line = self.prev_end_line();
        let header_end = self.first_colon.unwrap_or(end_line);
        Ok(vec![Stmt { kind, span: start, end_line, header_end }])
    }

    fn looks_like_match(&self) -> bool {
        // `match` is a soft keyword: it starts a match statement only if the line ends in ':'
        // followed by a newline and an indented block of `case` clauses.
        match self.peek_n(1) {
            Tok::Op(o) if matches!(*o, "=" | "." | ")" | "," | ":" | "+=" | "-=") => return false,
            Tok::Newline | Tok::Eof => return false,
            _ => {}
        }
        let mut i = self.pos + 1;
        let mut depth = 0i32;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::Op(o) if matches!(*o, "(" | "[" | "{") => depth += 1,
                Tok::Op(o) if matches!(*o, ")" | "]" | "}") => depth -= 1,
                Tok::Op(":") if depth == 0 => {
                    return matches!(self.toks.get(i + 1).map(|t| &t.tok), Some(Tok::Newline))
                        && matches!(self.toks.get(i + 2).map(|t| &t.tok), Some(Tok::Indent))
                        && matches!(self.toks.get(i + 3).map(|t| &t.tok), Some(Tok::Name(n)) if n == "case");
                }
                Tok::Newline | Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_if(&mut self) -> PResult<StmtKind> {
        let test = self.parse_named_expr()?;
        let body = self.parse_block()?;
        let orelse = if self.is_kw("elif") {
            let start = self.span();
            self.advance();
            let k = self.parse_if()?;
            vec![Stmt { kind: k, span: start, end_line: self.prev_end_line(), header_end: start.line }]
        } else {
            self.parse_else()?
        };
        Ok(StmtKind::If { test, body, orelse })
    }

    fn parse_else(&mut self) -> PResult<Vec<Stmt>> {
        if self.eat_kw("else") {
            self.parse_block()
        } else {
            Ok(vec![])
        }
    }

    fn parse_for(&mut self, is_async: bool) -> PResult<StmtKind> {
        let target = self.parse_target_list()?;
        self.expect_kw("in")?;
        let iter = self.parse_star_exprs_tuple()?;
        let body = self.parse_block()?;
        let orelse = self.parse_else()?;
        Ok(StmtKind::For { target, iter, body, orelse, is_async })
    }

    fn parse_try(&mut self) -> PResult<StmtKind> {
        let body = self.parse_block()?;
        let mut handlers = Vec::new();
        let mut star = false;
        while self.is_kw("except") {
            let hs = self.span();
            self.advance();
            if self.eat_op("*") {
                star = true;
            }
            let (typ, name) = if self.is_op(":") {
                (None, None)
            } else {
                // Python 3.14 (PEP 758) allows `except A, B:` without parentheses.
                let first = self.parse_expr()?;
                let typ = if self.is_op(",") {
                    let mut elts = vec![first];
                    while self.eat_op(",") {
                        elts.push(self.parse_expr()?);
                    }
                    Expr::new(ExprKind::Tuple { elts, parens: false }, hs)
                } else {
                    first
                };
                let name = if self.eat_kw("as") { Some(self.ident()?) } else { None };
                (Some(typ), name)
            };
            let header_end = self.span().line;
            let hbody = self.parse_block()?;
            handlers.push(ExceptHandler { typ, name, body: hbody, span: hs, header_end });
        }
        let orelse = self.parse_else()?;
        let finalbody = if self.eat_kw("finally") { self.parse_block()? } else { vec![] };
        if handlers.is_empty() && finalbody.is_empty() {
            return Err(self.err("expected 'except' or 'finally' block"));
        }
        Ok(StmtKind::Try { body, handlers, orelse, finalbody, star })
    }

    fn parse_with(&mut self, is_async: bool) -> PResult<StmtKind> {
        let mut items = Vec::new();
        // Parenthesized with-items: `with (a as b, c as d):`
        let paren = self.is_op("(") && self.paren_with_items();
        if paren {
            self.advance();
        }
        loop {
            let ctx = self.parse_expr()?;
            let var = if self.eat_kw("as") { Some(self.parse_target()?) } else { None };
            items.push(WithItem { ctx, var });
            if !self.eat_op(",") {
                break;
            }
            if paren && self.is_op(")") {
                break;
            }
        }
        if paren {
            self.expect_op(")")?;
        }
        let body = self.parse_block()?;
        Ok(StmtKind::With { items, body, is_async })
    }

    fn paren_with_items(&self) -> bool {
        // Find matching ')' and check that ':' follows it directly and an `as` appears at depth 1.
        let mut depth = 0;
        let mut saw_as = false;
        let mut i = self.pos;
        while i < self.toks.len() {
            match &self.toks[i].tok {
                Tok::Op("(") | Tok::Op("[") | Tok::Op("{") => depth += 1,
                Tok::Op(")") | Tok::Op("]") | Tok::Op("}") => {
                    depth -= 1;
                    if depth == 0 {
                        return saw_as && matches!(self.toks.get(i + 1).map(|t| &t.tok), Some(Tok::Op(":")));
                    }
                }
                Tok::Name(n) if n == "as" && depth == 1 => saw_as = true,
                Tok::Newline | Tok::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_type_params(&mut self) -> PResult<Vec<TypeParam>> {
        let mut tps = Vec::new();
        if self.eat_op("[") {
            while !self.is_op("]") {
                self.eat_op("*");
                self.eat_op("**");
                let name = self.ident()?;
                let bound = if self.eat_op(":") { Some(self.parse_expr()?) } else { None };
                if self.eat_op("=") {
                    self.parse_expr()?;
                }
                tps.push(TypeParam { name, bound });
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op("]")?;
        }
        Ok(tps)
    }

    fn parse_funcdef(&mut self, decorators: Vec<Expr>, is_async: bool) -> PResult<FuncDef> {
        let def_line = self.prev_span().line;
        let name = self.ident()?;
        let type_params = self.parse_type_params()?;
        self.expect_op("(")?;
        let params = self.parse_params(")", true)?;
        let close_paren_pos = self.span().start;
        self.expect_op(")")?;
        let returns = if self.eat_op("->") { Some(self.parse_expr()?) } else { None };
        let colon_line = self.span().line;
        let colon_pos = self.span().start;
        let body = self.parse_block()?;
        Ok(FuncDef { name, type_params, params, returns, body, decorators, is_async, header_lines: (def_line, colon_line), close_paren_pos, colon_pos })
    }

    fn parse_params(&mut self, close: &str, annotations: bool) -> PResult<Params> {
        let mut p = Params::default();
        let mut seen_star = false;
        while !self.is_op(close) {
            let s = self.span();
            if self.eat_op("/") {
                p.posonly.append(&mut p.args);
            } else if self.eat_op("**") {
                let name = self.ident()?;
                let ann = if annotations && self.eat_op(":") { Some(self.parse_expr()?) } else { None };
                p.kwarg = Some(Arg { name, ann, default: None, span: s });
            } else if self.eat_op("*") {
                seen_star = true;
                if !self.is_op(",") && !self.is_op(close) {
                    let name = self.ident()?;
                    let ann = if annotations && self.eat_op(":") {
                        if self.is_op("*") {
                            self.advance();
                        }
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    p.vararg = Some(Arg { name, ann, default: None, span: s });
                }
            } else {
                let name = self.ident()?;
                let ann = if annotations && self.eat_op(":") { Some(self.parse_expr()?) } else { None };
                let default = if self.eat_op("=") { Some(self.parse_expr()?) } else { None };
                let arg = Arg { name, ann, default, span: s };
                if seen_star {
                    p.kwonly.push(arg);
                } else {
                    p.args.push(arg);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        Ok(p)
    }

    fn parse_classdef(&mut self, decorators: Vec<Expr>) -> PResult<ClassDef> {
        let def_line = self.prev_span().line;
        let name = self.ident()?;
        let type_params = self.parse_type_params()?;
        let mut bases = Vec::new();
        let mut keywords = Vec::new();
        if self.eat_op("(") {
            let (a, k) = self.parse_call_args()?;
            bases = a;
            keywords = k;
        }
        let colon_line = self.span().line;
        let body = self.parse_block()?;
        Ok(ClassDef { name, type_params, bases, keywords, body, decorators, header_lines: (def_line, colon_line) })
    }

    fn parse_match(&mut self) -> PResult<StmtKind> {
        let subject = self.parse_star_exprs_tuple()?;
        if self.first_colon.is_none() {
            self.first_colon = Some(self.span().line);
        }
        self.expect_op(":")?;
        // looks_like_match() guarantees NEWLINE INDENT `case` follows the colon
        self.advance();
        self.advance();
        let mut cases = Vec::new();
        while self.is_kw("case") {
            let line = self.span().line;
            self.advance();
            let pattern = self.parse_pattern_top()?;
            let guard = if self.eat_kw("if") { Some(self.parse_named_expr()?) } else { None };
            let body = self.parse_block()?;
            cases.push(MatchCase { line, pattern, guard, body });
            while matches!(self.peek(), Tok::Newline) {
                self.advance();
            }
        }
        if !matches!(self.peek(), Tok::Dedent | Tok::Eof) {
            return Err(self.err("expected 'case'"));
        }
        if matches!(self.peek(), Tok::Dedent) {
            self.advance();
        }
        Ok(StmtKind::Match { subject, cases })
    }

    fn parse_pattern_top(&mut self) -> PResult<Pattern> {
        let first = self.parse_pattern_as()?;
        if self.is_op(",") {
            let mut pats = vec![first];
            while self.eat_op(",") {
                if self.is_op(":") || self.is_kw("if") {
                    break;
                }
                pats.push(self.parse_pattern_as()?);
            }
            return Ok(Pattern::Sequence(pats, true));
        }
        Ok(first)
    }

    fn parse_pattern_as(&mut self) -> PResult<Pattern> {
        let mut alts = vec![self.parse_pattern_closed()?];
        while self.eat_op("|") {
            alts.push(self.parse_pattern_closed()?);
        }
        let pat = if alts.len() == 1 { alts.pop().unwrap() } else { Pattern::Or(alts) };
        if self.eat_kw("as") {
            let name = self.ident()?;
            return Ok(Pattern::As(Box::new(pat), name));
        }
        Ok(pat)
    }

    fn parse_pattern_closed(&mut self) -> PResult<Pattern> {
        if self.eat_op("*") {
            let n = self.ident()?;
            return Ok(Pattern::Star(if n == "_" { None } else { Some(n) }));
        }
        if self.eat_op("(") {
            if self.eat_op(")") {
                return Ok(Pattern::Sequence(vec![], true));
            }
            let first = self.parse_pattern_as()?;
            if self.is_op(",") {
                let mut pats = vec![first];
                while self.eat_op(",") {
                    if self.is_op(")") {
                        break;
                    }
                    pats.push(self.parse_pattern_as()?);
                }
                self.expect_op(")")?;
                return Ok(Pattern::Sequence(pats, true));
            }
            self.expect_op(")")?;
            return Ok(first);
        }
        if self.eat_op("[") {
            let mut pats = Vec::new();
            while !self.is_op("]") {
                pats.push(self.parse_pattern_as()?);
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op("]")?;
            return Ok(Pattern::Sequence(pats, false));
        }
        if self.eat_op("{") {
            let mut keys = Vec::new();
            let mut values = Vec::new();
            let mut rest = None;
            while !self.is_op("}") {
                if self.eat_op("**") {
                    rest = Some(self.ident()?);
                } else {
                    keys.push(self.parse_pattern_value_expr()?);
                    self.expect_op(":")?;
                    values.push(self.parse_pattern_as()?);
                }
                if !self.eat_op(",") {
                    break;
                }
            }
            self.expect_op("}")?;
            return Ok(Pattern::Mapping { keys, values, rest });
        }
        if let Tok::Name(n) = self.peek().clone() {
            if !is_keyword(&n) && !matches!(self.peek_n(1), Tok::Op(".") | Tok::Op("(")) {
                self.advance();
                return Ok(if n == "_" { Pattern::Wildcard } else { Pattern::Capture(n) });
            }
            if !is_keyword(&n) {
                // Dotted name: value pattern or class pattern.
                let start = self.span();
                let mut e = Expr::new(ExprKind::Name(self.ident()?), start);
                while self.eat_op(".") {
                    let attr = self.ident()?;
                    e = Expr::new(ExprKind::Attribute { value: Box::new(e), attr }, self.join(start));
                }
                if self.eat_op("(") {
                    let mut args = Vec::new();
                    let mut kwargs = Vec::new();
                    while !self.is_op(")") {
                        if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek_n(1), Tok::Op("=")) {
                            let k = self.ident()?;
                            self.expect_op("=")?;
                            kwargs.push((k, self.parse_pattern_as()?));
                        } else {
                            args.push(self.parse_pattern_as()?);
                        }
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    self.expect_op(")")?;
                    return Ok(Pattern::Class { cls: e, args, kwargs });
                }
                return Ok(Pattern::Value(e));
            }
        }
        Ok(Pattern::Value(self.parse_pattern_value_expr()?))
    }

    fn parse_pattern_value_expr(&mut self) -> PResult<Expr> {
        // Literals, negative numbers, and simple arithmetic for complex-like literals.
        self.parse_arith()
    }

    /// One logical line of simple statements separated by ';'.
    fn parse_simple_line(&mut self) -> PResult<Vec<Stmt>> {
        let mut out = Vec::new();
        loop {
            let start = self.span();
            let kind = self.parse_simple_stmt()?;
            let end_line = self.prev_end_line();
            out.push(Stmt { kind, span: self.join(start), end_line, header_end: end_line });
            if self.eat_op(";") {
                if matches!(self.peek(), Tok::Newline) {
                    break;
                }
                continue;
            }
            break;
        }
        match self.peek() {
            Tok::Newline => {
                self.advance();
            }
            Tok::Eof | Tok::Dedent => {}
            _ => return Err(self.err(format!("expected end of line, found {}", self.describe()))),
        }
        Ok(out)
    }

    fn parse_simple_stmt(&mut self) -> PResult<StmtKind> {
        let kw = if let Tok::Name(n) = self.peek() { n.clone() } else { String::new() };
        match kw.as_str() {
            "pass" => {
                self.advance();
                Ok(StmtKind::Pass)
            }
            "break" => {
                self.advance();
                Ok(StmtKind::Break)
            }
            "continue" => {
                self.advance();
                Ok(StmtKind::Continue)
            }
            "return" => {
                self.advance();
                if self.at_stmt_end() {
                    Ok(StmtKind::Return(None))
                } else {
                    Ok(StmtKind::Return(Some(self.parse_star_exprs_tuple()?)))
                }
            }
            "raise" => {
                self.advance();
                if self.at_stmt_end() {
                    return Ok(StmtKind::Raise { exc: None, cause: None });
                }
                let exc = Some(self.parse_expr()?);
                let cause = if self.eat_kw("from") { Some(self.parse_expr()?) } else { None };
                Ok(StmtKind::Raise { exc, cause })
            }
            "global" | "nonlocal" => {
                self.advance();
                let mut names = vec![self.ident()?];
                while self.eat_op(",") {
                    names.push(self.ident()?);
                }
                Ok(if kw == "global" { StmtKind::Global(names) } else { StmtKind::Nonlocal(names) })
            }
            "del" => {
                self.advance();
                let mut targets = vec![self.parse_target()?];
                while self.eat_op(",") {
                    if self.at_stmt_end() {
                        break;
                    }
                    targets.push(self.parse_target()?);
                }
                Ok(StmtKind::Del(targets))
            }
            "assert" => {
                self.advance();
                let test = self.parse_expr()?;
                let msg = if self.eat_op(",") { Some(self.parse_expr()?) } else { None };
                Ok(StmtKind::Assert { test, msg })
            }
            "import" => {
                self.advance();
                let mut names = Vec::new();
                loop {
                    let name = self.dotted_name()?;
                    let asname = if self.eat_kw("as") { Some(self.ident()?) } else { None };
                    names.push(Alias { name, asname });
                    if !self.eat_op(",") {
                        break;
                    }
                }
                Ok(StmtKind::Import(names))
            }
            "from" => {
                self.advance();
                let mut level = 0;
                loop {
                    if self.eat_op(".") {
                        level += 1;
                    } else if self.eat_op("...") {
                        level += 3;
                    } else {
                        break;
                    }
                }
                let module = if self.is_kw("import") { String::new() } else { self.dotted_name()? };
                self.expect_kw("import")?;
                let mut names = Vec::new();
                if self.eat_op("*") {
                    names.push(Alias { name: "*".into(), asname: None });
                } else {
                    let paren = self.eat_op("(");
                    loop {
                        if paren && self.is_op(")") {
                            break;
                        }
                        let name = self.ident()?;
                        let asname = if self.eat_kw("as") { Some(self.ident()?) } else { None };
                        names.push(Alias { name, asname });
                        if !self.eat_op(",") {
                            break;
                        }
                    }
                    if paren {
                        self.expect_op(")")?;
                    }
                }
                Ok(StmtKind::ImportFrom { module, level, names })
            }
            "type" if matches!(self.peek_n(1), Tok::Name(_)) && matches!(self.peek_n(2), Tok::Op("=") | Tok::Op("[")) => {
                self.advance();
                let name = self.ident()?;
                let type_params = self.parse_type_params()?;
                self.expect_op("=")?;
                let value = self.parse_expr()?;
                Ok(StmtKind::TypeAlias { name, type_params, value })
            }
            _ => self.parse_expr_stmt(),
        }
    }

    fn at_stmt_end(&self) -> bool {
        matches!(self.peek(), Tok::Newline | Tok::Eof | Tok::Dedent) || self.is_op(";")
    }

    fn dotted_name(&mut self) -> PResult<String> {
        let mut s = self.ident()?;
        while self.eat_op(".") {
            s.push('.');
            s.push_str(&self.ident()?);
        }
        Ok(s)
    }

    fn parse_expr_stmt(&mut self) -> PResult<StmtKind> {
        let first = self.parse_star_exprs_tuple()?;
        if self.eat_op(":") {
            let ann = self.parse_expr()?;
            let value = if self.eat_op("=") { Some(self.parse_assign_rhs()?) } else { None };
            return Ok(StmtKind::AnnAssign { target: first, ann, value });
        }
        let aug = match self.peek() {
            Tok::Op(o) => match *o {
                "+=" => Some(BinOp::Add),
                "-=" => Some(BinOp::Sub),
                "*=" => Some(BinOp::Mul),
                "/=" => Some(BinOp::Div),
                "//=" => Some(BinOp::FloorDiv),
                "%=" => Some(BinOp::Mod),
                "**=" => Some(BinOp::Pow),
                "@=" => Some(BinOp::MatMul),
                "<<=" => Some(BinOp::LShift),
                ">>=" => Some(BinOp::RShift),
                "&=" => Some(BinOp::BitAnd),
                "|=" => Some(BinOp::BitOr),
                "^=" => Some(BinOp::BitXor),
                _ => None,
            },
            _ => None,
        };
        if let Some(op) = aug {
            self.advance();
            let value = self.parse_assign_rhs()?;
            return Ok(StmtKind::AugAssign { target: first, op, value });
        }
        if self.is_op("=") {
            let mut targets = vec![first];
            let mut value;
            loop {
                self.expect_op("=")?;
                value = self.parse_assign_rhs()?;
                if self.is_op("=") {
                    targets.push(value);
                } else {
                    break;
                }
            }
            return Ok(StmtKind::Assign { targets, value });
        }
        Ok(StmtKind::Expr(first))
    }

    fn parse_assign_rhs(&mut self) -> PResult<Expr> {
        if self.is_kw("yield") {
            return self.parse_yield();
        }
        self.parse_star_exprs_tuple()
    }

    fn parse_yield(&mut self) -> PResult<Expr> {
        let start = self.span();
        self.expect_kw("yield")?;
        if self.eat_kw("from") {
            let e = self.parse_expr()?;
            return Ok(Expr::new(ExprKind::YieldFrom(Box::new(e)), self.join(start)));
        }
        if self.at_stmt_end() || self.is_op(")") || self.is_op("=") {
            return Ok(Expr::new(ExprKind::Yield(None), self.join(start)));
        }
        let e = self.parse_star_exprs_tuple()?;
        Ok(Expr::new(ExprKind::Yield(Some(Box::new(e))), self.join(start)))
    }

    // ---------------------------------------------------------------- expressions

    /// Comma-separated expressions forming a tuple if more than one (or trailing comma).
    fn parse_star_exprs_tuple(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.is_kw("yield") {
            return self.parse_yield();
        }
        let first = self.parse_star_or_named()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.expr_can_start() {
                elts.push(self.parse_star_or_named()?);
            } else {
                break;
            }
        }
        Ok(Expr::new(ExprKind::Tuple { elts, parens: false }, self.join(start)))
    }

    fn expr_can_start(&self) -> bool {
        match self.peek() {
            Tok::Name(n) => !is_keyword(n) || matches!(n.as_str(), "not" | "lambda" | "None" | "True" | "False" | "await" | "yield"),
            Tok::Int(_) | Tok::BigInt(_) | Tok::Float(_) | Tok::Imag(_) | Tok::Str { .. } => true,
            Tok::Op(o) => matches!(*o, "(" | "[" | "{" | "-" | "+" | "~" | "*" | "..." | "**"),
            _ => false,
        }
    }

    fn parse_star_or_named(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat_op("*") {
            let e = self.parse_bitor()?;
            return Ok(Expr::new(ExprKind::Starred(Box::new(e)), self.join(start)));
        }
        self.parse_named_expr()
    }

    fn parse_named_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        if let Tok::Name(n) = self.peek().clone() {
            if !is_keyword(&n) && matches!(self.peek_n(1), Tok::Op(":=")) {
                self.advance();
                self.advance();
                let value = self.parse_expr()?;
                return Ok(Expr::new(ExprKind::NamedExpr { target: n, value: Box::new(value) }, self.join(start)));
            }
        }
        self.parse_expr()
    }

    /// `expression` in the Python grammar: ternary / lambda.
    pub fn parse_expr(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.is_kw("lambda") {
            self.advance();
            let params = self.parse_params(":", false)?;
            self.expect_op(":")?;
            let body = self.parse_expr()?;
            return Ok(Expr::new(ExprKind::Lambda { params: Box::new(params), body: Box::new(body) }, self.join(start)));
        }
        let body = self.parse_or()?;
        if self.is_kw("if") {
            // Ternary `a if cond else b` — but not inside comprehensions where `if` is a filter.
            let save = self.pos;
            self.advance();
            let test = self.parse_or()?;
            if self.eat_kw("else") {
                let orelse = self.parse_expr()?;
                return Ok(Expr::new(
                    ExprKind::IfExp { test: Box::new(test), body: Box::new(body), orelse: Box::new(orelse) },
                    self.join(start),
                ));
            }
            self.pos = save;
        }
        Ok(body)
    }

    fn parse_expr_no_cond(&mut self) -> PResult<Expr> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let start = self.span();
        let first = self.parse_and()?;
        if !self.is_kw("or") {
            return Ok(first);
        }
        let mut values = vec![first];
        while self.eat_kw("or") {
            values.push(self.parse_and()?);
        }
        Ok(Expr::new(ExprKind::BoolOp { op: BoolOp::Or, values }, self.join(start)))
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let start = self.span();
        let first = self.parse_not()?;
        if !self.is_kw("and") {
            return Ok(first);
        }
        let mut values = vec![first];
        while self.eat_kw("and") {
            values.push(self.parse_not()?);
        }
        Ok(Expr::new(ExprKind::BoolOp { op: BoolOp::And, values }, self.join(start)))
    }

    fn parse_not(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat_kw("not") {
            let e = self.parse_not()?;
            return Ok(Expr::new(ExprKind::UnaryOp { op: UnOp::Not, operand: Box::new(e) }, self.join(start)));
        }
        self.parse_comparison()
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let start = self.span();
        let left = self.parse_bitor()?;
        let mut ops = Vec::new();
        let mut comparators = Vec::new();
        loop {
            let op = match self.peek() {
                Tok::Op("==") => CmpOp::Eq,
                Tok::Op("!=") => CmpOp::NotEq,
                Tok::Op("<") => CmpOp::Lt,
                Tok::Op("<=") => CmpOp::LtE,
                Tok::Op(">") => CmpOp::Gt,
                Tok::Op(">=") => CmpOp::GtE,
                Tok::Name(n) if n == "in" && !self.no_in => CmpOp::In,
                Tok::Name(n) if n == "is" => {
                    if matches!(self.peek_n(1), Tok::Name(m) if m == "not") {
                        self.advance();
                        CmpOp::IsNot
                    } else {
                        CmpOp::Is
                    }
                }
                Tok::Name(n) if n == "not" && matches!(self.peek_n(1), Tok::Name(m) if m == "in") => {
                    self.advance();
                    CmpOp::NotIn
                }
                _ => break,
            };
            self.advance();
            ops.push(op);
            comparators.push(self.parse_bitor()?);
        }
        if ops.is_empty() {
            return Ok(left);
        }
        Ok(Expr::new(ExprKind::Compare { left: Box::new(left), ops, comparators }, self.join(start)))
    }

    fn binop_at(&self, level: u8) -> Option<BinOp> {
        let op = match self.peek() {
            Tok::Op(o) => match *o {
                "|" => BinOp::BitOr,
                "^" => BinOp::BitXor,
                "&" => BinOp::BitAnd,
                "<<" => BinOp::LShift,
                ">>" => BinOp::RShift,
                "+" => BinOp::Add,
                "-" => BinOp::Sub,
                "*" => BinOp::Mul,
                "/" => BinOp::Div,
                "//" => BinOp::FloorDiv,
                "%" => BinOp::Mod,
                "@" => BinOp::MatMul,
                _ => return None,
            },
            _ => return None,
        };
        if op.precedence() == level {
            Some(op)
        } else {
            None
        }
    }

    fn parse_binary(&mut self, level: u8) -> PResult<Expr> {
        if level > 11 {
            return self.parse_unary();
        }
        let start = self.span();
        let mut left = self.parse_binary(level + 1)?;
        while let Some(op) = self.binop_at(level) {
            self.advance();
            let right = self.parse_binary(level + 1)?;
            left = Expr::new(ExprKind::BinOp { left: Box::new(left), op, right: Box::new(right) }, self.join(start));
        }
        Ok(left)
    }

    fn parse_bitor(&mut self) -> PResult<Expr> {
        self.parse_binary(6)
    }

    fn parse_arith(&mut self) -> PResult<Expr> {
        self.parse_binary(10)
    }

    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.peek() {
            Tok::Op("-") => Some(UnOp::Neg),
            Tok::Op("+") => Some(UnOp::Pos),
            Tok::Op("~") => Some(UnOp::Invert),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let e = self.parse_unary()?;
            return Ok(Expr::new(ExprKind::UnaryOp { op, operand: Box::new(e) }, self.join(start)));
        }
        self.parse_power()
    }

    fn parse_power(&mut self) -> PResult<Expr> {
        let start = self.span();
        let base = self.parse_await()?;
        if self.eat_op("**") {
            let exp = self.parse_unary()?;
            return Ok(Expr::new(ExprKind::BinOp { left: Box::new(base), op: BinOp::Pow, right: Box::new(exp) }, self.join(start)));
        }
        Ok(base)
    }

    fn parse_await(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat_kw("await") {
            let e = self.parse_primary()?;
            return Ok(Expr::new(ExprKind::Await(Box::new(e)), self.join(start)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let mut e = self.parse_atom()?;
        loop {
            if self.eat_op(".") {
                let attr = match self.advance() {
                    Tok::Name(n) => n,
                    _ => return Err(self.err("expected attribute name")),
                };
                e = Expr::new(ExprKind::Attribute { value: Box::new(e), attr }, self.join(start));
            } else if self.eat_op("(") {
                let (args, keywords) = self.parse_call_args()?;
                e = Expr::new(ExprKind::Call { func: Box::new(e), args, keywords }, self.join(start));
            } else if self.eat_op("[") {
                let index = self.parse_subscript()?;
                self.expect_op("]")?;
                e = Expr::new(ExprKind::Subscript { value: Box::new(e), index: Box::new(index) }, self.join(start));
            } else {
                break;
            }
        }
        Ok(e)
    }

    /// Parses call arguments after '(' through the closing ')'.
    fn parse_call_args(&mut self) -> PResult<(Vec<Expr>, Vec<Keyword>)> {
        let old_no_in = std::mem::replace(&mut self.no_in, false);
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        while !self.is_op(")") {
            let start = self.span();
            if self.eat_op("**") {
                let v = self.parse_expr()?;
                keywords.push(Keyword { name: None, value: v });
            } else if self.eat_op("*") {
                let v = self.parse_expr()?;
                args.push(Expr::new(ExprKind::Starred(Box::new(v)), self.join(start)));
            } else if matches!(self.peek(), Tok::Name(n) if !is_keyword(n)) && matches!(self.peek_n(1), Tok::Op("=")) {
                let name = self.ident()?;
                self.advance();
                let v = self.parse_expr()?;
                keywords.push(Keyword { name: Some(name), value: v });
            } else {
                let v = self.parse_named_expr()?;
                if self.is_kw("for") || self.is_kw("async") {
                    let gens = self.parse_comp_fors()?;
                    args.push(Expr::new(ExprKind::GenExp { elt: Box::new(v), gens }, self.join(start)));
                } else {
                    args.push(v);
                }
            }
            if !self.eat_op(",") {
                break;
            }
        }
        self.expect_op(")")?;
        self.no_in = old_no_in;
        Ok((args, keywords))
    }

    fn parse_subscript(&mut self) -> PResult<Expr> {
        let start = self.span();
        let first = self.parse_slice_item()?;
        if !self.is_op(",") {
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.is_op("]") {
                break;
            }
            elts.push(self.parse_slice_item()?);
        }
        Ok(Expr::new(ExprKind::Tuple { elts, parens: false }, self.join(start)))
    }

    fn parse_slice_item(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat_op("*") {
            let e = self.parse_bitor()?;
            return Ok(Expr::new(ExprKind::Starred(Box::new(e)), self.join(start)));
        }
        let lower = if self.is_op(":") { None } else { Some(Box::new(self.parse_named_expr()?)) };
        if !self.is_op(":") {
            return Ok(*lower.unwrap());
        }
        self.advance();
        let upper = if self.is_op(":") || self.is_op("]") || self.is_op(",") { None } else { Some(Box::new(self.parse_expr()?)) };
        let step = if self.eat_op(":") {
            if self.is_op("]") || self.is_op(",") {
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            }
        } else {
            None
        };
        Ok(Expr::new(ExprKind::Slice { lower, upper, step }, self.join(start)))
    }

    fn parse_comp_fors(&mut self) -> PResult<Vec<Comprehension>> {
        let mut gens = Vec::new();
        loop {
            let is_async = self.eat_kw("async");
            if !self.eat_kw("for") {
                if is_async {
                    return Err(self.err("expected 'for'"));
                }
                break;
            }
            let target = self.parse_target_list()?;
            self.expect_kw("in")?;
            let iter = self.parse_expr_no_cond()?;
            let mut ifs = Vec::new();
            while self.eat_kw("if") {
                ifs.push(self.parse_expr_no_cond()?);
            }
            gens.push(Comprehension { target, iter, ifs, is_async });
        }
        Ok(gens)
    }

    /// Assignment targets for `for`, comprehensions, `with ... as`.
    fn parse_target_list(&mut self) -> PResult<Expr> {
        let start = self.span();
        let old = std::mem::replace(&mut self.no_in, true);
        let first = self.parse_target()?;
        let res = if self.is_op(",") {
            let mut elts = vec![first];
            while self.eat_op(",") {
                if self.is_kw("in") || self.is_op("=") {
                    break;
                }
                elts.push(self.parse_target()?);
            }
            Expr::new(ExprKind::Tuple { elts, parens: false }, self.join(start))
        } else {
            first
        };
        self.no_in = old;
        Ok(res)
    }

    fn parse_target(&mut self) -> PResult<Expr> {
        let start = self.span();
        if self.eat_op("*") {
            let e = self.parse_target()?;
            return Ok(Expr::new(ExprKind::Starred(Box::new(e)), self.join(start)));
        }
        let old = std::mem::replace(&mut self.no_in, true);
        let e = self.parse_bitor();
        self.no_in = old;
        e
    }

    fn parse_atom(&mut self) -> PResult<Expr> {
        let start = self.span();
        let tok = self.peek().clone();
        match tok {
            Tok::Int(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::Int(v), start))
            }
            Tok::BigInt(s) => {
                self.advance();
                Ok(Expr::new(ExprKind::BigInt(s), start))
            }
            Tok::Float(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::Float(v), start))
            }
            Tok::Imag(v) => {
                self.advance();
                Ok(Expr::new(ExprKind::Imag(v), start))
            }
            Tok::Str { .. } => self.parse_strings(),
            Tok::Op("...") => {
                self.advance();
                Ok(Expr::new(ExprKind::Ellipsis, start))
            }
            Tok::Op("(") => {
                self.advance();
                let old_no_in = std::mem::replace(&mut self.no_in, false);
                let res = self.parse_paren(start);
                self.no_in = old_no_in;
                res
            }
            Tok::Op("[") => {
                self.advance();
                let old_no_in = std::mem::replace(&mut self.no_in, false);
                let res = self.parse_list(start);
                self.no_in = old_no_in;
                res
            }
            Tok::Op("{") => {
                self.advance();
                let old_no_in = std::mem::replace(&mut self.no_in, false);
                let res = self.parse_brace(start);
                self.no_in = old_no_in;
                res
            }
            Tok::Name(n) => match n.as_str() {
                "None" => {
                    self.advance();
                    Ok(Expr::new(ExprKind::NoneLit, start))
                }
                "True" => {
                    self.advance();
                    Ok(Expr::new(ExprKind::Bool(true), start))
                }
                "False" => {
                    self.advance();
                    Ok(Expr::new(ExprKind::Bool(false), start))
                }
                "yield" => self.parse_yield(),
                _ if is_keyword(&n) => Err(self.err(format!("unexpected keyword '{}'", n))),
                _ => {
                    self.advance();
                    Ok(Expr::new(ExprKind::Name(n), start))
                }
            },
            _ => Err(self.err(format!("expected expression, found {}", self.describe()))),
        }
    }

    fn parse_paren(&mut self, start: Span) -> PResult<Expr> {
        if self.eat_op(")") {
            return Ok(Expr::new(ExprKind::Tuple { elts: vec![], parens: true }, self.join(start)));
        }
        if self.is_kw("yield") {
            let y = self.parse_yield()?;
            self.expect_op(")")?;
            return Ok(y);
        }
        let first = self.parse_star_or_named()?;
        if self.is_kw("for") || self.is_kw("async") {
            let gens = self.parse_comp_fors()?;
            self.expect_op(")")?;
            return Ok(Expr::new(ExprKind::GenExp { elt: Box::new(first), gens }, self.join(start)));
        }
        if self.eat_op(")") {
            if let ExprKind::Starred(_) = first.kind {
                return Err(self.err("cannot use starred expression here"));
            }
            // The span is widened to include the parentheses so the formatter can keep them.
            let mut first = first;
            first.span = self.join(start);
            return Ok(first);
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.is_op(")") {
                break;
            }
            elts.push(self.parse_star_or_named()?);
        }
        self.expect_op(")")?;
        Ok(Expr::new(ExprKind::Tuple { elts, parens: true }, self.join(start)))
    }

    fn parse_list(&mut self, start: Span) -> PResult<Expr> {
        if self.eat_op("]") {
            return Ok(Expr::new(ExprKind::List(vec![]), self.join(start)));
        }
        let first = self.parse_star_or_named()?;
        if self.is_kw("for") || self.is_kw("async") {
            let gens = self.parse_comp_fors()?;
            self.expect_op("]")?;
            return Ok(Expr::new(ExprKind::ListComp { elt: Box::new(first), gens }, self.join(start)));
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.is_op("]") {
                break;
            }
            elts.push(self.parse_star_or_named()?);
        }
        self.expect_op("]")?;
        Ok(Expr::new(ExprKind::List(elts), self.join(start)))
    }

    fn parse_brace(&mut self, start: Span) -> PResult<Expr> {
        if self.eat_op("}") {
            return Ok(Expr::new(ExprKind::Dict(vec![]), self.join(start)));
        }
        if self.eat_op("**") {
            let v = self.parse_bitor()?;
            let mut items = vec![(None, v)];
            self.parse_dict_rest(&mut items)?;
            return Ok(Expr::new(ExprKind::Dict(items), self.join(start)));
        }
        let first = self.parse_star_or_named()?;
        if self.eat_op(":") {
            let v = self.parse_expr()?;
            if self.is_kw("for") || self.is_kw("async") {
                let gens = self.parse_comp_fors()?;
                self.expect_op("}")?;
                return Ok(Expr::new(ExprKind::DictComp { key: Box::new(first), value: Box::new(v), gens }, self.join(start)));
            }
            let mut items = vec![(Some(first), v)];
            self.parse_dict_rest(&mut items)?;
            return Ok(Expr::new(ExprKind::Dict(items), self.join(start)));
        }
        if self.is_kw("for") || self.is_kw("async") {
            let gens = self.parse_comp_fors()?;
            self.expect_op("}")?;
            return Ok(Expr::new(ExprKind::SetComp { elt: Box::new(first), gens }, self.join(start)));
        }
        let mut elts = vec![first];
        while self.eat_op(",") {
            if self.is_op("}") {
                break;
            }
            elts.push(self.parse_star_or_named()?);
        }
        self.expect_op("}")?;
        Ok(Expr::new(ExprKind::Set(elts), self.join(start)))
    }

    fn parse_dict_rest(&mut self, items: &mut Vec<(Option<Expr>, Expr)>) -> PResult<()> {
        while self.eat_op(",") {
            if self.is_op("}") {
                break;
            }
            if self.eat_op("**") {
                let v = self.parse_bitor()?;
                items.push((None, v));
            } else {
                let k = self.parse_expr()?;
                self.expect_op(":")?;
                let v = self.parse_expr()?;
                items.push((Some(k), v));
            }
        }
        self.expect_op("}")
    }

    fn parse_strings(&mut self) -> PResult<Expr> {
        let start = self.span();
        let mut raw = Vec::new();
        let mut parts: Vec<FStrPart> = Vec::new();
        let mut is_f = false;
        let mut is_bytes: Option<bool> = None;
        let mut bytes_val = Vec::new();
        while let Tok::Str { prefix, body, raw_src } = self.peek().clone() {
            let span = self.span();
            self.advance();
            raw.push(raw_src);
            let b = prefix.contains('b');
            if let Some(prev) = is_bytes {
                if prev != b {
                    return Err(Diag::new(&self.file, span, "cannot mix bytes and nonbytes literals"));
                }
            }
            is_bytes = Some(b);
            if b {
                for ch in body.chars() {
                    bytes_val.push(ch as u32 as u8);
                }
                continue;
            }
            if prefix.contains('f') || prefix.contains('t') {
                is_f = true;
                let fparts = self.parse_fstring_body(&body, prefix.contains('r'), span)?;
                for p in fparts {
                    push_part(&mut parts, p);
                }
            } else {
                push_part(&mut parts, FStrPart::Lit(body));
            }
        }
        let span = self.join(start);
        if is_bytes == Some(true) {
            return Ok(Expr::new(ExprKind::Bytes { value: bytes_val, raw }, span));
        }
        if is_f {
            return Ok(Expr::new(ExprKind::FStr { parts, raw }, span));
        }
        let value = match parts.pop() {
            Some(FStrPart::Lit(s)) => s,
            _ => String::new(),
        };
        Ok(Expr::new(ExprKind::Str { value, raw }, span))
    }

    fn parse_fstring_body(&self, body: &str, raw: bool, span: Span) -> PResult<Vec<FStrPart>> {
        let chars: Vec<char> = body.chars().collect();
        let mut i = 0;
        let mut parts = Vec::new();
        let mut lit = String::new();
        while i < chars.len() {
            let c = chars[i];
            if c == '{' {
                if chars.get(i + 1) == Some(&'{') {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                if !lit.is_empty() {
                    parts.push(FStrPart::Lit(decode_escapes(&std::mem::take(&mut lit), raw)));
                }
                // Find the end of the expression: matching '}' at depth 0, honoring strings.
                i += 1;
                let (expr_src, conv, spec_src, end, debug) = scan_fstring_field(&chars, i)
                    .ok_or_else(|| Diag::new(&self.file, span, "unterminated '{' in f-string"))?;
                i = end;
                let expr = parse_expr_str(&expr_src, &self.file, span)?;
                let spec = match spec_src {
                    Some(s) => self.parse_fstring_body(&s, raw, span)?,
                    None => vec![],
                };
                let debug_text = debug;
                parts.push(FStrPart::Expr { expr: Box::new(expr), conv, spec, debug_text });
            } else if c == '}' {
                if chars.get(i + 1) == Some(&'}') {
                    lit.push('}');
                    i += 2;
                    continue;
                }
                return Err(Diag::new(&self.file, span, "single '}' is not allowed in f-string"));
            } else {
                lit.push(c);
                i += 1;
            }
        }
        if !lit.is_empty() {
            parts.push(FStrPart::Lit(decode_escapes(&lit, raw)));
        }
        Ok(parts)
    }
}

fn push_part(parts: &mut Vec<FStrPart>, p: FStrPart) {
    if let FStrPart::Lit(s) = &p {
        if let Some(FStrPart::Lit(prev)) = parts.last_mut() {
            prev.push_str(s);
            return;
        }
    }
    parts.push(p);
}

/// Returns (expr_source, conversion, format_spec_source, index_after_close, is_debug).
/// Returns (expression source, conversion, format spec source, end index, `=` debug text).
fn scan_fstring_field(chars: &[char], mut i: usize) -> Option<(String, Option<char>, Option<String>, usize, Option<String>)> {
    let start = i;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut expr_end = None;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            '(' | '[' | '{' => depth += 1,
            ')' | ']' => depth -= 1,
            '}' if depth > 0 => depth -= 1,
            '}' | ':' if depth == 0 => {
                expr_end = Some(i);
                break;
            }
            '!' if depth == 0 && chars.get(i + 1) != Some(&'=') => {
                expr_end = Some(i);
                break;
            }
            _ => {}
        }
        i += 1;
    }
    let expr_end = expr_end?;
    let mut expr_src: String = chars[start..expr_end].iter().collect();
    let mut debug = None;
    let trimmed = expr_src.trim_end();
    if trimmed.ends_with('=') && !trimmed.ends_with("==") && !trimmed.ends_with("!=") && !trimmed.ends_with("<=") && !trimmed.ends_with(">=") {
        // `{x = }` prints the field text verbatim, whitespace included
        debug = Some(expr_src.clone());
        expr_src = trimmed[..trimmed.len() - 1].to_string();
    }
    i = expr_end;
    let mut conv = None;
    if chars[i] == '!' {
        conv = chars.get(i + 1).copied();
        i += 2;
    }
    let mut spec = None;
    if chars.get(i) == Some(&':') {
        i += 1;
        let s = i;
        let mut d = 0;
        while i < chars.len() {
            match chars[i] {
                '{' => d += 1,
                '}' if d > 0 => d -= 1,
                '}' => break,
                _ => {}
            }
            i += 1;
        }
        spec = Some(chars[s..i].iter().collect());
    }
    if chars.get(i) != Some(&'}') {
        return None;
    }
    Some((expr_src, conv, spec, i + 1, debug))
}

/// Decode backslash escapes for f-string literal segments (the lexer leaves them raw).
pub fn decode_escapes(s: &str, raw: bool) -> String {
    if raw || !s.contains('\\') {
        return s.to_string();
    }
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '\\' || i + 1 >= chars.len() {
            out.push(c);
            i += 1;
            continue;
        }
        let e = chars[i + 1];
        i += 2;
        match e {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            '\\' => out.push('\\'),
            '\'' => out.push('\''),
            '"' => out.push('"'),
            '0' => out.push('\0'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\n' => {}
            'x' | 'u' | 'U' => {
                let n = match e { 'x' => 2, 'u' => 4, _ => 8 };
                let hex: String = chars[i..(i + n).min(chars.len())].iter().collect();
                i += n;
                if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Module {
        parse(src, "t.fpy").unwrap_or_else(|d| panic!("{:?}: {}", src, d)).module
    }

    #[test]
    fn syntax_errors() {
        let cases = [
            ("x = (1, 2\n", "expected ')'"),
            ("if x\n    pass\n", "expected ':'"),
            ("def f():\npass\n", "expected an indented block"),
            ("async x\n", "expected 'def', 'for' or 'with' after 'async'"),
            ("@dec x\n", "expected newline after decorator"),
            ("@dec\nx = 1\n", "expected 'def' or 'class' after decorator"),
            ("try:\n    pass\nx = 1\n", "expected 'except' or 'finally' block"),
            ("match x:\n    case 1:\n        pass\n    y = 1\n", "expected 'case'"),
            ("x.(y)\n", "expected attribute name"),
            ("[x fo y in z]\n", "expected"),
            ("b'a' 'b'\n", "cannot mix bytes and nonbytes literals"),
            ("f'{}'\n", "expected expression"),
            ("print(1 2)\n", "expected"),
            ("x = )\n", "expected"),
            ("del\n", "expected"),
        ];
        let mut bad = Vec::new();
        for (src, want) in cases {
            match parse(src, "t.fpy") {
                Ok(_) => bad.push(format!("{:?}: parsed", src)),
                Err(d) if !d.msg.contains(want) => bad.push(format!("{:?}: got {:?}, want {:?}", src, d.msg, want)),
                _ => {}
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn unusual_but_valid_syntax() {
        let srcs = [
            "x = yield\n",
            "def f():\n    x = yield\n    y = yield from g()\n    z = await h()\n",
            "match p:\n    case {\"a\": 1, \"b\": [x, y], **rest}:\n        pass\n    case {}:\n        pass\n    case Point(x=1, y=2):\n        pass\n    case mod.Cls():\n        pass\n    case (1 | 2) as v:\n        pass\n    case (x):\n        pass\n    case ():\n        pass\n    case 1, 2,:\n        pass\n",
            "with (a, b):\n    pass\nwith (a as x, b as y,):\n    pass\nwith (yield):\n    pass\n",
            "from ... import x\nfrom .... import y\nfrom . import (a, b,)\n",
            "def f(a, /, b, *, c): pass\ndef g(a, b=1, /): pass\n",
            "x = 1j + 2.5J\ny = 0b101 + 0o17 + 0x1F + 1_000\nz = 99999999999999999999999\n",
            "s = '\\t\\n\\r\\'\\\"\\\\\\a\\b\\f\\v\\x41\\u00e9\\U0001F600\\101\\0'\n",
            "f'{x!r}{y!s:>{w}}{z=}{{}}{a:{b}.{c}}'\n",
            "rb'\\d' + Rb'x' + BR'y'\n",
            "x = [*a, *b]\ny = {**c, 'k': 1}\nprint(*a, *b, **c, **d)\n",
            "lambda: (yield)\n",
            "a = b = c = 1\na, = x\n[a, b] = 1, 2\n",
            "x[1:2:3, ...] = y\n",
            "global a, b\n",
            "if x:\n    pass\nelif y:\n    pass\nelif z:\n    pass\n",
            "type T[K, *Ts, **P] = dict[K, int]\n",
            "x = (yield)\nclass C[T: int, U = str]: pass\n",
            "print(x for x in y)\nprint((x for x in y), 1)\n",
            "assert x, (y)\nraise\nraise E from e\n",
            "for x, in y: pass\nfor (a, b) in c: pass\n",
            "x = not not y\nz = - - 1\nw = ~+1\n",
            "a if b else c if d else e\n",
            "x: 'Forward' = 1\n",
            "if True: x = 1; y = 2\n",
            "class A: pass\nclass B(): pass\nclass C(A, metaclass=M): pass\n",
        ];
        let bad: Vec<String> = srcs.iter().filter_map(|s| parse(s, "t.fpy").err().map(|d| format!("{:?}: {}", s, d.msg))).collect();
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn parenthesized_spans_and_headers() {
        let m = ok("def f(\n    a: int,\n) -> int:\n    return (\n        a\n    )\n");
        if let StmtKind::FuncDef(f) = &m.body[0].kind {
            assert_eq!(f.header_lines, (1, 3));
        } else {
            panic!("expected a function");
        }
        assert!(parse_expr_str("a + b", "t", Span::default()).is_ok());
        assert!(parse_expr_str("a +", "t", Span::default()).is_err());
    }
}
