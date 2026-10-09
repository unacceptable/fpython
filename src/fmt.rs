//! `fpython fmt`: the canonical formatter for .fpy files.
//!
//! Black-compatible in spirit: 4-space indentation, double quotes, two blank lines around
//! top-level definitions, magic trailing commas, and brackets split one item per line when a
//! line exceeds the limit. Comments are preserved; a statement that has comments *inside* it
//! is emitted verbatim (re-indented) so no comment is ever lost.

use crate::ast::*;
use crate::lexer::Comment;
use crate::parser;
use std::collections::BTreeMap;

pub const MAX_WIDTH: usize = 100;

// ---------------------------------------------------------------------- document algebra

#[derive(Debug, Clone)]
pub enum Doc {
    Text(String),
    /// A space when flat, a newline when broken.
    Line,
    /// Nothing when flat, a newline when broken.
    SoftLine,
    Indent(Box<Doc>),
    Group(Box<Doc>),
    /// Breaks its enclosing group.
    ForceBreak,
    Concat(Vec<Doc>),
    /// (broken, flat)
    IfBreak(String, String),
    /// Content inside brackets: only here may Line/SoftLine become newlines.
    Nest(Box<Doc>),
}

fn nest(d: Doc) -> Doc {
    Doc::Nest(Box::new(d))
}

fn text(s: impl Into<String>) -> Doc {
    Doc::Text(s.into())
}
fn concat(v: Vec<Doc>) -> Doc {
    Doc::Concat(v)
}
fn group(d: Doc) -> Doc {
    Doc::Group(Box::new(d))
}
fn indent(d: Doc) -> Doc {
    Doc::Indent(Box::new(d))
}

fn has_force_break(d: &Doc) -> bool {
    match d {
        Doc::ForceBreak => true,
        Doc::Indent(x) | Doc::Nest(x) => has_force_break(x),
        Doc::Group(x) => has_force_break(x),
        Doc::Concat(v) => v.iter().any(has_force_break),
        _ => false,
    }
}

/// Width of the doc when printed flat (None if it contains a forced break).
fn flat_width(d: &Doc) -> Option<usize> {
    match d {
        Doc::Text(s) => {
            if s.contains('\n') {
                None
            } else {
                Some(s.chars().count())
            }
        }
        Doc::Line => Some(1),
        Doc::SoftLine => Some(0),
        Doc::Indent(x) | Doc::Group(x) | Doc::Nest(x) => flat_width(x),
        Doc::ForceBreak => None,
        Doc::Concat(v) => {
            let mut w = 0;
            for x in v {
                w += flat_width(x)?;
            }
            Some(w)
        }
        Doc::IfBreak(_, f) => Some(f.chars().count()),
    }
}

struct Printer {
    out: String,
    col: usize,
    width: usize,
    depth: usize,
}

impl Printer {
    fn print(&mut self, d: &Doc, ind: usize, flat: bool, rest: usize) {
        match d {
            Doc::Text(s) => {
                self.out.push_str(s);
                match s.rfind('\n') {
                    Some(i) => self.col = s[i + 1..].chars().count(),
                    None => self.col += s.chars().count(),
                }
            }
            Doc::Nest(x) => {
                self.depth += 1;
                self.print(x, ind, flat, rest);
                self.depth -= 1;
            }
            Doc::Line => {
                if flat || self.depth == 0 {
                    self.out.push(' ');
                    self.col += 1;
                } else {
                    self.newline(ind);
                }
            }
            Doc::SoftLine => {
                if !flat && self.depth > 0 {
                    self.newline(ind);
                }
            }
            Doc::Indent(x) => self.print(x, ind + 4, flat, rest),
            Doc::Group(x) => {
                if flat {
                    self.print(x, ind, true, rest);
                } else {
                    let fits = !has_force_break(x) && flat_width(x).map(|w| self.col + w + rest <= self.width).unwrap_or(false);
                    self.print(x, ind, fits, rest);
                }
            }
            Doc::ForceBreak => {}
            Doc::Concat(v) => {
                for (i, x) in v.iter().enumerate() {
                    // width of what follows on the same line, for fit decisions
                    let after: usize = if flat {
                        0
                    } else {
                        let (w, broke) = trailing_width(&v[i + 1..], self.col + flat_width(x).unwrap_or(0), self.width);
                        if broke {
                            w
                        } else {
                            w + rest
                        }
                    };
                    self.print(x, ind, flat, after);
                }
            }
            Doc::IfBreak(b, f) => {
                let s = if flat || (self.depth == 0 && b != "(" && b != ")") { f } else { b };
                self.out.push_str(s);
                self.col += s.chars().count();
            }
        }
    }

    fn newline(&mut self, ind: usize) {
        // strip trailing spaces
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
        for _ in 0..ind {
            self.out.push(' ');
        }
        self.col = ind;
    }
}

/// Width up to the first line break in a sequence (and whether a break was reached).
/// Groups that cannot fit flat from `col` are assumed to break at their first break point.
fn trailing_width(v: &[Doc], col: usize, width: usize) -> (usize, bool) {
    let mut w = 0;
    for d in v {
        match d {
            Doc::Line | Doc::SoftLine => return (w, true),
            Doc::Text(s) => w += s.chars().count(),
            Doc::Group(x) => match flat_width(x) {
                Some(fw) if col + w + fw <= width => w += fw,
                _ => {
                    let (w2, b) = trailing_width(std::slice::from_ref(x), col + w, width);
                    w += w2;
                    if b {
                        return (w, true);
                    }
                }
            },
            Doc::Indent(x) | Doc::Nest(x) => {
                let (w2, b) = trailing_width(std::slice::from_ref(x), col + w, width);
                w += w2;
                if b {
                    return (w, true);
                }
            }
            Doc::Concat(x) => {
                let (w2, b) = trailing_width(x, col + w, width);
                w += w2;
                if b {
                    return (w, true);
                }
            }
            Doc::IfBreak(_, f) => w += f.len(),
            Doc::ForceBreak => return (w, true),
        }
    }
    (w, false)
}

pub fn render(d: &Doc, start_col: usize, width: usize) -> String {
    let mut p = Printer { out: String::new(), col: start_col, width, depth: 0 };
    p.print(&group(d.clone()), start_col, false, 0);
    p.out
}

// ---------------------------------------------------------------------- formatter

pub struct Formatter<'a> {
    src: &'a str,
    lines: Vec<&'a str>,
    comments: BTreeMap<usize, Comment>,
    blank: std::collections::HashSet<usize>,
    /// Lines inside multi-line string tokens (never re-indented, never treated as continuations).
    protected: std::collections::HashSet<usize>,
    out: String,
    cur_header_end: Option<usize>,
}

/// Format to a fixpoint (the greedy layout can occasionally settle on a second pass).
pub fn format_source(src: &str, file: &str) -> Result<String, crate::diag::Diag> {
    let mut cur = format_once(src, file)?;
    for _ in 0..3 {
        let next = format_once(&cur, file)?;
        if next == cur {
            break;
        }
        cur = next;
    }
    Ok(cur)
}

fn format_once(src: &str, file: &str) -> Result<String, crate::diag::Diag> {
    let parsed = parser::parse(src, file)?;
    let mut f = Formatter {
        src,
        lines: src.lines().collect(),
        comments: parsed.comments.iter().map(|c| (c.line, c.clone())).collect(),
        blank: parsed.blank_lines.iter().copied().collect(),
        protected: std::collections::HashSet::new(),
        out: String::new(),
        cur_header_end: None,
    };
    if let Ok(lx) = crate::lexer::lex(src, file) {
        for t in &lx.tokens {
            if matches!(t.tok, crate::lexer::Tok::Str { .. }) && t.end_line > t.span.line {
                for l in (t.span.line + 1)..=t.end_line {
                    f.protected.insert(l);
                }
            }
        }
    }
    f.block(&parsed.module.body, 0, true, 0);
    // trailing comments at end of file
    let rest: Vec<Comment> = f.comments.values().cloned().collect();
    if !rest.is_empty() {
        let last_line = parsed.module.body.last().map(|s| s.end_line).unwrap_or(0);
        let mut prev = last_line;
        for c in rest {
            if c.line > prev + 1 && !f.out.is_empty() && f.blank_between(prev, c.line) > 0 {
                f.out.push('\n');
            }
            f.out.push_str(&c.text);
            f.out.push('\n');
            prev = c.line;
        }
        f.comments.clear();
    }
    let mut out = f.out;
    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn detached(src: &str) -> Formatter<'_> {
    Formatter {
        src,
        lines: Vec::new(),
        comments: BTreeMap::new(),
        blank: std::collections::HashSet::new(),
        protected: std::collections::HashSet::new(),
        out: String::new(),
        cur_header_end: None,
    }
}

/// Canonical one-line source of an expression from `src` (used by `fpython doc`).
pub fn expr_source(e: &Expr, src: &str) -> String {
    render(&detached(src).expr(e, 0), 0, usize::MAX / 2)
}

/// Canonical `def` header (without the trailing ':'), laid out like `fpython fmt` at `width`.
pub fn def_header(f: &FuncDef, src: &str, width: usize) -> String {
    let mut h = render(&detached(src).func_header(f), 0, width);
    if h.ends_with(':') {
        h.pop();
    }
    h
}

/// Canonical `class` header (without the trailing ':').
pub fn class_header(c: &ClassDef, src: &str) -> String {
    let fm = detached(src);
    let mut parts = vec![text(format!("class {}", c.name))];
    if !c.type_params.is_empty() {
        parts.push(fm.type_params(&c.type_params));
    }
    let mut items: Vec<Doc> = c.bases.iter().map(|b| fm.expr(b, 0)).collect();
    items.extend(c.keywords.iter().map(|k| fm.keyword(k)));
    if !items.is_empty() {
        parts.push(fm.bracketed("(", items, ")", false));
    }
    render(&concat(parts), 0, usize::MAX / 2)
}

fn is_def(s: &Stmt) -> bool {
    matches!(s.kind, StmtKind::FuncDef(_) | StmtKind::ClassDef(_))
}

fn first_line(s: &Stmt) -> usize {
    match &s.kind {
        StmtKind::FuncDef(f) if !f.decorators.is_empty() => f.decorators[0].span.line,
        StmtKind::ClassDef(c) if !c.decorators.is_empty() => c.decorators[0].span.line,
        _ => s.span.line,
    }
}

impl<'a> Formatter<'a> {
    fn blank_between(&self, a: usize, b: usize) -> usize {
        ((a + 1)..b).filter(|l| self.blank.contains(l)).count()
    }

    fn pad(&mut self, ind: usize) {
        for _ in 0..ind {
            self.out.push(' ');
        }
    }

    /// Emit comments that sit on their own lines before `line`.
    fn leading_comments(&mut self, line: usize, ind: usize) {
        let keys: Vec<usize> = self.comments.range(..line).filter(|(_, c)| !c.trailing).map(|(k, _)| *k).collect();
        let mut prev: Option<usize> = None;
        for k in keys {
            let c = self.comments.remove(&k).unwrap();
            if let Some(p) = prev {
                if self.blank_between(p, k) > 0 {
                    self.out.push('\n');
                }
            }
            self.pad(ind);
            self.out.push_str(&normalize_comment(&c.text));
            self.out.push('\n');
            prev = Some(k);
        }
    }

    fn trailing_comment(&mut self, line: usize) -> Option<String> {
        match self.comments.get(&line) {
            Some(c) if c.trailing => {
                let t = normalize_comment(&c.text);
                self.comments.remove(&line);
                Some(t)
            }
            _ => None,
        }
    }

    fn block(&mut self, body: &[Stmt], ind: usize, top: bool, parent_line: usize) {
        let mut prev: Option<&Stmt> = None;
        for st in body {
            let fl = first_line(st);
            // first leading comment line (to decide blank lines before the comment block)
            let first_comment = self.comments.range(..fl).filter(|(l, c)| !c.trailing && **l > prev.map(|p| p.end_line).unwrap_or(parent_line)).map(|(l, _)| *l).next();
            let anchor = first_comment.unwrap_or(fl);
            if let Some(p) = prev {
                let user_blank = self.blank_between(p.end_line, anchor);
                let want = if top {
                    if is_def(st) || is_def(p) {
                        2
                    } else {
                        user_blank.min(2)
                    }
                } else if is_def(st) || (is_def(p) && !matches!(st.kind, StmtKind::Pass)) {
                    1.max(user_blank.min(1))
                } else {
                    user_blank.min(1)
                };
                for _ in 0..want {
                    self.out.push('\n');
                }
            }
            self.leading_comments(fl, ind);
            self.stmt(st, ind);
            prev = Some(st);
        }
    }

    fn src_of(&self, start_line: usize, end_line: usize) -> Vec<&'a str> {
        self.lines[start_line.saturating_sub(1)..end_line.min(self.lines.len())].to_vec()
    }

    /// Emit a statement verbatim (re-indented), used when it contains interior comments.
    fn verbatim(&mut self, st: &Stmt, ind: usize) {
        let fl = first_line(st);
        let lines = self.src_of(fl, st.end_line);
        let base = lines.first().map(|l| l.len() - l.trim_start().len()).unwrap_or(0);
        for (k, l) in lines.iter().enumerate() {
            let lineno = fl + k;
            if self.protected.contains(&lineno) {
                // inside a multi-line string: copy exactly
                self.out.push_str(l);
                self.out.push('\n');
                continue;
            }
            if l.trim().is_empty() {
                self.out.push('\n');
                continue;
            }
            let cur = l.len() - l.trim_start().len();
            self.pad(ind + cur.saturating_sub(base));
            self.out.push_str(l.trim_start().trim_end());
            self.out.push('\n');
        }
        let keys: Vec<usize> = self.comments.range(fl..=st.end_line).map(|(k, _)| *k).collect();
        for k in keys {
            self.comments.remove(&k);
        }
    }

    fn has_interior_comments(&self, start: usize, end: usize) -> bool {
        self.comments.range(start..end).next().is_some() || self.comments.get(&end).map(|c| !c.trailing).unwrap_or(false)
    }

    fn stmt(&mut self, st: &Stmt, ind: usize) {
        let simple = !matches!(
            st.kind,
            StmtKind::If { .. } | StmtKind::While { .. } | StmtKind::For { .. } | StmtKind::Try { .. } | StmtKind::With { .. } | StmtKind::FuncDef(_) | StmtKind::ClassDef(_) | StmtKind::Match { .. }
        );
        if simple {
            if self.has_interior_comments(st.span.line, st.end_line) || self.has_line_continuation(st) {
                self.verbatim(st, ind);
                return;
            }
            let d = self.simple_stmt(st);
            self.pad(ind);
            let s = render(&d, ind, MAX_WIDTH);
            self.out.push_str(&s);
            if let Some(c) = self.trailing_comment(st.end_line) {
                self.out.push_str("  ");
                self.out.push_str(&c);
            }
            self.out.push('\n');
            return;
        }
        // A header with interior comments whose body starts on the same line: keep it all verbatim.
        let (hstart, hend, body_first) = match &st.kind {
            StmtKind::FuncDef(f) => (f.header_lines.0, f.header_lines.1, f.body.first().map(|b| b.span.line)),
            StmtKind::ClassDef(c) => (c.header_lines.0, c.header_lines.1, c.body.first().map(|b| b.span.line)),
            StmtKind::If { body, .. } | StmtKind::While { body, .. } | StmtKind::For { body, .. } | StmtKind::With { body, .. } | StmtKind::Try { body, .. } => {
                (st.span.line, st.header_end, body.first().map(|b| b.span.line))
            }
            _ => (st.span.line, st.header_end, None),
        };
        if body_first == Some(hend) && ((hend > hstart && self.has_interior_comments(hstart, hend)) || self.has_interior_comments(hend, st.end_line) || self.has_line_continuation(st)) {
            self.verbatim(st, ind);
            return;
        }
        self.cur_header_end = Some(st.header_end);
        match &st.kind {
            StmtKind::FuncDef(f) => {
                self.cur_header_end = None;
                for d in &f.decorators {
                    self.pad(ind);
                    let s = render(&concat(vec![text("@"), self.expr(d, 0)]), ind, MAX_WIDTH);
                    self.out.push_str(&s);
                    self.out.push('\n');
                }
                let (def_line, header_end) = f.header_lines;
                if self.has_interior_comments(def_line, header_end) {
                    // keep the header as written
                    self.verbatim_header(def_line, header_end, ind);
                } else {
                    let d = self.func_header(f);
                    self.pad(ind);
                    let s = render(&d, ind, MAX_WIDTH);
                    self.out.push_str(&s);
                    self.header_comment(def_line, header_end);
                }
                self.body(&f.body, ind, header_end);
            }
            StmtKind::ClassDef(c) => {
                self.cur_header_end = None;
                for d in &c.decorators {
                    self.pad(ind);
                    let s = render(&concat(vec![text("@"), self.expr(d, 0)]), ind, MAX_WIDTH);
                    self.out.push_str(&s);
                    self.out.push('\n');
                }
                let mut parts = vec![text("class "), text(c.name.clone())];
                if !c.type_params.is_empty() {
                    parts.push(self.type_params(&c.type_params));
                }
                if !c.bases.is_empty() || !c.keywords.is_empty() {
                    let mut items: Vec<Doc> = c.bases.iter().map(|b| self.expr(b, 0)).collect();
                    for k in &c.keywords {
                        items.push(self.keyword(k));
                    }
                    parts.push(self.bracketed("(", items, ")", false));
                }
                parts.push(text(":"));
                self.pad(ind);
                let s = render(&concat(parts), ind, MAX_WIDTH);
                self.out.push_str(&s);
                let header_end = c.header_lines.1;
                self.header_comment(c.header_lines.0, header_end);
                self.body(&c.body, ind, header_end);
            }
            StmtKind::If { test, body, orelse } => {
                self.header(ind, concat(vec![text("if "), self.stmt_expr(test), text(":")]), st.span.line);
                self.body(body, ind, st.span.line);
                self.orelse_chain(orelse, ind, body.last().map(|b| b.end_line).unwrap_or(st.span.line));
            }
            StmtKind::While { test, body, orelse } => {
                self.header(ind, concat(vec![text("while "), self.stmt_expr(test), text(":")]), st.span.line);
                self.body(body, ind, st.span.line);
                if !orelse.is_empty() {
                    self.else_block("else:", orelse, ind, body.last().map(|b| b.end_line).unwrap_or(st.span.line));
                }
            }
            StmtKind::For { target, iter, body, orelse, is_async } => {
                let kw = if *is_async { "async for " } else { "for " };
                self.header(ind, concat(vec![text(kw), self.target(target), text(" in "), self.stmt_expr_target(iter), text(":")]), st.span.line);
                self.body(body, ind, st.span.line);
                if !orelse.is_empty() {
                    self.else_block("else:", orelse, ind, body.last().map(|b| b.end_line).unwrap_or(st.span.line));
                }
            }
            StmtKind::Try { body, handlers, orelse, finalbody, star } => {
                self.header(ind, text("try:"), st.span.line);
                self.body(body, ind, st.span.line);
                let mut last = body.last().map(|b| b.end_line).unwrap_or(st.span.line);
                for h in handlers {
                    self.leading_comments(h.span.line, ind);
                    let mut parts = vec![text(if *star { "except*" } else { "except" })];
                    if let Some(t) = &h.typ {
                        parts.push(text(" "));
                        parts.push(self.expr(t, 0));
                        if let Some(n) = &h.name {
                            parts.push(text(format!(" as {}", n)));
                        }
                    }
                    parts.push(text(":"));
                    self.cur_header_end = Some(h.header_end);
                    self.header(ind, concat(parts), h.span.line);
                    self.body(&h.body, ind, h.span.line);
                    last = h.body.last().map(|b| b.end_line).unwrap_or(last);
                }
                if !orelse.is_empty() {
                    self.else_block("else:", orelse, ind, last);
                    last = orelse.last().map(|b| b.end_line).unwrap_or(last);
                }
                if !finalbody.is_empty() {
                    self.else_block("finally:", finalbody, ind, last);
                }
            }
            StmtKind::With { items, body, is_async } => {
                let mut its = Vec::new();
                for it in items {
                    let mut d = vec![self.expr(&it.ctx, 0)];
                    if let Some(v) = &it.var {
                        d.push(text(" as "));
                        d.push(self.target(v));
                    }
                    its.push(concat(d));
                }
                let kw = if *is_async { "async with " } else { "with " };
                let mut parts = vec![text(kw)];
                for (i, d) in its.into_iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(d);
                }
                parts.push(text(":"));
                self.header(ind, concat(parts), st.span.line);
                self.body(body, ind, st.span.line);
            }
            StmtKind::Match { subject, cases } => {
                self.header(ind, concat(vec![text("match "), self.target(subject), text(":")]), st.span.line);
                for c in cases {
                    let first = c.line;
                    self.leading_comments(first, ind + 4);
                    let mut parts = vec![text("case "), self.pattern(&c.pattern)];
                    if let Some(g) = &c.guard {
                        parts.push(text(" if "));
                        parts.push(self.expr(g, 0));
                    }
                    parts.push(text(":"));
                    self.header(ind + 4, concat(parts), first);
                    self.body(&c.body, ind + 4, first);
                }
            }
            _ => unreachable!(),
        }
    }

    fn has_line_continuation(&self, st: &Stmt) -> bool {
        let lines = self.src_of(st.span.line, st.end_line);
        lines.iter().enumerate().any(|(k, l)| {
            let n = st.span.line + k;
            // a backslash at the end of a line that continues inside a string is not a continuation
            l.trim_end().ends_with('\\') && !self.protected.contains(&(n + 1)) && !self.protected.contains(&n)
        })
    }

    fn verbatim_header(&mut self, a: usize, b: usize, ind: usize) {
        let lines = self.src_of(a, b);
        let base = lines.first().map(|l| l.len() - l.trim_start().len()).unwrap_or(0);
        for (k, l) in lines.iter().enumerate() {
            if self.protected.contains(&(a + k)) {
                self.out.push_str(l);
                self.out.push('\n');
                continue;
            }
            if l.trim().is_empty() {
                self.out.push('\n');
                continue;
            }
            let cur = l.len() - l.trim_start().len();
            self.pad(ind + cur.saturating_sub(base));
            self.out.push_str(l.trim());
            self.out.push('\n');
        }
        let keys: Vec<usize> = self.comments.range(a..=b).map(|(k, _)| *k).collect();
        for k in keys {
            self.comments.remove(&k);
        }
    }

    fn header_comment(&mut self, line: usize, header_end: usize) {
        let c = self.trailing_comment(header_end).or_else(|| self.trailing_comment(line));
        if let Some(c) = c {
            self.out.push_str("  ");
            self.out.push_str(&c);
        }
        self.out.push('\n');
    }

    fn header(&mut self, ind: usize, d: Doc, line: usize) {
        let mut end = line;
        if let Some(e) = self.cur_header_end.take() {
            if e > line && self.has_interior_comments(line, e) {
                self.verbatim_header(line, e, ind);
                return;
            }
            end = e.max(line);
        }
        self.pad(ind);
        let s = render(&d, ind, MAX_WIDTH);
        self.out.push_str(&s);
        self.header_comment(line, end);
    }

    fn body(&mut self, body: &[Stmt], ind: usize, header_line: usize) {
        self.block(body, ind + 4, false, header_line);
    }

    fn else_block(&mut self, kw: &str, body: &[Stmt], ind: usize, prev_end: usize) {
        let line = self.find_keyword_line(kw.trim_end_matches(':'), prev_end, body.first().map(|b| b.span.line).unwrap_or(prev_end + 1));
        self.leading_comments(line, ind);
        self.header(ind, text(kw), line);
        self.body(body, ind, line);
    }

    fn find_keyword_line(&self, kw: &str, after: usize, before: usize) -> usize {
        for l in (after + 1)..before.max(after + 1) {
            if let Some(t) = self.lines.get(l - 1) {
                if t.trim_start().starts_with(kw) {
                    return l;
                }
            }
        }
        after + 1
    }

    fn orelse_chain(&mut self, orelse: &[Stmt], ind: usize, prev_end: usize) {
        if orelse.is_empty() {
            return;
        }
        // elif
        if orelse.len() == 1 {
            if let StmtKind::If { test, body, orelse: o2 } = &orelse[0].kind {
                let line = orelse[0].span.line;
                let is_elif = self.lines.get(line - 1).map(|l| l.trim_start().starts_with("elif")).unwrap_or(false);
                if is_elif {
                    self.leading_comments(line, ind);
                    self.cur_header_end = Some(orelse[0].header_end);
                    self.header(ind, concat(vec![text("elif "), self.stmt_expr(test), text(":")]), line);
                    self.body(body, ind, line);
                    self.orelse_chain(o2, ind, body.last().map(|b| b.end_line).unwrap_or(line));
                    return;
                }
            }
        }
        self.else_block("else:", orelse, ind, prev_end);
    }

    fn func_header(&self, f: &FuncDef) -> Doc {
        let mut parts = vec![text(if f.is_async { "async def " } else { "def " }), text(f.name.clone())];
        if !f.type_params.is_empty() {
            parts.push(self.type_params(&f.type_params));
        }
        let items = self.params(&f.params, true);
        let magic = false;
        parts.push(self.bracketed("(", items, ")", magic));
        if let Some(r) = &f.returns {
            parts.push(text(" -> "));
            parts.push(self.expr(r, 0));
        }
        parts.push(text(":"));
        concat(parts)
    }

    fn type_params(&self, tps: &[TypeParam]) -> Doc {
        let items: Vec<Doc> = tps
            .iter()
            .map(|t| match &t.bound {
                Some(b) => concat(vec![text(format!("{}: ", t.name)), self.expr(b, 0)]),
                None => text(t.name.clone()),
            })
            .collect();
        self.bracketed("[", items, "]", false)
    }

    fn params(&self, p: &Params, annotations: bool) -> Vec<Doc> {
        let mut items = Vec::new();
        let arg = |a: &Arg| -> Doc {
            let mut d = vec![text(a.name.clone())];
            if let Some(ann) = &a.ann {
                if annotations {
                    d.push(text(": "));
                    d.push(self.expr(ann, 0));
                }
            }
            if let Some(def) = &a.default {
                if a.ann.is_some() && annotations {
                    d.push(text(" = "));
                } else {
                    d.push(text("="));
                }
                d.push(self.expr(def, 0));
            }
            concat(d)
        };
        for a in &p.posonly {
            items.push(arg(a));
        }
        if !p.posonly.is_empty() {
            items.push(text("/"));
        }
        for a in &p.args {
            items.push(arg(a));
        }
        if let Some(v) = &p.vararg {
            let mut d = vec![text(format!("*{}", v.name))];
            if let Some(ann) = &v.ann {
                d.push(text(": "));
                d.push(self.expr(ann, 0));
            }
            items.push(concat(d));
        } else if !p.kwonly.is_empty() {
            items.push(text("*"));
        }
        for a in &p.kwonly {
            items.push(arg(a));
        }
        if let Some(k) = &p.kwarg {
            let mut d = vec![text(format!("**{}", k.name))];
            if let Some(ann) = &k.ann {
                d.push(text(": "));
                d.push(self.expr(ann, 0));
            }
            items.push(concat(d));
        }
        items
    }

    fn simple_stmt(&self, st: &Stmt) -> Doc {
        match &st.kind {
            StmtKind::Expr(e) => self.rhs(e),
            StmtKind::Assign { targets, value } => {
                let mut parts = Vec::new();
                for t in targets {
                    parts.push(self.target(t));
                    parts.push(text(" = "));
                }
                parts.push(self.rhs(value));
                concat(parts)
            }
            StmtKind::AnnAssign { target, ann, value } => {
                let mut parts = vec![self.target(target), text(": "), self.expr(ann, 0)];
                if let Some(v) = value {
                    parts.push(text(" = "));
                    parts.push(self.rhs(v));
                }
                concat(parts)
            }
            StmtKind::AugAssign { target, op, value } => concat(vec![self.target(target), text(format!(" {}= ", op.symbol())), self.rhs(value)]),
            StmtKind::Pass => text("pass"),
            StmtKind::Break => text("break"),
            StmtKind::Continue => text("continue"),
            StmtKind::Return(v) => match v {
                Some(v) => concat(vec![text("return "), self.rhs(v)]),
                None => text("return"),
            },
            StmtKind::Raise { exc, cause } => {
                let mut parts = vec![text("raise")];
                if let Some(e) = exc {
                    parts.push(text(" "));
                    parts.push(self.expr(e, 0));
                }
                if let Some(c) = cause {
                    parts.push(text(" from "));
                    parts.push(self.expr(c, 0));
                }
                concat(parts)
            }
            StmtKind::Global(ns) => text(format!("global {}", ns.join(", "))),
            StmtKind::Nonlocal(ns) => text(format!("nonlocal {}", ns.join(", "))),
            StmtKind::Del(ts) => {
                let mut parts = vec![text("del ")];
                for (i, t) in ts.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.target(t));
                }
                concat(parts)
            }
            StmtKind::Assert { test, msg } => {
                let mut parts = vec![text("assert "), self.stmt_expr(test)];
                if let Some(m) = msg {
                    parts.push(text(", "));
                    parts.push(self.expr(m, 0));
                }
                concat(parts)
            }
            StmtKind::Import(names) => text(format!("import {}", names.iter().map(alias).collect::<Vec<_>>().join(", "))),
            StmtKind::ImportFrom { module, level, names } => {
                let m = format!("{}{}", ".".repeat(*level), module);
                let items: Vec<Doc> = names.iter().map(|a| text(alias(a))).collect();
                if names.len() == 1 && names[0].name == "*" {
                    return text(format!("from {} import *", m));
                }
                let flat = format!("from {} import {}", m, names.iter().map(alias).collect::<Vec<_>>().join(", "));
                if flat.len() <= MAX_WIDTH && !self.src_slice_has_trailing_comma(st) {
                    return text(flat);
                }
                concat(vec![text(format!("from {} import ", m)), self.bracketed("(", items, ")", true)])
            }
            StmtKind::TypeAlias { name, type_params, value } => {
                let mut parts = vec![text(format!("type {}", name))];
                if !type_params.is_empty() {
                    parts.push(self.type_params(type_params));
                }
                parts.push(text(" = "));
                parts.push(self.expr(value, 0));
                concat(parts)
            }
            _ => text(""),
        }
    }

    fn src_slice_has_trailing_comma(&self, st: &Stmt) -> bool {
        let s = &self.src[st.span.start.min(self.src.len())..st.span.end.min(self.src.len())];
        let t = s.trim_end().trim_end_matches(')').trim_end();
        s.trim_end().ends_with(')') && t.ends_with(',')
    }

    /// Right-hand sides: bare tuples stay unparenthesized.
    fn rhs(&self, e: &Expr) -> Doc {
        if self.has_parens(e) {
            return self.expr_tuple_noparen(e);
        }
        invisible_parens(self.expr_tuple_noparen(e), e)
    }

    /// A statement-level expression that may be wrapped in parentheses when split.
    fn stmt_expr(&self, e: &Expr) -> Doc {
        if self.has_parens(e) {
            return self.expr(e, 0);
        }
        invisible_parens(self.expr(e, 0), e)
    }

    fn expr_tuple_noparen(&self, e: &Expr) -> Doc {
        if let ExprKind::Tuple { elts, parens: false } = &e.kind {
            if !elts.is_empty() && !self.has_parens(e) {
                let mut parts = Vec::new();
                for (i, x) in elts.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(","));
                        parts.push(Doc::Line);
                    }
                    parts.push(self.expr(x, 1));
                }
                if elts.len() == 1 {
                    parts.push(Doc::IfBreak("".into(), ",".into()));
                }
                return concat(parts);
            }
        }
        self.expr(e, 0)
    }

    fn stmt_expr_target(&self, e: &Expr) -> Doc {
        match &e.kind {
            ExprKind::Tuple { parens: false, .. } => self.target(e),
            _ => self.stmt_expr(e),
        }
    }

    /// Assignment/loop targets and other unbracketed tuple positions: never split.
    fn target(&self, e: &Expr) -> Doc {
        if let ExprKind::Tuple { elts, parens: false } = &e.kind {
            if !elts.is_empty() && !self.has_parens(e) {
                let mut parts = Vec::new();
                for (i, x) in elts.iter().enumerate() {
                    if i > 0 {
                        parts.push(text(", "));
                    }
                    parts.push(self.expr(x, 1));
                }
                if elts.len() == 1 {
                    parts.push(text(","));
                }
                return concat(parts);
            }
        }
        self.expr(e, 0)
    }

    fn keyword(&self, k: &Keyword) -> Doc {
        match &k.name {
            Some(n) => concat(vec![text(format!("{}=", n)), self.expr(&k.value, 1)]),
            None => concat(vec![text("**"), self.expr(&k.value, 15)]),
        }
    }

    /// `open items close` that splits one item per line (with trailing comma) when too long.
    fn bracketed(&self, open: &str, items: Vec<Doc>, close: &str, force: bool) -> Doc {
        if items.is_empty() {
            return text(format!("{}{}", open, close));
        }
        let mut inner = vec![Doc::SoftLine];
        let n = items.len();
        for (i, it) in items.into_iter().enumerate() {
            inner.push(it);
            if i + 1 < n {
                inner.push(text(","));
                inner.push(Doc::Line);
            }
        }
        inner.push(Doc::IfBreak(",".into(), "".into()));
        let mut body = vec![text(open), nest(concat(vec![indent(concat(inner)), Doc::SoftLine])), text(close)];
        if force {
            body.insert(0, Doc::ForceBreak);
        }
        group(concat(body))
    }

    fn magic_comma(&self, e: &Expr) -> bool {
        let end = e.span.end.min(self.src.len());
        let start = e.span.start.min(end);
        let s = self.src[start..end].trim_end();
        if s.len() < 2 {
            return false;
        }
        let body = s[..s.len() - 1].trim_end();
        body.ends_with(',') && !body.ends_with("(,")
    }

    fn has_parens(&self, e: &Expr) -> bool {
        let start = e.span.start;
        let end = e.span.end.min(self.src.len());
        if start >= end || !self.src[start..].starts_with('(') {
            return false;
        }
        if !self.src[..end].ends_with(')') {
            return false;
        }
        // the opening paren must match the closing one
        let bytes = &self.src.as_bytes()[start..end];
        let mut depth = 0i32;
        let mut quote: Option<u8> = None;
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(q) = quote {
                if b == b'\\' {
                    continue;
                }
                if b == q {
                    quote = None;
                }
                continue;
            }
            match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 && i != bytes.len() - 1 {
                        return false;
                    }
                }
                _ => {}
            }
        }
        depth == 0
    }

    /// Expression doc; `ctx` is the minimum precedence the context requires without parens.
    pub fn expr(&self, e: &Expr, ctx: u8) -> Doc {
        let p = prec(e);
        let keep = self.has_parens(e) && !matches!(e.kind, ExprKind::Tuple { .. } | ExprKind::GenExp { .. });
        let d = self.expr_inner(e);
        if keep {
            // user parentheses are preserved (and allow splitting inside them)
            return group(concat(vec![text("("), nest(concat(vec![indent(concat(vec![Doc::SoftLine, d])), Doc::SoftLine])), text(")")]));
        }
        if p < ctx {
            // same layout as user-written parentheses, so a second pass is stable
            group(concat(vec![text("("), nest(concat(vec![indent(concat(vec![Doc::SoftLine, d])), Doc::SoftLine])), text(")")]))
        } else {
            d
        }
    }

    fn expr_inner(&self, e: &Expr) -> Doc {
        match &e.kind {
            ExprKind::Name(n) => text(n.clone()),
            ExprKind::Int(_) | ExprKind::BigInt(_) | ExprKind::Float(_) | ExprKind::Imag(_) => {
                if let ExprKind::BigInt(b) = &e.kind {
                    return text(normalize_number(b));
                }
                let s = &self.src[e.span.start.min(self.src.len())..e.span.end.min(self.src.len())];
                let s = s.trim_start_matches('(').trim_end_matches(')').trim();
                text(normalize_number(s))
            }
            ExprKind::Bool(b) => text(if *b { "True" } else { "False" }),
            ExprKind::NoneLit => text("None"),
            ExprKind::Ellipsis => text("..."),
            ExprKind::Str { raw, .. } | ExprKind::Bytes { raw, .. } | ExprKind::FStr { raw, .. } => {
                if raw.len() == 1 {
                    return text(normalize_string(&raw[0]));
                }
                // Implicit concatenation keeps the author's layout: split if it spanned lines.
                let span_src = self.src[e.span.start.min(self.src.len())..e.span.end.min(self.src.len())]
                    .trim_start_matches(|c: char| c == '(' || c.is_whitespace())
                    .trim_end_matches(|c: char| c == ')' || c.is_whitespace());
                let multiline = raw.iter().map(|r| r.matches('\n').count()).sum::<usize>() < span_src.matches('\n').count();
                let parts: Vec<Doc> = raw.iter().map(|r| text(normalize_string(r))).collect();
                let mut v = Vec::new();
                if multiline {
                    v.push(Doc::ForceBreak);
                }
                for (i, p) in parts.into_iter().enumerate() {
                    if i > 0 {
                        v.push(if multiline { Doc::Line } else { text(" ") });
                    }
                    v.push(p);
                }
                group(concat(v))
            }
            ExprKind::BinOp { left, op, right } => {
                let p = op.precedence();
                let (lp, rp) = if *op == BinOp::Pow { (p + 1, p) } else { (p, p + 1) };
                let simple_pow = *op == BinOp::Pow && is_simple_operand(left) && is_simple_operand(right);
                let sep = if simple_pow { "" } else { " " };
                concat(vec![self.expr(left, lp), text(format!("{}{}{}", sep, op.symbol(), sep)), self.expr(right, rp)])
            }
            ExprKind::UnaryOp { op, operand } => {
                let (s, p) = match op {
                    UnOp::Not => ("not ", 4),
                    UnOp::Neg => ("-", 12),
                    UnOp::Pos => ("+", 12),
                    UnOp::Invert => ("~", 12),
                };
                concat(vec![text(s), self.expr(operand, p)])
            }
            ExprKind::BoolOp { op, values } => {
                let (kw, p) = match op {
                    BoolOp::And => ("and", 3),
                    BoolOp::Or => ("or", 2),
                };
                let mut v = Vec::new();
                for (i, x) in values.iter().enumerate() {
                    if i > 0 {
                        v.push(Doc::Line);
                        v.push(text(format!("{} ", kw)));
                    }
                    v.push(self.expr(x, p + 1));
                }
                concat(v)
            }
            ExprKind::Compare { left, ops, comparators } => {
                let mut v = vec![self.expr(left, 6)];
                for (o, c) in ops.iter().zip(comparators) {
                    v.push(text(format!(" {} ", o.symbol())));
                    v.push(self.expr(c, 6));
                }
                concat(v)
            }
            ExprKind::Call { func, args, keywords } => {
                let mut items: Vec<Doc> = Vec::new();
                // a lone generator argument keeps its own parentheses: f(x for x in y)
                if args.len() == 1 && keywords.is_empty() {
                    if let ExprKind::GenExp { elt, gens } = &args[0].kind {
                        if !self.has_parens(&args[0]) || self.src[args[0].span.start..].starts_with('(') && !self.genexp_has_own_parens(e, &args[0]) {
                            let g = concat(vec![self.expr(elt, 1), self.comp_fors(gens)]);
                            return concat(vec![self.expr(func, 15), group(concat(vec![text("("), nest(concat(vec![indent(concat(vec![Doc::SoftLine, g])), Doc::SoftLine])), text(")")]))]);
                        }
                    }
                }
                for a in args {
                    items.push(self.expr(a, 1));
                }
                for k in keywords {
                    items.push(self.keyword(k));
                }
                let magic = self.magic_comma(e);
                concat(vec![self.expr(func, 15), self.bracketed("(", items, ")", magic)])
            }
            ExprKind::Attribute { value, attr } => {
                // integer literals need a space or parens before '.'
                let v = self.expr(value, 15);
                if matches!(value.kind, ExprKind::Int(_)) && !self.has_parens(value) {
                    return concat(vec![text("("), v, text(")"), text(format!(".{}", attr))]);
                }
                concat(vec![v, text(format!(".{}", attr))])
            }
            ExprKind::Subscript { value, index } => {
                let idx = match &index.kind {
                    ExprKind::Tuple { elts, parens: false } if !elts.is_empty() && !self.has_parens(index) => {
                        let mut v = Vec::new();
                        for (i, x) in elts.iter().enumerate() {
                            if i > 0 {
                                v.push(text(", "));
                            }
                            v.push(self.expr(x, 1));
                        }
                        if elts.len() == 1 {
                            v.push(text(","));
                        }
                        concat(v)
                    }
                    _ => self.expr(index, 0),
                };
                concat(vec![self.expr(value, 15), text("["), idx, text("]")])
            }
            ExprKind::Slice { lower, upper, step } => {
                let complex = [lower, upper, step].iter().any(|x| x.as_ref().map(|e| !is_simple_operand(e)).unwrap_or(false));
                let sep = if complex { " : " } else { ":" };
                let mut v = Vec::new();
                if let Some(l) = lower {
                    v.push(self.expr(l, 1));
                }
                v.push(text(if complex && lower.is_none() { ": " } else if complex && upper.is_none() && step.is_none() { " :" } else { sep }));
                if let Some(u) = upper {
                    v.push(self.expr(u, 1));
                }
                if let Some(s) = step {
                    v.push(text(sep));
                    v.push(self.expr(s, 1));
                }
                concat(v)
            }
            ExprKind::Starred(x) => concat(vec![text("*"), self.expr(x, 6)]),
            ExprKind::List(elts) => {
                let items = elts.iter().map(|x| self.expr(x, 1)).collect();
                self.bracketed("[", items, "]", self.magic_comma(e))
            }
            ExprKind::Set(elts) => {
                let items = elts.iter().map(|x| self.expr(x, 1)).collect();
                self.bracketed("{", items, "}", self.magic_comma(e))
            }
            ExprKind::Tuple { elts, .. } => {
                if elts.len() == 1 {
                    return concat(vec![text("("), self.expr(&elts[0], 1), text(",)")]);
                }
                let items = elts.iter().map(|x| self.expr(x, 1)).collect();
                self.bracketed("(", items, ")", self.magic_comma(e))
            }
            ExprKind::Dict(items) => {
                let ds = items
                    .iter()
                    .map(|(k, v)| match k {
                        Some(k) => concat(vec![self.expr(k, 1), text(": "), self.expr(v, 1)]),
                        None => concat(vec![text("**"), self.expr(v, 6)]),
                    })
                    .collect();
                self.bracketed("{", ds, "}", self.magic_comma(e))
            }
            ExprKind::ListComp { elt, gens } => self.comp("[", self.expr(elt, 1), gens, "]"),
            ExprKind::SetComp { elt, gens } => self.comp("{", self.expr(elt, 1), gens, "}"),
            ExprKind::GenExp { elt, gens } => self.comp("(", self.expr(elt, 1), gens, ")"),
            ExprKind::DictComp { key, value, gens } => self.comp("{", concat(vec![self.expr(key, 1), text(": "), self.expr(value, 1)]), gens, "}"),
            ExprKind::IfExp { test, body, orelse } => concat(vec![self.expr(body, 2), Doc::Line, text("if "), self.expr(test, 2), Doc::Line, text("else "), self.expr(orelse, 1)]),
            ExprKind::Lambda { params, body } => {
                let ps = self.params(params, false);
                let mut v = vec![text("lambda")];
                if !ps.is_empty() {
                    v.push(text(" "));
                    for (i, p) in ps.into_iter().enumerate() {
                        if i > 0 {
                            v.push(text(", "));
                        }
                        v.push(p);
                    }
                }
                v.push(text(": "));
                v.push(self.expr(body, 0));
                concat(v)
            }
            ExprKind::NamedExpr { target, value } => concat(vec![text(format!("{} := ", target)), self.expr(value, 1)]),
            ExprKind::Await(x) => concat(vec![text("await "), self.expr(x, 15)]),
            ExprKind::Yield(v) => match v {
                Some(v) => concat(vec![text("yield "), self.target(v)]),
                None => text("yield"),
            },
            ExprKind::YieldFrom(v) => concat(vec![text("yield from "), self.expr(v, 1)]),
        }
    }

    fn genexp_has_own_parens(&self, call: &Expr, arg: &Expr) -> bool {
        // f((x for x in y)) -> keep; f(x for x in y) -> arg span starts at '(' of the call
        let _ = call;
        let s = &self.src[arg.span.start..arg.span.end.min(self.src.len())];
        s.starts_with("((")
    }

    fn comp_fors(&self, gens: &[Comprehension]) -> Doc {
        let mut v = Vec::new();
        for g in gens {
            v.push(Doc::Line);
            v.push(text(if g.is_async { "async for " } else { "for " }));
            v.push(self.target(&g.target));
            v.push(text(" in "));
            v.push(self.expr(&g.iter, 2));
            for c in &g.ifs {
                v.push(Doc::Line);
                v.push(text("if "));
                v.push(self.expr(c, 2));
            }
        }
        concat(v)
    }

    fn comp(&self, open: &str, elt: Doc, gens: &[Comprehension], close: &str) -> Doc {
        group(concat(vec![text(open), nest(concat(vec![indent(concat(vec![Doc::SoftLine, elt, self.comp_fors(gens)])), Doc::SoftLine])), text(close)]))
    }

    fn pattern(&self, p: &Pattern) -> Doc {
        match p {
            Pattern::Wildcard => text("_"),
            Pattern::Capture(n) => text(n.clone()),
            Pattern::Value(e) => self.expr(e, 0),
            Pattern::Or(ps) => {
                let mut v = Vec::new();
                for (i, x) in ps.iter().enumerate() {
                    if i > 0 {
                        v.push(text(" | "));
                    }
                    if matches!(x, Pattern::As(..) | Pattern::Or(..)) {
                        v.push(concat(vec![text("("), self.pattern(x), text(")")]));
                    } else {
                        v.push(self.pattern(x));
                    }
                }
                concat(v)
            }
            Pattern::As(p, n) => {
                let inner = if matches!(**p, Pattern::As(..) | Pattern::Or(..)) { concat(vec![text("("), self.pattern(p), text(")")]) } else { self.pattern(p) };
                concat(vec![inner, text(format!(" as {}", n))])
            }
            Pattern::Sequence(ps, paren) => {
                let items: Vec<Doc> = ps.iter().map(|x| self.pattern(x)).collect();
                if *paren && items.len() == 1 {
                    concat(vec![text("("), items.into_iter().next().unwrap(), text(",)")])
                } else if *paren {
                    self.bracketed("(", items, ")", false)
                } else {
                    self.bracketed("[", items, "]", false)
                }
            }
            Pattern::Star(n) => text(format!("*{}", n.clone().unwrap_or_else(|| "_".into()))),
            Pattern::Class { cls, args, kwargs } => {
                let mut items: Vec<Doc> = args.iter().map(|x| self.pattern(x)).collect();
                for (k, v) in kwargs {
                    items.push(concat(vec![text(format!("{}=", k)), self.pattern(v)]));
                }
                concat(vec![self.expr(cls, 15), self.bracketed("(", items, ")", false)])
            }
            Pattern::Mapping { keys, values, rest } => {
                let mut items: Vec<Doc> = keys.iter().zip(values).map(|(k, v)| concat(vec![self.expr(k, 1), text(": "), self.pattern(v)])).collect();
                if let Some(r) = rest {
                    items.push(text(format!("**{}", r)));
                }
                self.bracketed("{", items, "}", false)
            }
        }
    }
}

/// Wrap an expression so that, if it must be split, it is enclosed in parentheses (only
/// expressions whose splitting points are not already inside brackets need this).
fn invisible_parens(d: Doc, e: &Expr) -> Doc {
    let needs = matches!(
        e.kind,
        ExprKind::BoolOp { .. } | ExprKind::IfExp { .. } | ExprKind::Str { .. } | ExprKind::Bytes { .. } | ExprKind::FStr { .. } | ExprKind::Tuple { parens: false, .. }
    );
    if !needs {
        return d;
    }
    if let ExprKind::Str { raw, .. } | ExprKind::Bytes { raw, .. } | ExprKind::FStr { raw, .. } = &e.kind {
        if raw.len() < 2 {
            return d;
        }
    }
    let trailing = if matches!(e.kind, ExprKind::Tuple { parens: false, .. }) { Doc::IfBreak(",".into(), "".into()) } else { text("") };
    group(concat(vec![
        Doc::IfBreak("(".into(), "".into()),
        nest(concat(vec![indent(concat(vec![Doc::SoftLine, d, trailing])), Doc::SoftLine])),
        Doc::IfBreak(")".into(), "".into()),
    ]))
}

fn alias(a: &Alias) -> String {
    match &a.asname {
        Some(n) => format!("{} as {}", a.name, n),
        None => a.name.clone(),
    }
}

fn is_simple_operand(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Name(_) | ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::NoneLit | ExprKind::Bool(_) => true,
        ExprKind::UnaryOp { op: UnOp::Neg, operand } => is_simple_operand(operand),
        ExprKind::Attribute { value, .. } => is_simple_operand(value),
        _ => false,
    }
}

/// Precedence of an expression (higher binds tighter).
fn prec(e: &Expr) -> u8 {
    match &e.kind {
        ExprKind::Tuple { parens: false, elts } if !elts.is_empty() => 0,
        ExprKind::Lambda { .. } => 1,
        ExprKind::NamedExpr { .. } => 0,
        ExprKind::IfExp { .. } => 1,
        ExprKind::BoolOp { op: BoolOp::Or, .. } => 2,
        ExprKind::BoolOp { op: BoolOp::And, .. } => 3,
        ExprKind::UnaryOp { op: UnOp::Not, .. } => 4,
        ExprKind::Compare { .. } => 5,
        ExprKind::BinOp { op, .. } => op.precedence(),
        ExprKind::UnaryOp { .. } => 12,
        ExprKind::Await(_) => 14,
        ExprKind::Yield(_) | ExprKind::YieldFrom(_) => 0,
        ExprKind::Starred(_) => 1,
        _ => 16,
    }
}

fn normalize_comment(c: &str) -> String {
    let body = c.trim_start_matches('#');
    if c.starts_with("#!") || body.is_empty() || body.starts_with(' ') || body.starts_with(':') {
        return c.to_string();
    }
    format!("# {}", body)
}

fn normalize_number(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("0x") {
        return format!("0x{}", s[2..].to_ascii_uppercase());
    }
    if lower.starts_with("0o") || lower.starts_with("0b") {
        return lower;
    }
    // exponent and suffix lowercase (e, j)
    lower
}

/// Prefer double quotes when it does not require more escaping (like Black).
fn normalize_string(raw: &str) -> String {
    let prefix_len = raw.find(|c| c == '\'' || c == '"').unwrap_or(0);
    let prefix = raw[..prefix_len].to_ascii_lowercase().replace('u', "");
    let body = &raw[prefix_len..];
    if body.starts_with("'''") || body.starts_with("\"\"\"") {
        if body.starts_with("'''") && !body[3..body.len() - 3].contains("\"\"\"") && !body[3..body.len() - 3].ends_with('"') && !body[3..].starts_with('"') {
            return format!("{}\"\"\"{}\"\"\"", prefix, &body[3..body.len() - 3]);
        }
        return format!("{}{}", prefix, body);
    }
    if body.starts_with('\'') && body.len() >= 2 {
        let inner = &body[1..body.len() - 1];
        // f-strings may contain nested quotes in fields; only convert if no double quote at all
        if !inner.contains('"') {
            // unescape \' since it's no longer needed
            let converted = if prefix.contains('r') { inner.to_string() } else { inner.replace("\\'", "'") };
            if prefix.contains('r') && inner.contains("\\'") {
                return format!("{}{}", prefix, body);
            }
            return format!("{}\"{}\"", prefix, converted);
        }
    }
    format!("{}{}", prefix, body)
}

/// Result of checking the formatter on one file (`fpython fmt-verify`).
#[derive(Debug, PartialEq)]
pub enum Verify {
    Ok,
    /// the input itself doesn't parse
    Skipped,
    /// a report describing what went wrong
    Bad(String),
}

/// Check that `format` preserves the AST, is idempotent and keeps every comment.
pub fn verify(src: &str, name: &str, verbose: bool, format: fn(&str, &str) -> Result<String, crate::diag::Diag>) -> Verify {
    let Ok(orig) = parser::parse(src, name) else { return Verify::Skipped };
    let out = match format(src, name) {
        Ok(o) => o,
        Err(d) => return Verify::Bad(format!("FORMAT-ERROR {}: {}\n", name, d)),
    };
    let re = match parser::parse(&out, name) {
        Ok(p) => p,
        Err(d) => return Verify::Bad(format!("REPARSE-ERROR {}: {}\n", name, d)),
    };
    if ast_fingerprint(&orig.module) != ast_fingerprint(&re.module) {
        let mut r = format!("AST-CHANGED {}\n", name);
        if verbose {
            // show the first top-level statement that changed
            if let Some((a, b)) = orig.module.body.iter().zip(re.module.body.iter()).find(|(a, b)| {
                ast_fingerprint(&Module { body: vec![(*a).clone()] }) != ast_fingerprint(&Module { body: vec![(*b).clone()] })
            }) {
                let show = |text: &str, st: &Stmt, label: &str, r: &mut String| {
                    let lines: Vec<&str> = text.lines().collect();
                    r.push_str(&format!("--- {} (line {}):\n", label, st.span.line));
                    for l in &lines[st.span.line - 1..st.end_line.min(lines.len()).min(st.span.line + 15)] {
                        r.push_str(&format!("  {}\n", l));
                    }
                };
                show(src, a, "original", &mut r);
                show(&out, b, "formatted", &mut r);
            }
        }
        return Verify::Bad(r);
    }
    let again = format(&out, name).unwrap_or_default();
    if again != out {
        let mut r = format!("NOT-IDEMPOTENT {}\n", name);
        if verbose {
            if let Some((i, (a, b))) = out.lines().zip(again.lines()).enumerate().find(|(_, (a, b))| a != b) {
                r.push_str(&format!("  line {}:\n  1st: {}\n  2nd: {}\n", i + 1, a, b));
            }
        }
        return Verify::Bad(r);
    }
    let ncomments = |s: &str| crate::lexer::lex(s, "x").map(|l| l.comments.len()).unwrap_or(0);
    if ncomments(src) != ncomments(&out) {
        return Verify::Bad(format!("COMMENTS-LOST {}\n", name));
    }
    Verify::Ok
}

/// A unified diff (3 lines of context) between `old` and `new`, as printed by `fpython fmt --diff`.
pub fn unified_diff(old: &str, new: &str, name: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // edit script from an LCS table; very large inputs degrade to a whole-file replacement
    let mut ops: Vec<(char, usize, usize)> = Vec::new(); // (' ' | '-' | '+', index in a, index in b)
    if a.len().saturating_mul(b.len()) <= 25_000_000 {
        let (n, m) = (a.len(), b.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if a[i] == b[j] { t[(i + 1) * (m + 1) + j + 1] + 1 } else { t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                ops.push((' ', i, j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1]) {
                ops.push(('-', i, j));
                i += 1;
            } else {
                ops.push(('+', i, j));
                j += 1;
            }
        }
    } else {
        ops.extend((0..a.len()).map(|i| ('-', i, 0)));
        ops.extend((0..b.len()).map(|j| ('+', a.len(), j)));
    }
    let mut out = String::new();
    let mut k = 0;
    while k < ops.len() {
        if ops[k].0 == ' ' {
            k += 1;
            continue;
        }
        // grow a hunk until there are more than 6 unchanged lines in a row
        let start = k.saturating_sub(3);
        let mut end = k;
        let mut same = 0usize;
        while end < ops.len() && same <= 6 {
            same = if ops[end].0 == ' ' { same + 1 } else { 0 };
            end += 1;
        }
        let end = end - same.saturating_sub(3);
        if out.is_empty() {
            out.push_str(&format!("--- {}\n+++ {}\n", name, name));
        }
        let hunk = &ops[start..end];
        let (a0, b0) = (hunk[0].1, hunk[0].2);
        let na = hunk.iter().filter(|o| o.0 != '+').count();
        let nb = hunk.iter().filter(|o| o.0 != '-').count();
        out.push_str(&format!("@@ -{},{} +{},{} @@\n", a0 + 1, na, b0 + 1, nb));
        for &(c, i, j) in hunk {
            let line = if c == '+' { b[j] } else { a[i] };
            out.push_str(&format!("{}{}\n", c, line));
        }
        k = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(s: &str) -> String {
        format_source(s, "t.fpy").unwrap()
    }

    #[test]
    fn basic_spacing() {
        assert_eq!(fmt("x=1+2\n"), "x = 1 + 2\n");
        assert_eq!(fmt("def f(a:int,b:int=2)->int:\n  return a*b\n"), "def f(a: int, b: int = 2) -> int:\n    return a * b\n");
    }

    #[test]
    fn quotes() {
        assert_eq!(fmt("s = 'hi'\n"), "s = \"hi\"\n");
        assert_eq!(fmt("s = 'say \"x\"'\n"), "s = 'say \"x\"'\n");
        assert_eq!(fmt("s = 'it\\'s'\n"), "s = \"it's\"\n");
    }

    #[test]
    fn blank_lines_between_defs() {
        assert_eq!(fmt("import os\ndef f() -> None:\n    pass\nx = 1\n"), "import os\n\n\ndef f() -> None:\n    pass\n\n\nx = 1\n");
    }

    #[test]
    fn comments_preserved() {
        let src = "# top\nx = 1  # trailing\n\n\n# lead\ndef f() -> None:\n    # inner\n    pass\n";
        let out = fmt(src);
        assert!(out.contains("# top"));
        assert!(out.contains("x = 1  # trailing"));
        assert!(out.contains("# inner"));
        assert_eq!(fmt(&out), out);
    }

    #[test]
    fn long_call_splits() {
        let src = format!("result = some_function({})\n", (0..30).map(|i| format!("arg{}", i)).collect::<Vec<_>>().join(", "));
        let out = fmt(&src);
        assert!(out.contains("(\n    arg0,\n"), "{}", out);
        assert!(out.trim_end().ends_with(")"));
        assert_eq!(fmt(&out), out);
    }

    #[test]
    fn magic_trailing_comma() {
        let out = fmt("x = [1, 2,]\n");
        assert_eq!(out, "x = [\n    1,\n    2,\n]\n");
    }

    #[test]
    fn keeps_parens_and_precedence() {
        assert_eq!(fmt("x = (a + b) * c\n"), "x = (a + b) * c\n");
        assert_eq!(fmt("x = a + b * c\n"), "x = a + b * c\n");
        assert_eq!(fmt("x = -a ** 2\n"), "x = -a**2\n");
        assert_eq!(fmt("x = 2**10\n"), "x = 2**10\n");
    }

    #[test]
    fn idempotent_on_constructs() {
        let src = "class A(B):\n    x: int = 0\n    def m(self) -> int:\n        if self.x:\n            return 1\n        elif self.x < 0:\n            return 2\n        else:\n            return 3\nfor i, j in enumerate(range(3)):\n    try:\n        pass\n    except (ValueError, KeyError) as e:\n        raise\n    finally:\n        print(i)\nwith open('f') as f:\n    data = [x for x in f if x]\nmatch v:\n    case [1, *rest]:\n        pass\n    case Point(x=0) | None:\n        pass\n";
        let out = fmt(src);
        assert_eq!(fmt(&out), out);
        // reparse succeeds
        parser::parse(&out, "t.fpy").unwrap();
    }

    #[test]
    fn number_normalization() {
        assert_eq!(fmt("x = 0XFF + 1E5 + 0O7\n"), "x = 0xFF + 1e5 + 0o7\n");
    }

    #[test]
    fn unified_diff_output() {
        assert_eq!(super::unified_diff("a\n", "a\n", "f"), "");
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n";
        let new = "1\n2\nthree\n4\n5\n6\n7\n8\n9\n10\n11\n12\n13\n";
        let d = super::unified_diff(old, new, "f.fpy");
        assert!(d.starts_with("--- f.fpy\n+++ f.fpy\n@@ -1,6 +1,6 @@\n 1\n 2\n-3\n+three\n 4\n"), "{}", d);
        assert!(d.ends_with("@@ -10,3 +10,4 @@\n 10\n 11\n 12\n+13\n"), "{}", d);
    }

    #[test]
    fn match_pattern_brackets_are_kept() {
        let src = "match x:\n    case (1, y):\n        pass\n    case [a, *rest]:\n        pass\n    case (z,):\n        pass\n";
        assert_eq!(format_source(src, "t").unwrap(), src);
    }

    /// Formatting preserves the AST, is idempotent and keeps every comment.
    fn round_trip(src: &str) -> String {
        let out = fmt(src);
        let a = parser::parse(src, "t.fpy").unwrap();
        let b = parser::parse(&out, "t.fpy").unwrap_or_else(|d| panic!("reparse failed: {}\n{}", d, out));
        assert_eq!(ast_fingerprint(&a.module), ast_fingerprint(&b.module), "AST changed:\n{}", out);
        assert_eq!(fmt(&out), out, "not idempotent:\n{}", out);
        let nc = |s: &str| crate::lexer::lex(s, "x").map(|l| l.comments.len()).unwrap_or(0);
        assert_eq!(nc(src), nc(&out), "comments lost:\n{}", out);
        out
    }

    #[test]
    fn round_trips_statement_corpus() {
        let src = r#"#!/usr/bin/env python3
"""Module docstring."""
from __future__ import annotations
from . import sibling
from ..pkg import (a, b as bee)
from os import *
import os.path as osp, sys
type Pair[T] = tuple[T, T]
type Num = int | float
counter: int
def f(a, /, b: int = 1, *args: int, c, d=2, **kw: str) -> None:
    global counter
    x = +a; y = -b; z = ~c
    def g():
        nonlocal x
        x = yield
        yield from range(3)
        return (yield x)
    raise ValueError("bad") from None
def kwonly(*, k: int) -> int: return k
async def co(xs):
    async with lock as l, other:
        pass
    async for x in xs:
        await x
    return [y async for y in xs]
@decorator(arg=1)
@other
class C(Base, metaclass=Meta, flag=True):
    """Doc."""
    x: int = 0
    def m(self): return self.x
    @property
    def p(self) -> int:
        ...
while True:
    break
else:
    pass
for i in range(3):
    continue
else:
    print("done")
try:
    pass
except* ValueError as eg:
    pass
try:
    pass
except Exception:
    pass
else:
    pass
finally:
    pass
with (open("a") as fa, open("b") as fb):
    pass
del a, b[0], c.d
assert x, "message"
lambda: 0
lambda *a, **k: (a, k)
print(*args, **kwargs, sep="")
v = a if b else c
w = [*xs, *ys]
d = {**base, "k": 1}
s = {1, 2}
t = ()
u = (1,)
sl = xs[1:2, ::3]
e = (yield_ for yield_ in xs)
total = sum(x for x in xs)
pair = (x for x in xs), 1
n = (1).bit_length() + 1.0.real
chained = a < b <= c != d is not e not in f
q = not a and (b or c)
p = a ** -b
m = (lambda: 1)()
st = "implicit" "concat"
by = b"bytes" rb"raw"
fs = f"{x!r:>10} {y=} {'nested'}"
lst = [
    1,  # one
    2,
]
call(
    # leading comment
    a,
)
if a:  # trailing comment
    pass  # pass comment
# final comment
"#;
        round_trip(src);
    }

    #[test]
    fn round_trips_match_corpus() {
        let src = "match cmd:\n    case {\"k\": v, **rest}:\n        pass\n    case Point(1, y=2) as pt:\n        pass\n    case [1, 2] | (3 | 4) as n:\n        pass\n    case (a as b) as c:\n        pass\n    case -1 | 1.5 | 2j | \"s\" | None | True:\n        pass\n    case x.y.Z:\n        pass\n    case a, b:\n        pass\n    case ():\n        pass\n    case _ if guard:\n        pass\n";
        round_trip(src);
    }

    #[test]
    fn long_constructs_break() {
        let list = |n: usize, f: &dyn Fn(usize) -> String, sep: &str| (0..n).map(f).collect::<Vec<_>>().join(sep);
        let out = round_trip(&format!("def f({}) -> None:\n    pass\n", list(12, &|i| format!("argument_{}: int", i), ", ")));
        assert!(out.contains("(\n    argument_0: int,\n"), "{}", out);
        assert!(round_trip(&format!("result = function_name({})\n", list(15, &|i| format!("value_{}", i), ", "))).contains("function_name(\n"));
        round_trip(&format!("if {}:\n    pass\n", list(12, &|i| format!("condition_{}", i), " and ")));
        round_trip(&format!("def f() -> tuple[int, ...]:\n    return {}\n", list(20, &|i| format!("value_{}", i), ", ")));
        assert!(round_trip(&format!("from module import {}\n", list(20, &|i| format!("name_{}", i), ", "))).contains("from module import (\n"));
        round_trip(&format!("x = obj{}\n", list(15, &|i| format!(".method_{}()", i), "")));
        round_trip(&format!("x = table[{}]\n", list(20, &|i| format!("key_{}", i), ", ")));
        round_trip(&format!("f = lambda {}: 0\n", list(20, &|i| format!("p{}", i), ", ")));
        round_trip(&format!("class C({}):\n    pass\n", list(12, &|i| format!("BaseNumber{}", i), ", ")));
        round_trip(&format!("with {}:\n    pass\n", list(8, &|i| format!("open('file_{}') as handle_{}", i, i), ", ")));
        round_trip(&format!("x = {{{}}}\n", list(12, &|i| format!("'key_{}': {}", i, i), ", ")));
        round_trip(&format!("x = [{} for item in items if item]\n", list(10, &|i| format!("item.field_{}", i), " + ")));
        let long_str = format!("x = \"{}\"\n", "s".repeat(120));
        assert_eq!(round_trip(&long_str), long_str);
    }

    #[test]
    fn comments_inside_expressions_are_kept_verbatim() {
        let src = "x = [\n    1,  # one\n    2,  # two\n]\nif (a and  # why\n        b):\n    pass\ndef f(\n    a,  # first\n    b,\n):\n    pass\n";
        round_trip(src);
    }

    #[test]
    fn docstrings_and_blank_lines() {
        assert_eq!(fmt("'''doc'''\n"), "\"\"\"doc\"\"\"\n");
        assert_eq!(fmt(""), "");
        assert_eq!(fmt("# only a comment\n"), "# only a comment\n");
        assert_eq!(fmt("x = 1\n\n\n\n\ny = 2"), "x = 1\n\n\ny = 2\n");
        round_trip("class A:\n\n    x = 1\n\n\n\n    def f(self):\n        pass\n");
    }

    #[test]
    fn verify_reports_formatter_bugs() {
        assert_eq!(verify("x = 1\n", "t", true, format_source), Verify::Ok);
        assert_eq!(verify("def (:\n", "t", true, format_source), Verify::Skipped);
        fn fails(_: &str, f: &str) -> Result<String, crate::diag::Diag> {
            Err(crate::diag::Diag::new(f, crate::diag::Span::default(), "boom"))
        }
        fn garbage(_: &str, _: &str) -> Result<String, crate::diag::Diag> {
            Ok("def (:\n".into())
        }
        fn changes(s: &str, _: &str) -> Result<String, crate::diag::Diag> {
            Ok(s.replace('1', "2"))
        }
        fn grows(s: &str, _: &str) -> Result<String, crate::diag::Diag> {
            Ok(format!("{}\n", s))
        }
        fn drops_comments(s: &str, _: &str) -> Result<String, crate::diag::Diag> {
            Ok(s.lines().filter(|l| !l.starts_with('#')).map(|l| format!("{}\n", l)).collect())
        }
        let bad = |f: fn(&str, &str) -> Result<String, crate::diag::Diag>, src: &str| match verify(src, "t", true, f) {
            Verify::Bad(r) => r,
            other => panic!("{:?}", other),
        };
        assert!(bad(fails, "x = 1\n").starts_with("FORMAT-ERROR"));
        assert!(bad(garbage, "x = 1\n").starts_with("REPARSE-ERROR"));
        let r = bad(changes, "x = 1\n");
        assert!(r.starts_with("AST-CHANGED") && r.contains("--- formatted (line 1):\n  x = 2"), "{}", r);
        assert!(bad(grows, "x = 1\n").starts_with("NOT-IDEMPOTENT"));
        assert!(bad(drops_comments, "# c\nx = 1\n").starts_with("COMMENTS-LOST"));
    }
}

/// Canonical AST dump without positions or raw-source details (for equivalence checks).
pub fn ast_fingerprint(m: &Module) -> String {
    let dbg = format!("{:?}", m.body);
    let mut out: Vec<u8> = Vec::with_capacity(dbg.len());
    let b = dbg.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"span: Span {") {
            let j = b[i..].iter().position(|&c| c == b'}').unwrap();
            i += j + 1;
            if b[i..].starts_with(b", ") {
                i += 2;
            }
            continue;
        }
        if b[i..].starts_with(b"end_line: ") || b[i..].starts_with(b"header_end: ") || b[i..].starts_with(b"MatchCase { line: ") {
            if b[i..].starts_with(b"MatchCase { line: ") {
                out.extend_from_slice(b"MatchCase { ");
                i += 18;
            } else {
                i += if b[i..].starts_with(b"end_line: ") { 10 } else { 12 };
            }
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if b[i..].starts_with(b", ") {
                i += 2;
            }
            continue;
        }
        if b[i..].starts_with(b"raw: [") {
            // skip the bracketed list of Debug-quoted strings
            i += 6;
            let mut in_str = false;
            while i < b.len() {
                let c = b[i];
                if in_str {
                    if c == b'\\' {
                        i += 2;
                        continue;
                    }
                    if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b']' {
                    i += 1;
                    break;
                }
                i += 1;
            }
            if b[i..].starts_with(b", ") {
                i += 2;
            }
            continue;
        }
        if b[i..].starts_with(b"close_paren_pos: ") || b[i..].starts_with(b"colon_pos: ") {
            i += if b[i..].starts_with(b"colon_pos: ") { 11 } else { 17 };
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if b[i..].starts_with(b", ") {
                i += 2;
            }
            continue;
        }
        if b[i..].starts_with(b"header_lines: (") {
            let j = b[i..].iter().position(|&c| c == b')').unwrap();
            i += j + 1;
            if b[i..].starts_with(b", ") {
                i += 2;
            }
            continue;
        }
        // tuple paren flags are presentation-only
        if b[i..].starts_with(b"parens: true") || b[i..].starts_with(b"parens: false") {
            i += if b[i..].starts_with(b"parens: true") { 12 } else { 13 };
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()

}
