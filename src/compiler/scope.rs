//! Name-binding analysis for functions: locals, globals, captured cells, free variables.

use crate::ast::*;
use std::collections::BTreeSet;

#[derive(Debug, Default, Clone)]
pub struct Scope {
    pub locals: BTreeSet<String>,
    pub globals: BTreeSet<String>,
    pub nonlocals: BTreeSet<String>,
    /// Locals captured by nested functions (must live in heap cells).
    pub cells: BTreeSet<String>,
    /// Names referenced here (or by nested scopes) that are not bound here.
    pub frees: BTreeSet<String>,
    pub has_yield: bool,
    /// Locals assigned inside a `try` body (need `volatile` because of setjmp).
    pub try_assigned: BTreeSet<String>,
    pub has_try: bool,
}

pub fn analyze_function(params: &Params, body: &[Stmt]) -> Scope {
    let mut s = Scope::default();
    for a in params.all_positional().chain(params.kwonly.iter()).chain(params.vararg.iter()).chain(params.kwarg.iter()) {
        s.locals.insert(a.name.clone());
    }
    let mut b = Binder { s: &mut s, in_try: 0 };
    for st in body {
        b.stmt(st);
    }
    for g in s.globals.clone().iter().chain(s.nonlocals.clone().iter()) {
        s.locals.remove(g);
    }
    let mut u = Uses { s: &mut s, comp_bound: Vec::new() };
    for st in body {
        u.stmt(st);
    }
    s
}

pub fn analyze_lambda(params: &Params, body: &Expr) -> Scope {
    let mut s = Scope::default();
    for a in params.all_positional().chain(params.kwonly.iter()) {
        s.locals.insert(a.name.clone());
    }
    let mut b = Binder { s: &mut s, in_try: 0 };
    b.expr(body);
    let mut u = Uses { s: &mut s, comp_bound: Vec::new() };
    u.expr(body);
    s
}

/// Scope of a generator expression (treated as an anonymous generator function).
pub fn analyze_genexp(elt: &Expr, gens: &[Comprehension]) -> Scope {
    let mut s = Scope::default();
    let mut u = Uses { s: &mut s, comp_bound: Vec::new() };
    u.comprehension(gens, &mut |u| u.expr(elt));
    s.has_yield = true;
    s
}

struct Binder<'a> {
    s: &'a mut Scope,
    in_try: usize,
}

impl Binder<'_> {
    fn bind(&mut self, name: &str) {
        self.s.locals.insert(name.to_string());
        if self.in_try > 0 {
            self.s.try_assigned.insert(name.to_string());
        }
    }

    fn target(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Name(n) => self.bind(n),
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                for x in elts {
                    self.target(x);
                }
            }
            ExprKind::Starred(x) => self.target(x),
            _ => self.expr(e),
        }
    }

    fn block(&mut self, b: &[Stmt]) {
        for s in b {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match &st.kind {
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Assign { targets, value } => {
                for t in targets {
                    self.target(t);
                }
                self.expr(value);
            }
            StmtKind::AnnAssign { target, value, .. } => {
                self.target(target);
                if let Some(v) = value {
                    self.expr(v);
                }
            }
            StmtKind::AugAssign { target, value, .. } => {
                self.target(target);
                self.expr(value);
            }
            StmtKind::Return(Some(e)) => self.expr(e),
            StmtKind::Raise { exc, cause } => {
                if let Some(e) = exc {
                    self.expr(e);
                }
                if let Some(e) = cause {
                    self.expr(e);
                }
            }
            StmtKind::Global(ns) => {
                for n in ns {
                    self.s.globals.insert(n.clone());
                }
            }
            StmtKind::Nonlocal(ns) => {
                for n in ns {
                    self.s.nonlocals.insert(n.clone());
                }
            }
            StmtKind::Del(ts) => {
                for t in ts {
                    self.expr(t);
                }
            }
            StmtKind::Assert { test, msg } => {
                self.expr(test);
                if let Some(m) = msg {
                    self.expr(m);
                }
            }
            StmtKind::Import(names) => {
                for a in names {
                    let n = a.asname.clone().unwrap_or_else(|| a.name.split('.').next().unwrap().to_string());
                    self.bind(&n);
                }
            }
            StmtKind::ImportFrom { names, .. } => {
                for a in names {
                    if a.name != "*" {
                        self.bind(a.asname.as_ref().unwrap_or(&a.name));
                    }
                }
            }
            StmtKind::If { test, body, orelse } | StmtKind::While { test, body, orelse } => {
                self.expr(test);
                self.block(body);
                self.block(orelse);
            }
            StmtKind::For { target, iter, body, orelse, .. } => {
                self.expr(iter);
                self.target(target);
                self.block(body);
                self.block(orelse);
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                self.s.has_try = true;
                self.in_try += 1;
                self.block(body);
                self.in_try -= 1;
                for h in handlers {
                    if let Some(n) = &h.name {
                        self.bind(n);
                    }
                    self.in_try += 1;
                    self.block(&h.body);
                    self.in_try -= 1;
                }
                self.block(orelse);
                self.block(finalbody);
            }
            StmtKind::With { items, body, .. } => {
                // `with` desugars to try/finally.
                self.s.has_try = true;
                for it in items {
                    self.expr(&it.ctx);
                    if let Some(v) = &it.var {
                        self.target(v);
                    }
                }
                self.in_try += 1;
                self.block(body);
                self.in_try -= 1;
            }
            StmtKind::FuncDef(f) => {
                self.bind(&f.name);
                for d in &f.decorators {
                    self.expr(d);
                }
            }
            StmtKind::ClassDef(c) => self.bind(&c.name),
            StmtKind::Match { subject, cases } => {
                self.expr(subject);
                for c in cases {
                    self.pattern(&c.pattern);
                    if let Some(g) = &c.guard {
                        self.expr(g);
                    }
                    self.block(&c.body);
                }
            }
            StmtKind::TypeAlias { .. } | StmtKind::Pass | StmtKind::Break | StmtKind::Continue | StmtKind::Return(None) => {}
        }
    }

    fn pattern(&mut self, p: &Pattern) {
        match p {
            Pattern::Capture(n) => self.bind(n),
            Pattern::As(p, n) => {
                self.pattern(p);
                self.bind(n);
            }
            Pattern::Or(ps) | Pattern::Sequence(ps, _) => {
                for p in ps {
                    self.pattern(p);
                }
            }
            Pattern::Star(Some(n)) => self.bind(n),
            Pattern::Class { args, kwargs, .. } => {
                for p in args {
                    self.pattern(p);
                }
                for (_, p) in kwargs {
                    self.pattern(p);
                }
            }
            Pattern::Mapping { values, rest, .. } => {
                for p in values {
                    self.pattern(p);
                }
                if let Some(r) = rest {
                    self.bind(r);
                }
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        // Only walrus targets and yields matter for binding.
        match &e.kind {
            ExprKind::NamedExpr { target, value } => {
                self.bind(target);
                self.expr(value);
            }
            ExprKind::Yield(v) => {
                self.s.has_yield = true;
                if let Some(v) = v {
                    self.expr(v);
                }
            }
            ExprKind::YieldFrom(v) => {
                self.s.has_yield = true;
                self.expr(v);
            }
            ExprKind::Lambda { .. } | ExprKind::GenExp { .. } => {}
            _ => walk_children(e, &mut |c| self.expr(c)),
        }
    }
}

struct Uses<'a> {
    s: &'a mut Scope,
    comp_bound: Vec<BTreeSet<String>>,
}

impl Uses<'_> {
    fn use_name(&mut self, n: &str) {
        if self.comp_bound.iter().any(|b| b.contains(n)) {
            return;
        }
        if !self.s.locals.contains(n) {
            self.s.frees.insert(n.to_string());
        }
    }

    fn nested(&mut self, inner: Scope) {
        for f in inner.frees {
            if self.comp_bound.iter().any(|b| b.contains(&f)) {
                continue;
            }
            if self.s.locals.contains(&f) {
                self.s.cells.insert(f);
            } else {
                self.s.frees.insert(f);
            }
        }
        for f in inner.nonlocals {
            if self.s.locals.contains(&f) {
                self.s.cells.insert(f);
            } else {
                self.s.frees.insert(f);
            }
        }
    }

    fn block(&mut self, b: &[Stmt]) {
        for s in b {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, st: &Stmt) {
        match &st.kind {
            StmtKind::FuncDef(f) => {
                for d in &f.decorators {
                    self.expr(d);
                }
                for a in f.params.all_positional().chain(f.params.kwonly.iter()) {
                    if let Some(d) = &a.default {
                        self.expr(d);
                    }
                }
                let inner = analyze_function(&f.params, &f.body);
                self.nested(inner);
            }
            StmtKind::ClassDef(c) => {
                for b in &c.bases {
                    self.expr(b);
                }
                for s in &c.body {
                    if let StmtKind::FuncDef(f) = &s.kind {
                        let inner = analyze_function(&f.params, &f.body);
                        self.nested(inner);
                    }
                }
            }
            StmtKind::Nonlocal(ns) => {
                for n in ns {
                    self.s.frees.insert(n.clone());
                }
            }
            _ => {
                let mut nodes: Vec<Node> = Vec::new();
                walk_stmt(st, &mut |n| nodes.push(n));
                for n in nodes {
                    match n {
                        Node::E(e) => self.expr(e),
                        Node::B(b) => self.block(b),
                    }
                }
            }
        }
    }

    fn comprehension(&mut self, gens: &[Comprehension], inner: &mut dyn FnMut(&mut Self)) {
        let mut bound = BTreeSet::new();
        for (i, g) in gens.iter().enumerate() {
            if i == 0 {
                self.expr(&g.iter);
            }
            collect_target_names(&g.target, &mut bound);
        }
        self.comp_bound.push(bound);
        for (i, g) in gens.iter().enumerate() {
            if i > 0 {
                self.expr(&g.iter);
            }
            for c in &g.ifs {
                self.expr(c);
            }
        }
        inner(self);
        self.comp_bound.pop();
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Name(n) => self.use_name(n),
            ExprKind::Lambda { params, body } => {
                for a in params.all_positional() {
                    if let Some(d) = &a.default {
                        self.expr(d);
                    }
                }
                let inner = analyze_lambda(params, body);
                self.nested(inner);
            }
            ExprKind::GenExp { elt, gens } => {
                // Lazy: behaves like a nested generator function. The first iterable is evaluated eagerly.
                self.expr(&gens[0].iter);
                let inner = analyze_genexp(elt, gens);
                self.nested(inner);
            }
            ExprKind::ListComp { elt, gens } | ExprKind::SetComp { elt, gens } => {
                let elt = elt.clone();
                self.comprehension(gens, &mut |u| u.expr(&elt));
            }
            ExprKind::DictComp { key, value, gens } => {
                let (k, v) = (key.clone(), value.clone());
                self.comprehension(gens, &mut |u| {
                    u.expr(&k);
                    u.expr(&v);
                });
            }
            ExprKind::NamedExpr { target, value } => {
                self.use_name(target);
                self.expr(value);
            }
            _ => walk_children(e, &mut |c| self.expr(c)),
        }
    }
}

pub enum Node<'a> {
    E(&'a Expr),
    B(&'a [Stmt]),
}

pub fn collect_target_names(e: &Expr, out: &mut BTreeSet<String>) {
    match &e.kind {
        ExprKind::Name(n) => {
            out.insert(n.clone());
        }
        ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
            for x in elts {
                collect_target_names(x, out);
            }
        }
        ExprKind::Starred(x) => collect_target_names(x, out),
        _ => {}
    }
}

/// Visit the direct sub-expressions and sub-blocks of a statement.
pub fn walk_stmt<'a>(st: &'a Stmt, f: &mut dyn FnMut(Node<'a>)) {
    match &st.kind {
        StmtKind::Expr(e) => f(Node::E(e)),
        StmtKind::Assign { targets, value } => {
            for t in targets {
                f(Node::E(t));
            }
            f(Node::E(value));
        }
        StmtKind::AnnAssign { target, value, .. } => {
            f(Node::E(target));
            if let Some(v) = value {
                f(Node::E(v));
            }
        }
        StmtKind::AugAssign { target, value, .. } => {
            f(Node::E(target));
            f(Node::E(value));
        }
        StmtKind::Return(Some(e)) => f(Node::E(e)),
        StmtKind::Raise { exc, cause } => {
            if let Some(e) = exc {
                f(Node::E(e));
            }
            if let Some(e) = cause {
                f(Node::E(e));
            }
        }
        StmtKind::Del(ts) => {
            for t in ts {
                f(Node::E(t));
            }
        }
        StmtKind::Assert { test, msg } => {
            f(Node::E(test));
            if let Some(m) = msg {
                f(Node::E(m));
            }
        }
        StmtKind::If { test, body, orelse } | StmtKind::While { test, body, orelse } => {
            f(Node::E(test));
            f(Node::B(body));
            f(Node::B(orelse));
        }
        StmtKind::For { target, iter, body, orelse, .. } => {
            f(Node::E(iter));
            f(Node::E(target));
            f(Node::B(body));
            f(Node::B(orelse));
        }
        StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
            f(Node::B(body));
            for h in handlers {
                if let Some(t) = &h.typ {
                    f(Node::E(t));
                }
                f(Node::B(&h.body));
            }
            f(Node::B(orelse));
            f(Node::B(finalbody));
        }
        StmtKind::With { items, body, .. } => {
            for it in items {
                f(Node::E(&it.ctx));
                if let Some(v) = &it.var {
                    f(Node::E(v));
                }
            }
            f(Node::B(body));
        }
        StmtKind::Match { subject, cases } => {
            f(Node::E(subject));
            for c in cases {
                walk_pattern_exprs(&c.pattern, &mut |e| f(Node::E(e)));
                if let Some(g) = &c.guard {
                    f(Node::E(g));
                }
                f(Node::B(&c.body));
            }
        }
        _ => {}
    }
}

fn walk_pattern_exprs<'a>(p: &'a Pattern, fe: &mut dyn FnMut(&'a Expr)) {
    match p {
        Pattern::Value(e) => fe(e),
        Pattern::Or(ps) | Pattern::Sequence(ps, _) => {
            for p in ps {
                walk_pattern_exprs(p, fe);
            }
        }
        Pattern::As(p, _) => walk_pattern_exprs(p, fe),
        Pattern::Class { cls, args, kwargs } => {
            fe(cls);
            for p in args {
                walk_pattern_exprs(p, fe);
            }
            for (_, p) in kwargs {
                walk_pattern_exprs(p, fe);
            }
        }
        Pattern::Mapping { keys, values, .. } => {
            for k in keys {
                fe(k);
            }
            for p in values {
                walk_pattern_exprs(p, fe);
            }
        }
        _ => {}
    }
}

/// Visit direct sub-expressions of an expression.
pub fn walk_children<'a>(e: &'a Expr, f: &mut dyn FnMut(&'a Expr)) {
    match &e.kind {
        ExprKind::BinOp { left, right, .. } => {
            f(left);
            f(right);
        }
        ExprKind::UnaryOp { operand, .. } => f(operand),
        ExprKind::BoolOp { values, .. } => values.iter().for_each(|v| f(v)),
        ExprKind::Compare { left, comparators, .. } => {
            f(left);
            comparators.iter().for_each(|v| f(v));
        }
        ExprKind::Call { func, args, keywords } => {
            f(func);
            args.iter().for_each(|v| f(v));
            keywords.iter().for_each(|k| f(&k.value));
        }
        ExprKind::Attribute { value, .. } => f(value),
        ExprKind::Subscript { value, index } => {
            f(value);
            f(index);
        }
        ExprKind::Slice { lower, upper, step } => {
            for x in [lower, upper, step].into_iter().flatten() {
                f(x);
            }
        }
        ExprKind::Starred(x) | ExprKind::Await(x) | ExprKind::YieldFrom(x) => f(x),
        ExprKind::Yield(Some(x)) => f(x),
        ExprKind::List(xs) | ExprKind::Set(xs) | ExprKind::Tuple { elts: xs, .. } => xs.iter().for_each(|v| f(v)),
        ExprKind::Dict(items) => {
            for (k, v) in items {
                if let Some(k) = k {
                    f(k);
                }
                f(v);
            }
        }
        ExprKind::ListComp { elt, gens } | ExprKind::SetComp { elt, gens } | ExprKind::GenExp { elt, gens } => {
            f(elt);
            for g in gens {
                f(&g.target);
                f(&g.iter);
                g.ifs.iter().for_each(|v| f(v));
            }
        }
        ExprKind::DictComp { key, value, gens } => {
            f(key);
            f(value);
            for g in gens {
                f(&g.target);
                f(&g.iter);
                g.ifs.iter().for_each(|v| f(v));
            }
        }
        ExprKind::IfExp { test, body, orelse } => {
            f(test);
            f(body);
            f(orelse);
        }
        ExprKind::Lambda { body, .. } => f(body),
        ExprKind::NamedExpr { value, .. } => f(value),
        ExprKind::FStr { parts, .. } => {
            fn walk_parts<'b>(parts: &'b [FStrPart], f: &mut dyn FnMut(&'b Expr)) {
                for p in parts {
                    if let FStrPart::Expr { expr, spec, .. } = p {
                        f(expr);
                        walk_parts(spec, f);
                    }
                }
            }
            walk_parts(parts, f);
        }
        _ => {}
    }
}
