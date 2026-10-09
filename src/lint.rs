//! `fpython lint`: fast, compile-free checks for .fpy files.
//!
//! Codes:
//!   E001 syntax error                      F401 unused import
//!   F811 redefinition of unused name        F821 undefined name
//!   F841 local variable assigned but never used
//!   T001 parameter missing type annotation  T002 missing return annotation
//!   T003 explicit `Any` (dynamic typing opt-in)
//!   B001 bare `except:`                     B002 mutable default argument
//!   B003 comparison to None with ==/!=      B004 comparison to True/False with ==
//!   B005 assert on a tuple (always true)    B006 unreachable code
//!   B007 f-string without placeholders      B008 shadowing a builtin
//!   N801 class name not CapWords            N802 function name not snake_case
//!   W291 trailing whitespace                W191 tab indentation
//!   W292 no newline at end of file          E501 line too long
//!   I001 file would be reformatted by `fpython fmt`
//!
//! Suppress with `# noqa`, `# noqa: CODE[,CODE]` or `# fpy: ignore[CODE]` on the line.

use crate::ast::*;
use crate::compiler::scope;
use crate::diag::Span;
use crate::parser;
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Issue {
    pub line: usize,
    pub col: usize,
    pub code: &'static str,
    pub msg: String,
}

pub const BUILTINS: &[&str] = &[
    "print", "len", "range", "str", "int", "float", "bool", "abs", "min", "max", "sum", "sorted", "reversed", "enumerate", "zip",
    "isinstance", "issubclass", "input", "list", "dict", "set", "frozenset", "tuple", "repr", "ord", "chr", "hex", "oct", "bin",
    "round", "divmod", "pow", "any", "all", "map", "filter", "iter", "next", "hash", "id", "super", "type", "callable", "format",
    "bytes", "bytearray", "object", "open", "exit", "quit", "getattr", "setattr", "hasattr", "delattr", "vars", "dir", "globals",
    "locals", "slice", "staticmethod", "classmethod", "property", "memoryview", "complex", "ascii", "breakpoint", "compile", "eval",
    "exec", "help", "NotImplemented", "Ellipsis", "__name__", "__file__", "__doc__", "__debug__", "BaseException", "Exception",
    "SystemExit", "KeyboardInterrupt", "GeneratorExit", "ArithmeticError", "ZeroDivisionError", "OverflowError", "LookupError",
    "IndexError", "KeyError", "ValueError", "UnicodeError", "TypeError", "AttributeError", "NameError", "RuntimeError",
    "NotImplementedError", "RecursionError", "AssertionError", "StopIteration", "OSError", "FileNotFoundError", "FileExistsError",
    "PermissionError", "TimeoutError", "ImportError", "ModuleNotFoundError", "MemoryError", "EOFError", "IOError", "Warning",
    "DeprecationWarning", "UserWarning", "cast", "TextFile", "__builtins__", "__spec__", "__package__", "__loader__",
    "UnboundLocalError", "BufferError", "ConnectionError", "BrokenPipeError", "ProcessLookupError", "StopAsyncIteration",
    "IsADirectoryError", "NotADirectoryError", "InterruptedError", "ChildProcessError", "BlockingIOError", "EnvironmentError",
    "UnicodeDecodeError", "UnicodeEncodeError", "SyntaxError", "IndentationError", "TabError", "SystemError", "ReferenceError",
    "FloatingPointError", "RuntimeWarning", "FutureWarning", "PendingDeprecationWarning", "SyntaxWarning", "ImportWarning",
    "UnicodeWarning", "BytesWarning", "ResourceWarning", "EncodingWarning", "aiter", "anext", "BaseExceptionGroup", "ExceptionGroup",
    "__import__", "__build_class__", "copyright", "credits", "license",
];

const SHADOW_WARN: &[&str] = &["list", "dict", "str", "int", "float", "set", "tuple", "type", "id", "len", "max", "min", "sum", "input", "format", "filter", "map", "bytes", "hash", "iter", "next", "object", "print", "range", "sorted", "zip", "open", "all", "any"];

pub struct LintOptions {
    pub max_line: usize,
    pub check_format: bool,
}

impl Default for LintOptions {
    fn default() -> Self {
        LintOptions { max_line: crate::fmt::MAX_WIDTH, check_format: true }
    }
}

pub fn lint_source(src: &str, file: &str, opts: &LintOptions) -> Vec<Issue> {
    let mut issues = Vec::new();
    // text-level checks
    for (i, line) in src.lines().enumerate() {
        let n = i + 1;
        if line.ends_with(' ') || line.ends_with('\t') {
            issues.push(Issue { line: n, col: line.trim_end().len() + 1, code: "W291", msg: "trailing whitespace".into() });
        }
        if line.starts_with('\t') {
            issues.push(Issue { line: n, col: 1, code: "W191", msg: "indentation contains tabs".into() });
        }
        let w = line.chars().count();
        if w > opts.max_line {
            issues.push(Issue { line: n, col: opts.max_line + 1, code: "E501", msg: format!("line too long ({} > {} characters)", w, opts.max_line) });
        }
    }
    if !src.is_empty() && !src.ends_with('\n') {
        issues.push(Issue { line: src.lines().count(), col: 1, code: "W292", msg: "no newline at end of file".into() });
    }
    let parsed = match parser::parse(src, file) {
        Ok(p) => p,
        Err(d) => {
            issues.push(Issue { line: d.span.line, col: d.span.col, code: "E001", msg: d.msg });
            return finish(issues, src);
        }
    };
    let mut l = Linter { issues: Vec::new(), module_names: HashSet::new(), imported: HashMap::new(), used_globals: HashSet::new(), defined_at: HashMap::new(), type_vars: Vec::new() };
    l.module(&parsed.module);
    issues.extend(l.issues);
    if opts.check_format {
        if let Ok(out) = crate::fmt::format_source(src, file) {
            if out != src {
                issues.push(Issue { line: 1, col: 1, code: "I001", msg: "file would be reformatted by `fpython fmt`".into() });
            }
        }
    }
    finish(issues, src)
}

fn finish(mut issues: Vec<Issue>, src: &str) -> Vec<Issue> {
    let lines: Vec<&str> = src.lines().collect();
    issues.retain(|i| !suppressed(lines.get(i.line.saturating_sub(1)).copied().unwrap_or(""), i.code));
    issues.sort();
    issues.dedup();
    issues
}

fn suppressed(line: &str, code: &str) -> bool {
    let Some(pos) = line.find('#') else { return false };
    let c = &line[pos..];
    if let Some(i) = c.find("noqa") {
        let rest = &c[i + 4..];
        if !rest.trim_start().starts_with(':') {
            return true;
        }
        return rest.split(|ch: char| ch == ',' || ch == ':' || ch.is_whitespace()).any(|t| t == code);
    }
    if let Some(i) = c.find("fpy: ignore") {
        let rest = &c[i + 11..];
        if !rest.starts_with('[') {
            return true;
        }
        return rest.trim_start_matches('[').split(|ch: char| ch == ',' || ch == ']').any(|t| t.trim() == code);
    }
    false
}

struct Linter {
    issues: Vec<Issue>,
    module_names: HashSet<String>,
    /// imported name -> span
    imported: HashMap<String, Span>,
    used_globals: HashSet<String>,
    defined_at: HashMap<String, Span>,
    type_vars: Vec<HashSet<String>>,
}

fn is_snake(s: &str) -> bool {
    let t = s.trim_matches('_');
    t.is_empty() || (t.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') && !t.contains("__"))
}

fn is_capwords(s: &str) -> bool {
    let t = s.trim_start_matches('_');
    t.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(true) && !t.contains('_')
}

impl Linter {
    fn push(&mut self, span: Span, code: &'static str, msg: impl Into<String>) {
        self.issues.push(Issue { line: span.line.max(1), col: span.col.max(1), code, msg: msg.into() });
    }

    fn module(&mut self, m: &Module) {
        // collect module-level bindings
        for st in &m.body {
            match &st.kind {
                StmtKind::FuncDef(f) => {
                    self.redef(&f.name, st.span);
                    self.module_names.insert(f.name.clone());
                }
                StmtKind::ClassDef(c) => {
                    self.redef(&c.name, st.span);
                    self.module_names.insert(c.name.clone());
                }
                StmtKind::Import(names) => {
                    for a in names {
                        let n = a.asname.clone().unwrap_or_else(|| a.name.split('.').next().unwrap().to_string());
                        self.module_names.insert(n.clone());
                        self.imported.insert(n, st.span);
                    }
                }
                StmtKind::ImportFrom { names, module, .. } => {
                    for a in names {
                        if a.name == "*" {
                            continue;
                        }
                        let n = a.asname.clone().unwrap_or_else(|| a.name.clone());
                        self.module_names.insert(n.clone());
                        if module != "__future__" {
                            self.imported.insert(n, st.span);
                        }
                    }
                }
                StmtKind::TypeAlias { name, .. } => {
                    self.module_names.insert(name.clone());
                }
                _ => {
                    let mut names = BTreeSet::new();
                    crate::compiler::collect_assigned(st, &mut names);
                    for n in names {
                        self.module_names.insert(n);
                    }
                }
            }
        }
        // names bound anywhere at module level (inside if/try/with/for too)
        let msc = scope::analyze_function(&Params::default(), &m.body);
        for n in msc.locals.iter().chain(msc.globals.iter()) {
            self.module_names.insert(n.clone());
        }
        for st in &m.body {
            collect_nested_imports(st, &mut self.imported);
        }
        fn has_star(body: &[Stmt]) -> bool {
            body.iter().any(|s| match &s.kind {
                StmtKind::ImportFrom { names, .. } => names.iter().any(|a| a.name == "*"),
                StmtKind::If { body, orelse, .. } => has_star(body) || has_star(orelse),
                StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                    has_star(body) || has_star(orelse) || has_star(finalbody) || handlers.iter().any(|h| has_star(&h.body))
                }
                _ => false,
            })
        }
        let star_import = has_star(&m.body);
        // `__all__` counts as using names
        for st in &m.body {
            if let StmtKind::Assign { targets, value } = &st.kind {
                if targets.iter().any(|t| matches!(&t.kind, ExprKind::Name(n) if n == "__all__")) {
                    if let ExprKind::List(items) | ExprKind::Tuple { elts: items, .. } = &value.kind {
                        for i in items {
                            if let ExprKind::Str { value, .. } = &i.kind {
                                self.used_globals.insert(value.clone());
                            }
                        }
                    }
                }
            }
        }
        let globals: HashSet<String> = self.module_names.clone();
        self.block(&m.body, &globals, &HashSet::new(), star_import, true);
        let mut unused: Vec<(String, Span)> = self.imported.iter().filter(|(n, _)| !self.used_globals.contains(*n)).map(|(n, s)| (n.clone(), *s)).collect();
        unused.sort_by_key(|(_, s)| (s.line, s.col));
        for (n, s) in unused {
            if n == "annotations" || n.starts_with('_') && n != "_" {
                continue;
            }
            self.push(s, "F401", format!("'{}' imported but unused", n));
        }
    }

    fn redef(&mut self, name: &str, span: Span) {
        if let Some(prev) = self.defined_at.get(name) {
            let p = prev.line;
            self.push(span, "F811", format!("redefinition of '{}' from line {}", name, p));
        }
        self.defined_at.insert(name.to_string(), span);
    }

    fn block(&mut self, body: &[Stmt], globals: &HashSet<String>, locals: &HashSet<String>, star: bool, module_level: bool) {
        let mut terminated = false;
        for st in body {
            if terminated {
                self.push(st.span, "B006", "unreachable code");
                terminated = false;
            }
            self.stmt(st, globals, locals, star, module_level);
            if matches!(st.kind, StmtKind::Return(_) | StmtKind::Raise { .. } | StmtKind::Break | StmtKind::Continue) {
                terminated = true;
            }
        }
    }

    fn func(&mut self, f: &FuncDef, span: Span, globals: &HashSet<String>, outer_locals: &HashSet<String>, star: bool, is_method: bool) {
        self.type_vars.push(f.type_params.iter().map(|t| t.name.clone()).collect());
        self.func_inner(f, span, globals, outer_locals, star, is_method);
        self.type_vars.pop();
    }

    fn func_inner(&mut self, f: &FuncDef, span: Span, globals: &HashSet<String>, outer_locals: &HashSet<String>, star: bool, is_method: bool) {
        if !is_snake(&f.name) && !f.name.starts_with("__") && !f.decorators.iter().any(|d| matches!(&d.kind, ExprKind::Name(n) if n == "overload")) {
            self.push(span, "N802", format!("function name '{}' should be snake_case", f.name));
        }
        for (i, a) in f.params.all_positional().chain(f.params.kwonly.iter()).enumerate() {
            let is_self = is_method && i == 0 && (a.name == "self" || a.name == "cls");
            if a.ann.is_none() && !is_self {
                self.push(a.span, "T001", format!("parameter '{}' is missing a type annotation", a.name));
            }
            if let Some(d) = &a.default {
                if matches!(d.kind, ExprKind::List(_) | ExprKind::Dict(_) | ExprKind::Set(_)) {
                    self.push(d.span, "B002", format!("mutable default argument for '{}'; use None and create it inside the function", a.name));
                }
                self.expr(d, globals, outer_locals, star);
            }
            if let Some(ann) = &a.ann {
                self.annotation(ann, globals, star);
            }
        }
        for a in f.params.vararg.iter().chain(f.params.kwarg.iter()) {
            if a.ann.is_none() {
                self.push(a.span, "T001", format!("parameter '{}' is missing a type annotation", a.name));
            }
        }
        match &f.returns {
            None if f.name != "__init__" => self.push(span, "T002", format!("function '{}' is missing a return type annotation", f.name)),
            Some(r) => self.annotation(r, globals, star),
            _ => {}
        }
        for d in &f.decorators {
            self.expr(d, globals, outer_locals, star);
        }
        let sc = scope::analyze_function(&f.params, &f.body);
        let mut locals: HashSet<String> = sc.locals.iter().cloned().collect();
        locals.extend(outer_locals.iter().cloned());
        // nested function scopes see enclosing locals
        self.block(&f.body, globals, &locals, star, false);
        // unused locals: assigned but never loaded
        let mut loads = HashSet::new();
        collect_loads(&f.body, &mut loads);
        let params: HashSet<&str> = f.params.all_positional().chain(f.params.kwonly.iter()).chain(f.params.vararg.iter()).chain(f.params.kwarg.iter()).map(|a| a.name.as_str()).collect();
        let mut assigned: Vec<(String, Span)> = Vec::new();
        collect_simple_assigns(&f.body, &mut assigned);
        let mut seen = HashSet::new();
        for (n, sp) in assigned {
            if params.contains(n.as_str()) || loads.contains(&n) || n.starts_with('_') || sc.globals.contains(&n) || sc.nonlocals.contains(&n) || sc.cells.contains(&n) {
                continue;
            }
            if seen.insert(n.clone()) {
                self.push(sp, "F841", format!("local variable '{}' is assigned to but never used", n));
            }
        }
    }

    fn annotation(&mut self, e: &Expr, globals: &HashSet<String>, star: bool) {
        // names inside annotations count as uses; string annotations are skipped
        let mut names = Vec::new();
        expr_names(e, &mut names);
        for (n, sp) in names {
            if n == "Any" {
                self.push(sp, "T003", "explicit 'Any' disables static checking for this value");
            }
            self.use_name(&n, sp, globals, &HashSet::new(), star);
        }
    }

    fn use_name(&mut self, n: &str, sp: Span, globals: &HashSet<String>, locals: &HashSet<String>, star: bool) {
        if locals.contains(n) || self.type_vars.iter().any(|t| t.contains(n)) {
            return;
        }
        if globals.contains(n) {
            self.used_globals.insert(n.to_string());
            return;
        }
        if BUILTINS.contains(&n) || star || is_typing_name(n) {
            return;
        }
        self.push(sp, "F821", format!("undefined name '{}'", n));
    }

    fn stmt(&mut self, st: &Stmt, globals: &HashSet<String>, locals: &HashSet<String>, star: bool, module_level: bool) {
        match &st.kind {
            StmtKind::FuncDef(f) => self.func(f, st.span, globals, locals, star, false),
            StmtKind::ClassDef(c) => {
                if !is_capwords(&c.name) {
                    self.push(st.span, "N801", format!("class name '{}' should use CapWords", c.name));
                }
                for b in &c.bases {
                    self.expr(b, globals, locals, star);
                }
                for d in &c.decorators {
                    self.expr(d, globals, locals, star);
                }
                // class body: methods and attribute annotations
                self.type_vars.push(c.type_params.iter().map(|t| t.name.clone()).collect());
                let mut class_names: HashSet<String> = locals.clone();
                for s in &c.body {
                    let mut ns = BTreeSet::new();
                    crate::compiler::collect_assigned(s, &mut ns);
                    class_names.extend(ns);
                    match &s.kind {
                        StmtKind::FuncDef(f) => {
                            class_names.insert(f.name.clone());
                        }
                        StmtKind::ClassDef(c2) => {
                            class_names.insert(c2.name.clone());
                        }
                        _ => {}
                    }
                }
                for s in &c.body {
                    match &s.kind {
                        StmtKind::FuncDef(f) => {
                            let is_static = f.decorators.iter().any(|d| matches!(&d.kind, ExprKind::Name(n) if n == "staticmethod"));
                            self.func(f, s.span, globals, locals, star, !is_static)
                        }
                        StmtKind::AnnAssign { ann, value, .. } => {
                            self.annotation(ann, globals, star);
                            if let Some(v) = value {
                                self.expr(v, globals, &class_names, star);
                            }
                        }
                        _ => self.stmt(s, globals, &class_names, star, false),
                    }
                }
                self.type_vars.pop();
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                self.block(body, globals, locals, star, module_level);
                for h in handlers {
                    if h.typ.is_none() {
                        self.push(h.span, "B001", "bare 'except:' catches SystemExit and KeyboardInterrupt; use 'except Exception:'");
                    }
                    if let Some(t) = &h.typ {
                        self.expr(t, globals, locals, star);
                    }
                    let mut l2 = locals.clone();
                    if let Some(n) = &h.name {
                        l2.insert(n.clone());
                    }
                    self.block(&h.body, globals, &l2, star, module_level);
                }
                self.block(orelse, globals, locals, star, module_level);
                self.block(finalbody, globals, locals, star, module_level);
            }
            StmtKind::Assert { test, msg } => {
                if let ExprKind::Tuple { elts, .. } = &test.kind {
                    if !elts.is_empty() {
                        self.push(test.span, "B005", "assert on a non-empty tuple is always true");
                    }
                }
                self.expr(test, globals, locals, star);
                if let Some(m) = msg {
                    self.expr(m, globals, locals, star);
                }
            }
            StmtKind::AnnAssign { target, ann, value } => {
                self.annotation(ann, globals, star);
                self.target(target, globals, locals, star, module_level);
                if let Some(v) = value {
                    self.expr(v, globals, locals, star);
                }
            }
            StmtKind::Assign { targets, value } => {
                for t in targets {
                    self.target(t, globals, locals, star, module_level);
                }
                self.expr(value, globals, locals, star);
            }
            StmtKind::For { target, iter, body, orelse, .. } => {
                self.target(target, globals, locals, star, module_level);
                self.expr(iter, globals, locals, star);
                self.block(body, globals, locals, star, module_level);
                self.block(orelse, globals, locals, star, module_level);
            }
            StmtKind::Import(_) | StmtKind::ImportFrom { .. } | StmtKind::Global(_) | StmtKind::Nonlocal(_) | StmtKind::Pass | StmtKind::Break | StmtKind::Continue => {}
            StmtKind::Match { subject, cases } => {
                self.expr(subject, globals, locals, star);
                for c in cases {
                    let mut l2 = locals.clone();
                    let mut names = BTreeSet::new();
                    pattern_bindings(&c.pattern, &mut names);
                    l2.extend(names);
                    pattern_uses(&c.pattern, &mut |e| self.expr(e, globals, &l2, star));
                    if let Some(g) = &c.guard {
                        self.expr(g, globals, &l2, star);
                    }
                    self.block(&c.body, globals, &l2, star, module_level);
                }
            }
            _ => {
                let mut nodes = Vec::new();
                scope::walk_stmt(st, &mut |n| nodes.push(n));
                for n in nodes {
                    match n {
                        scope::Node::E(e) => self.expr(e, globals, locals, star),
                        scope::Node::B(b) => self.block(b, globals, locals, star, module_level),
                    }
                }
            }
        }
    }

    fn target(&mut self, t: &Expr, globals: &HashSet<String>, locals: &HashSet<String>, star: bool, module_level: bool) {
        match &t.kind {
            ExprKind::Name(n) => {
                if SHADOW_WARN.contains(&n.as_str()) {
                    self.push(t.span, "B008", format!("variable '{}' shadows a builtin", n));
                }
                let _ = module_level;
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                for e in elts {
                    self.target(e, globals, locals, star, module_level);
                }
            }
            ExprKind::Starred(x) => self.target(x, globals, locals, star, module_level),
            _ => self.expr(t, globals, locals, star),
        }
    }

    fn expr(&mut self, e: &Expr, globals: &HashSet<String>, locals: &HashSet<String>, star: bool) {
        match &e.kind {
            ExprKind::Name(n) => self.use_name(n, e.span, globals, locals, star),
            ExprKind::Compare { left, ops, comparators } => {
                for (op, c) in ops.iter().zip(comparators) {
                    if matches!(op, CmpOp::Eq | CmpOp::NotEq) {
                        if matches!(c.kind, ExprKind::NoneLit) || matches!(left.kind, ExprKind::NoneLit) {
                            self.push(e.span, "B003", "comparison to None should use 'is' / 'is not'");
                        }
                        if matches!(c.kind, ExprKind::Bool(_)) {
                            self.push(e.span, "B004", "comparison to True/False; use the value directly");
                        }
                    }
                }
                self.expr(left, globals, locals, star);
                for c in comparators {
                    self.expr(c, globals, locals, star);
                }
            }
            ExprKind::FStr { parts, .. } => {
                if !parts.iter().any(|p| matches!(p, FStrPart::Expr { .. })) {
                    self.push(e.span, "B007", "f-string without any placeholders");
                }
                scope::walk_children(e, &mut |c| self.expr(c, globals, locals, star));
            }
            ExprKind::Lambda { params, body } => {
                let mut l2 = locals.clone();
                for a in params.all_positional().chain(params.kwonly.iter()) {
                    l2.insert(a.name.clone());
                }
                self.expr(body, globals, &l2, star);
            }
            ExprKind::ListComp { elt, gens } | ExprKind::SetComp { elt, gens } | ExprKind::GenExp { elt, gens } => {
                let l2 = self.comp_locals(gens, globals, locals, star);
                self.expr(elt, globals, &l2, star);
            }
            ExprKind::DictComp { key, value, gens } => {
                let l2 = self.comp_locals(gens, globals, locals, star);
                self.expr(key, globals, &l2, star);
                self.expr(value, globals, &l2, star);
            }
            _ => scope::walk_children(e, &mut |c| self.expr(c, globals, locals, star)),
        }
    }

    fn comp_locals(&mut self, gens: &[Comprehension], globals: &HashSet<String>, locals: &HashSet<String>, star: bool) -> HashSet<String> {
        let mut l2 = locals.clone();
        for g in gens {
            self.expr(&g.iter, globals, &l2, star);
            let mut ns = BTreeSet::new();
            scope::collect_target_names(&g.target, &mut ns);
            l2.extend(ns);
            for c in &g.ifs {
                self.expr(c, globals, &l2, star);
            }
        }
        l2
    }
}

/// Imports nested in module-level if/try blocks (top-level ones are handled directly).
fn collect_nested_imports(st: &Stmt, out: &mut HashMap<String, Span>) {
    match &st.kind {
        StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
            for s in body.iter().chain(orelse) {
                collect_imports_in(s, out);
            }
        }
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            for s in body.iter().chain(orelse).chain(finalbody) {
                collect_imports_in(s, out);
            }
            for h in handlers {
                for s in &h.body {
                    collect_imports_in(s, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_imports_in(st: &Stmt, out: &mut HashMap<String, Span>) {
    match &st.kind {
        StmtKind::Import(names) => {
            for a in names {
                let n = a.asname.clone().unwrap_or_else(|| a.name.split('.').next().unwrap().to_string());
                // conditional imports are commonly re-exports; don't report them as unused
                let _ = out.entry(n);
            }
        }
        _ => collect_nested_imports(st, out),
    }
}

fn is_typing_name(n: &str) -> bool {
    matches!(n, "Any" | "Iterator" | "Iterable" | "Callable" | "Optional" | "Union" | "Generator" | "Sequence" | "Mapping" | "Self" | "Never" | "NoReturn" | "Final" | "ClassVar" | "Literal" | "TypeAlias")
}

fn expr_names(e: &Expr, out: &mut Vec<(String, Span)>) {
    match &e.kind {
        ExprKind::Name(n) => out.push((n.clone(), e.span)),
        ExprKind::Str { .. } => {}
        _ => scope::walk_children(e, &mut |c| expr_names(c, out)),
    }
}

fn collect_loads(body: &[Stmt], out: &mut HashSet<String>) {
    fn ex(e: &Expr, out: &mut HashSet<String>) {
        match &e.kind {
            ExprKind::Name(n) => {
                out.insert(n.clone());
            }
            ExprKind::Lambda { body, .. } => ex(body, out),
            _ => scope::walk_children(e, &mut |c| ex(c, out)),
        }
    }
    for st in body {
        match &st.kind {
            StmtKind::Assign { targets, value } => {
                for t in targets {
                    if !matches!(t.kind, ExprKind::Name(_) | ExprKind::Tuple { .. } | ExprKind::List(_)) {
                        ex(t, out);
                    }
                    if let ExprKind::Tuple { elts, .. } | ExprKind::List(elts) = &t.kind {
                        for x in elts {
                            if !matches!(x.kind, ExprKind::Name(_)) {
                                ex(x, out);
                            }
                        }
                    }
                }
                ex(value, out);
            }
            StmtKind::AnnAssign { target, value, ann } => {
                if !matches!(target.kind, ExprKind::Name(_)) {
                    ex(target, out);
                }
                ex(ann, out);
                if let Some(v) = value {
                    ex(v, out);
                }
            }
            StmtKind::AugAssign { target, value, .. } => {
                ex(target, out);
                ex(value, out);
            }
            StmtKind::FuncDef(f) => {
                collect_loads(&f.body, out);
                for d in &f.decorators {
                    ex(d, out);
                }
                for a in f.params.all_positional() {
                    if let Some(d) = &a.default {
                        ex(d, out);
                    }
                }
            }
            StmtKind::ClassDef(c) => collect_loads(&c.body, out),
            StmtKind::Nonlocal(ns) | StmtKind::Global(ns) => {
                for n in ns {
                    out.insert(n.clone());
                }
            }
            _ => {
                let mut nodes = Vec::new();
                scope::walk_stmt(st, &mut |n| nodes.push(n));
                for n in nodes {
                    match n {
                        scope::Node::E(e) => ex(e, out),
                        scope::Node::B(b) => collect_loads(b, out),
                    }
                }
            }
        }
    }
}

fn collect_simple_assigns(body: &[Stmt], out: &mut Vec<(String, Span)>) {
    for st in body {
        match &st.kind {
            StmtKind::Assign { targets, .. } => {
                for t in targets {
                    if let ExprKind::Name(n) = &t.kind {
                        out.push((n.clone(), t.span));
                    }
                }
            }
            StmtKind::AnnAssign { target, value: Some(_), .. } => {
                if let ExprKind::Name(n) = &target.kind {
                    out.push((n.clone(), target.span));
                }
            }
            StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
                collect_simple_assigns(body, out);
                collect_simple_assigns(orelse, out);
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                collect_simple_assigns(body, out);
                for h in handlers {
                    collect_simple_assigns(&h.body, out);
                }
                collect_simple_assigns(orelse, out);
                collect_simple_assigns(finalbody, out);
            }
            StmtKind::With { body, .. } => collect_simple_assigns(body, out),
            _ => {}
        }
    }
}

fn pattern_bindings(p: &Pattern, out: &mut BTreeSet<String>) {
    match p {
        Pattern::Capture(n) => {
            out.insert(n.clone());
        }
        Pattern::As(p, n) => {
            out.insert(n.clone());
            pattern_bindings(p, out);
        }
        Pattern::Or(ps) | Pattern::Sequence(ps, _) => ps.iter().for_each(|p| pattern_bindings(p, out)),
        Pattern::Star(Some(n)) => {
            out.insert(n.clone());
        }
        Pattern::Class { args, kwargs, .. } => {
            args.iter().for_each(|p| pattern_bindings(p, out));
            kwargs.iter().for_each(|(_, p)| pattern_bindings(p, out));
        }
        Pattern::Mapping { values, rest, .. } => {
            values.iter().for_each(|p| pattern_bindings(p, out));
            if let Some(r) = rest {
                out.insert(r.clone());
            }
        }
        _ => {}
    }
}

fn pattern_uses(p: &Pattern, f: &mut dyn FnMut(&Expr)) {
    match p {
        Pattern::Value(e) => f(e),
        Pattern::As(p, _) => pattern_uses(p, f),
        Pattern::Or(ps) | Pattern::Sequence(ps, _) => ps.iter().for_each(|p| pattern_uses(p, f)),
        Pattern::Class { cls, args, kwargs } => {
            f(cls);
            args.iter().for_each(|p| pattern_uses(p, f));
            kwargs.iter().for_each(|(_, p)| pattern_uses(p, f));
        }
        Pattern::Mapping { keys, values, .. } => {
            keys.iter().for_each(|k| f(k));
            values.iter().for_each(|p| pattern_uses(p, f));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(src: &str) -> Vec<&'static str> {
        lint_source(src, "t.fpy", &LintOptions { check_format: false, ..Default::default() }).into_iter().map(|i| i.code).collect()
    }

    #[test]
    fn clean_file() {
        assert!(codes("import math\n\n\ndef f(x: float) -> float:\n    return math.sqrt(x)\n").is_empty());
    }

    #[test]
    fn unused_import_and_undefined() {
        let c = codes("import os\n\n\ndef f() -> int:\n    return y\n");
        assert!(c.contains(&"F401"));
        assert!(c.contains(&"F821"));
    }

    #[test]
    fn typing_rules() {
        let c = codes("def f(x, y: int = []):\n    pass\n");
        assert!(c.contains(&"T001"));
        assert!(c.contains(&"T002"));
        assert!(c.contains(&"B002"));
    }

    #[test]
    fn bug_patterns() {
        let src = "def f(x: int) -> None:\n    try:\n        pass\n    except:\n        pass\n    if x == None:\n        return\n    assert (x, 'msg')\n    s = f'plain'\n    print(s)\n    return\n    print(1)\n";
        let c = codes(src);
        for code in ["B001", "B003", "B005", "B007", "B006"] {
            assert!(c.contains(&code), "missing {}: {:?}", code, c);
        }
    }

    #[test]
    fn unused_local_and_names() {
        let c = codes("class bad_name:\n    pass\n\n\ndef BadFunc() -> None:\n    unused = 1\n");
        assert!(c.contains(&"N801"));
        assert!(c.contains(&"N802"));
        assert!(c.contains(&"F841"));
    }

    #[test]
    fn suppression() {
        assert!(!codes("import os  # noqa\n").contains(&"F401"));
        assert!(!codes("import os  # noqa: F401\n").contains(&"F401"));
        assert!(codes("import os  # noqa: E501\n").contains(&"F401"));
        assert!(!codes("import os  # fpy: ignore[F401]\n").contains(&"F401"));
    }

    #[test]
    fn whitespace_rules() {
        let c = codes("x = 1   \n\ty = 2");
        assert!(c.contains(&"W291"));
        assert!(c.contains(&"W292"));
        assert!(c.contains(&"E001") || c.contains(&"W191"));
    }

    #[test]
    fn syntax_error() {
        assert_eq!(codes("def (:\n"), vec!["E001"]);
    }

    fn issues(src: &str) -> Vec<(usize, &'static str)> {
        lint_source(src, "t.fpy", &LintOptions { check_format: false, ..Default::default() }).into_iter().map(|i| (i.line, i.code)).collect()
    }

    /// A program touching every statement and expression kind the linter walks; only the lines
    /// marked below should be reported.
    #[test]
    fn walks_all_constructs() {
        let src = r#"from __future__ import annotations
from os.path import join as pjoin, basename
from typing import Any
import collections.abc

__all__ = ["exported", "Point"]
type Pair = tuple[int, int]


def exported() -> None:
    pass


class Point:
    origin: int = 0
    label = "p"

    def __init__(self, x: int) -> None:
        self.x = x

    @staticmethod
    def make(v: int) -> Point:
        return Point(v + Point.origin)

    @classmethod
    def unit(cls) -> Point:
        return cls(1)

    class Inner:
        pass

    def area(self) -> int:
        return self.x * self.x


def walk(items: list[int], d: dict[str, int], v: Any) -> int:
    total = 0
    count = 0
    first, *rest = items
    (a, b) = (1, 2)
    [c, e] = [3, 4]
    total += first + len(rest) + a + b + c + e
    for i, x in enumerate(items):
        if x == True:
            continue
        total += i
    else:
        pass
    while count < 3:
        count += 1
    with open(pjoin("a", "b")) as fh:
        total += len(fh.read())
    try:
        total += d["k"]
    except KeyError as err:
        print(err)
    finally:
        pass
    match items:
        case [1, *others] if others:
            total += len(others)
        case {"k": kv, **more}:
            total += kv + len(more)
        case Point(x=px) | Point(x=px):
            total += px
        case (1 | 2) as small:
            total += small
        case _:
            pass
    f = lambda q, r=2: q + r
    sq = [n * n for n in items if n]
    st = {n for n in items}
    dc = {k: n for k, n in d.items()}
    gen = sum(n for n in items)
    total += f(1) + len(sq) + len(st) + len(dc) + gen
    obj: Point = Point.make(2)
    obj.x += 1
    d["k"] += 1
    del d["k"]
    total += obj.x + collections.abc.__name__.__len__() + len(basename("x"))
    return total + v


def scopes() -> int:
    n = 0

    def inc() -> None:
        nonlocal n
        n += 1

    inc()
    return n


counter = 0


def bump() -> None:
    global counter
    counter += 1


def dup() -> None:
    pass


def dup() -> None:
    pass


def shadows(list: int) -> int:
    id = list
    return id
"#;
        let got = issues(src);
        let codes: Vec<&str> = got.iter().map(|x| x.1).collect();
        assert!(got.contains(&(44, "B004")), "{:?}", got); // x == True
        assert!(codes.contains(&"T003"), "{:?}", got); // explicit Any
        assert!(codes.contains(&"F811"), "{:?}", got); // dup redefined
        assert!(codes.contains(&"B008"), "{:?}", got); // `id` shadows a builtin
        for bad in ["F401", "F821", "F841", "E001"] {
            assert!(!codes.contains(&bad), "unexpected {}: {:?}", bad, got);
        }
    }

    #[test]
    fn star_imports_disable_undefined_names() {
        assert!(!codes("from os import *\n\nprint(getcwd())\n").contains(&"F821"));
        let nested = "try:\n    from os import *\nexcept ImportError:\n    pass\nif True:\n    from sys import *\n\nprint(getcwd())\n";
        assert!(!codes(nested).contains(&"F821"));
    }

    #[test]
    fn nested_imports_and_unused_locals() {
        let src = "def f() -> None:\n    if True:\n        import json\n    try:\n        import os\n    except ImportError:\n        pass\n    for _ in range(1):\n        import sys\n    while False:\n        pass\n    with open('x') as fh:\n        y: int = 1\n    print(json, os, sys, fh)\n";
        let c = issues(src);
        assert!(c.contains(&(13, "F841")), "{:?}", c);
        assert!(!c.iter().any(|x| x.1 == "F401"), "{:?}", c);
    }

    #[test]
    fn unannotated_varargs_and_format_check() {
        let c = codes("def f(*args, **kw) -> None:\n    pass\n");
        assert_eq!(c.iter().filter(|c| **c == "T001").count(), 2);
        let opts = LintOptions::default();
        let i = lint_source("x=1\n", "t.fpy", &opts);
        assert!(i.iter().any(|i| i.code == "I001"));
        assert!(lint_source("x = 1\n", "t.fpy", &opts).is_empty());
        assert!(lint_source("x = 1\n", "t.fpy", &LintOptions { max_line: 3, check_format: false }).iter().any(|i| i.code == "E501"));
    }

    #[test]
    fn suppression_forms() {
        assert!(suppressed("x  # noqa", "F401"));
        assert!(suppressed("x  # noqa: F401, E501", "E501"));
        assert!(!suppressed("x  # noqa: F401", "E501"));
        assert!(suppressed("x  # fpy: ignore", "T003"));
        assert!(suppressed("x  # fpy: ignore[T003, F401]", "F401"));
        assert!(!suppressed("x  # fpy: ignore[T003]", "F401"));
        assert!(!suppressed("x  # other comment", "F401"));
        assert!(!suppressed("x", "F401"));
    }
}
