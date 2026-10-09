//! `fpython convert`: turn Python 3 source into fpython (like 2to3, but for types).
//!
//! The converter keeps the author's source text and inserts annotations in place:
//!   * parameter / return types inferred statically (defaults, call sites, return
//!     expressions, usage) and optionally from a runtime trace (`--trace`);
//!   * annotations for empty containers (`x: list[int] = []`) and instance attributes;
//!   * `%`-formatting and simple `.format()` calls rewritten as f-strings.
//! The result is formatted with `fpython fmt` and type-checked; anything that still
//! needs a human is reported.

use crate::ast::*;
use crate::compiler::scope;
use crate::parser;
use std::collections::{BTreeMap, HashMap};

// ---------------------------------------------------------------------- inferred types

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CT {
    Unknown,
    Any,
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    NoneT,
    List(Box<CT>),
    Dict(Box<CT>, Box<CT>),
    Set(Box<CT>),
    Tuple(Vec<CT>),
    Opt(Box<CT>),
    Iter(Box<CT>),
    Class(String),
}

impl CT {
    pub fn render(&self) -> String {
        match self {
            CT::Unknown | CT::Any => "Any".into(),
            CT::Int => "int".into(),
            CT::Float => "float".into(),
            CT::Bool => "bool".into(),
            CT::Str => "str".into(),
            CT::Bytes => "bytes".into(),
            CT::NoneT => "None".into(),
            CT::List(e) => format!("list[{}]", e.render()),
            CT::Set(e) => format!("set[{}]", e.render()),
            CT::Dict(k, v) => format!("dict[{}, {}]", k.render(), v.render()),
            CT::Tuple(ts) => {
                if ts.is_empty() {
                    "tuple[()]".into()
                } else {
                    format!("tuple[{}]", ts.iter().map(|t| t.render()).collect::<Vec<_>>().join(", "))
                }
            }
            CT::Opt(t) => format!("{} | None", t.render()),
            CT::Iter(t) => format!("Iterator[{}]", t.render()),
            CT::Class(n) => n.clone(),
        }
    }

    fn known(&self) -> bool {
        !matches!(self, CT::Unknown)
    }

    pub fn uses_any(&self) -> bool {
        match self {
            CT::Unknown | CT::Any => true,
            CT::List(e) | CT::Set(e) | CT::Opt(e) | CT::Iter(e) => e.uses_any(),
            CT::Dict(k, v) => k.uses_any() || v.uses_any(),
            CT::Tuple(ts) => ts.iter().any(|t| t.uses_any()),
            _ => false,
        }
    }

    fn elem(&self) -> CT {
        match self {
            CT::List(e) | CT::Set(e) | CT::Iter(e) => (**e).clone(),
            CT::Dict(k, _) => (**k).clone(),
            CT::Str => CT::Str,
            CT::Bytes => CT::Int,
            CT::Tuple(ts) if !ts.is_empty() && ts.iter().all(|t| *t == ts[0]) => ts[0].clone(),
            CT::Any => CT::Any,
            _ => CT::Unknown,
        }
    }
}

pub fn unify(a: &CT, b: &CT) -> CT {
    if a == b {
        return a.clone();
    }
    match (a, b) {
        (CT::Unknown, x) | (x, CT::Unknown) => x.clone(),
        (CT::Int, CT::Float) | (CT::Float, CT::Int) => CT::Float,
        (CT::Bool, CT::Int) | (CT::Int, CT::Bool) => CT::Int,
        (CT::NoneT, CT::Opt(t)) | (CT::Opt(t), CT::NoneT) => CT::Opt(t.clone()),
        (CT::NoneT, x) | (x, CT::NoneT) => CT::Opt(Box::new(x.clone())),
        (CT::Opt(x), y) | (y, CT::Opt(x)) => CT::Opt(Box::new(unify(x, y))),
        (CT::List(x), CT::List(y)) => CT::List(Box::new(unify(x, y))),
        (CT::Set(x), CT::Set(y)) => CT::Set(Box::new(unify(x, y))),
        (CT::Iter(x), CT::Iter(y)) => CT::Iter(Box::new(unify(x, y))),
        (CT::Dict(k1, v1), CT::Dict(k2, v2)) => CT::Dict(Box::new(unify(k1, k2)), Box::new(unify(v1, v2))),
        (CT::Tuple(x), CT::Tuple(y)) if x.len() == y.len() => CT::Tuple(x.iter().zip(y).map(|(a, b)| unify(a, b)).collect()),
        _ => CT::Any,
    }
}

// ---------------------------------------------------------------------- tracing (runtime types)

/// Python helper that records argument/return types of functions defined in the target file.
pub const TRACER: &str = r#"
import sys, json, os, types, runpy
target = os.path.abspath(sys.argv[1])
out = sys.argv[2]
sys.argv = [target] + sys.argv[3:]
seen = {}

def tname(v, depth=0):
    if v is None: return "None"
    t = type(v)
    if t is bool: return "bool"
    if t is int: return "int"
    if t is float: return "float"
    if t is str: return "str"
    if t is bytes: return "bytes"
    if depth > 2: return "Any"
    if t is list:
        es = sorted({tname(x, depth+1) for x in v[:20]})
        return "list[%s]" % (es[0] if len(es) == 1 else "Any") if es else "list[?]"
    if t is set:
        es = sorted({tname(x, depth+1) for x in list(v)[:20]})
        return "set[%s]" % (es[0] if len(es) == 1 else "Any") if es else "set[?]"
    if t is dict:
        ks = sorted({tname(x, depth+1) for x in list(v.keys())[:20]})
        vs = sorted({tname(x, depth+1) for x in list(v.values())[:20]})
        if not ks: return "dict[?, ?]"
        return "dict[%s, %s]" % (ks[0] if len(ks) == 1 else "Any", vs[0] if len(vs) == 1 else "Any")
    if t is tuple:
        if len(v) > 8: return "Any"
        return "tuple[%s]" % ", ".join(tname(x, depth+1) for x in v) if v else "tuple[()]"
    if isinstance(v, types.GeneratorType): return "Iterator[Any]"
    mod = getattr(t, "__module__", "")
    if mod == "__main__": return t.__qualname__
    return "Any"

def prof(frame, event, arg):
    code = frame.f_code
    if code.co_filename != target: return
    key = "%d" % code.co_firstlineno
    if event == "call":
        rec = seen.setdefault(key, {"args": {}, "ret": []})
        n = code.co_argcount + code.co_kwonlyargcount
        for name in code.co_varnames[:n]:
            if name in frame.f_locals:
                s = rec["args"].setdefault(name, [])
                tn = tname(frame.f_locals[name])
                if tn not in s and len(s) < 6: s.append(tn)
    elif event == "return":
        rec = seen.setdefault(key, {"args": {}, "ret": []})
        tn = tname(arg)
        if tn not in rec["ret"] and len(rec["ret"]) < 6: rec["ret"].append(tn)

sys.setprofile(prof)
try:
    runpy.run_path(target, run_name="__main__")
except SystemExit:
    pass
finally:
    sys.setprofile(None)
    with open(out, "w") as f:
        json.dump(seen, f)
"#;

fn parse_trace_type(s: &str) -> CT {
    let s = s.trim();
    match s {
        "int" => return CT::Int,
        "float" => return CT::Float,
        "bool" => return CT::Bool,
        "str" => return CT::Str,
        "bytes" => return CT::Bytes,
        "None" => return CT::NoneT,
        "Any" => return CT::Any,
        "?" => return CT::Unknown,
        _ => {}
    }
    let inner = |p: &str| -> Vec<CT> {
        let body = &s[p.len()..s.len() - 1];
        split_top(body).iter().map(|x| parse_trace_type(x)).collect()
    };
    if s.starts_with("list[") {
        return CT::List(Box::new(inner("list[").pop().unwrap_or(CT::Unknown)));
    }
    if s.starts_with("set[") {
        return CT::Set(Box::new(inner("set[").pop().unwrap_or(CT::Unknown)));
    }
    if s.starts_with("dict[") {
        let v = inner("dict[");
        return CT::Dict(Box::new(v.first().cloned().unwrap_or(CT::Unknown)), Box::new(v.get(1).cloned().unwrap_or(CT::Unknown)));
    }
    if s.starts_with("tuple[") {
        if s == "tuple[()]" {
            return CT::Tuple(vec![]);
        }
        return CT::Tuple(inner("tuple["));
    }
    if s.starts_with("Iterator[") {
        return CT::Iter(Box::new(inner("Iterator[").pop().unwrap_or(CT::Any)));
    }
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') {
        return CT::Class(s.rsplit('.').next().unwrap().to_string());
    }
    CT::Any
}

fn split_top(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in s.chars() {
        match c {
            '[' => {
                depth += 1;
                cur.push(c)
            }
            ']' => {
                depth -= 1;
                cur.push(c)
            }
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// Run the tracer: returns def-line -> (param types, return type).
pub fn run_trace(py_file: &std::path::Path, args: &[String]) -> Result<HashMap<usize, (HashMap<String, CT>, CT)>, String> {
    let dir = std::env::temp_dir().join(format!("fpython-trace-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let script = dir.join("fpy_tracer.py");
    let out = dir.join("trace.json");
    std::fs::write(&script, TRACER).map_err(|e| e.to_string())?;
    let py = std::env::var("FPY_PYTHON").unwrap_or_else(|_| "python3".into());
    let status = std::process::Command::new(&py).arg(&script).arg(py_file).arg(&out).args(args).status().map_err(|e| format!("cannot run {}: {}", py, e))?;
    let _ = status;
    let data = std::fs::read_to_string(&out).map_err(|e| format!("trace produced no output: {}", e))?;
    let _ = std::fs::remove_dir_all(&dir);
    parse_trace_json(&data)
}

/// Minimal parser for the tracer's JSON: {"line": {"args": {name: [types]}, "ret": [types]}}.
pub fn parse_trace_json(data: &str) -> Result<HashMap<usize, (HashMap<String, CT>, CT)>, String> {
    let v = mini_json(data).ok_or("invalid trace json")?;
    let mut out = HashMap::new();
    if let J::Obj(entries) = v {
        for (k, rec) in entries {
            let line: usize = k.parse().map_err(|_| "bad line key")?;
            let mut params = HashMap::new();
            let mut ret = CT::Unknown;
            if let J::Obj(fields) = rec {
                for (fk, fv) in fields {
                    match (fk.as_str(), fv) {
                        ("args", J::Obj(args)) => {
                            for (name, ts) in args {
                                if let J::Arr(ts) = ts {
                                    let mut t = CT::Unknown;
                                    for x in ts {
                                        if let J::Str(s) = x {
                                            t = unify(&t, &parse_trace_type(&s));
                                        }
                                    }
                                    params.insert(name, t);
                                }
                            }
                        }
                        ("ret", J::Arr(ts)) => {
                            for x in ts {
                                if let J::Str(s) = x {
                                    ret = unify(&ret, &parse_trace_type(&s));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            out.insert(line, (params, ret));
        }
    }
    Ok(out)
}

enum J {
    Obj(Vec<(String, J)>),
    Arr(Vec<J>),
    Str(String),
    Other,
}

fn mini_json(s: &str) -> Option<J> {
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    fn ws(b: &[char], i: &mut usize) {
        while *i < b.len() && b[*i].is_whitespace() {
            *i += 1;
        }
    }
    fn val(b: &[char], i: &mut usize) -> Option<J> {
        ws(b, i);
        match b.get(*i)? {
            '{' => {
                *i += 1;
                let mut v = Vec::new();
                loop {
                    ws(b, i);
                    if b.get(*i) == Some(&'}') {
                        *i += 1;
                        return Some(J::Obj(v));
                    }
                    let k = match val(b, i)? {
                        J::Str(s) => s,
                        _ => return None,
                    };
                    ws(b, i);
                    if b.get(*i) != Some(&':') {
                        return None;
                    }
                    *i += 1;
                    let x = val(b, i)?;
                    v.push((k, x));
                    ws(b, i);
                    if b.get(*i) == Some(&',') {
                        *i += 1;
                    }
                }
            }
            '[' => {
                *i += 1;
                let mut v = Vec::new();
                loop {
                    ws(b, i);
                    if b.get(*i) == Some(&']') {
                        *i += 1;
                        return Some(J::Arr(v));
                    }
                    v.push(val(b, i)?);
                    ws(b, i);
                    if b.get(*i) == Some(&',') {
                        *i += 1;
                    }
                }
            }
            '"' => {
                *i += 1;
                let mut s = String::new();
                while *i < b.len() && b[*i] != '"' {
                    if b[*i] == '\\' {
                        *i += 1;
                    }
                    s.push(b[*i]);
                    *i += 1;
                }
                *i += 1;
                Some(J::Str(s))
            }
            _ => {
                while *i < b.len() && !matches!(b[*i], ',' | '}' | ']') {
                    *i += 1;
                }
                Some(J::Other)
            }
        }
    }
    val(&b, &mut i)
}

// ---------------------------------------------------------------------- static inference

#[derive(Debug, Clone)]
struct FnRec {
    name: String,
    class: Option<String>,
    params: Vec<(String, CT)>,
    ret: CT,
    is_method: bool,
    is_static: bool,
    def_line: usize,
    has_yield: bool,
}

struct Infer<'a> {
    module: &'a Module,
    funcs: Vec<FnRec>,
    /// function name -> index (module-level), "Class.method" -> index
    index: HashMap<String, usize>,
    classes: HashMap<String, BTreeMap<String, CT>>,
    globals: HashMap<String, CT>,
}

fn module_attr_type(m: &str, a: &str) -> CT {
    match (m, a) {
        ("math", "pi" | "e" | "tau" | "inf" | "nan") => CT::Float,
        ("math", "floor" | "ceil" | "trunc" | "gcd" | "lcm" | "factorial" | "comb" | "perm" | "isqrt") => CT::Int,
        ("math", "isnan" | "isinf" | "isfinite" | "isclose") => CT::Bool,
        ("math", _) => CT::Float,
        ("time", "time" | "perf_counter" | "monotonic") => CT::Float,
        ("time", "time_ns" | "perf_counter_ns" | "monotonic_ns") => CT::Int,
        ("random", "random" | "uniform" | "gauss") => CT::Float,
        ("random", "randint" | "randrange") => CT::Int,
        ("os", "getcwd" | "sep" | "linesep" | "name") => CT::Str,
        ("os", "getenv") => CT::Opt(Box::new(CT::Str)),
        ("os", "listdir") => CT::List(Box::new(CT::Str)),
        ("sys", "argv") => CT::List(Box::new(CT::Str)),
        ("sys", "platform" | "version") => CT::Str,
        ("sys", "maxsize") => CT::Int,
        ("json", "dumps") => CT::Str,
        ("json", "loads") => CT::Any,
        ("string", _) => CT::Str,
        _ => CT::Unknown,
    }
}

impl<'a> Infer<'a> {
    fn new(module: &'a Module) -> Infer<'a> {
        let mut inf = Infer { module, funcs: Vec::new(), index: HashMap::new(), classes: HashMap::new(), globals: HashMap::new() };
        for st in &module.body {
            match &st.kind {
                StmtKind::FuncDef(f) => inf.add_fn(f, None),
                StmtKind::ClassDef(c) => {
                    inf.classes.insert(c.name.clone(), BTreeMap::new());
                    for s in &c.body {
                        if let StmtKind::FuncDef(f) = &s.kind {
                            inf.add_fn(f, Some(c.name.clone()));
                        }
                        if let StmtKind::AnnAssign { target, ann, .. } = &s.kind {
                            if let ExprKind::Name(n) = &target.kind {
                                let t = ann_to_ct(ann);
                                inf.classes.get_mut(&c.name).unwrap().insert(n.clone(), t);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        inf
    }

    fn add_fn(&mut self, f: &FuncDef, class: Option<String>) {
        let is_static = f.decorators.iter().any(|d| matches!(&d.kind, ExprKind::Name(n) if n == "staticmethod"));
        let is_method = class.is_some() && !is_static;
        let mut params = Vec::new();
        for (i, a) in f.params.all_positional().chain(f.params.kwonly.iter()).enumerate() {
            let t = if is_method && i == 0 {
                CT::Class(class.clone().unwrap())
            } else if let Some(ann) = &a.ann {
                ann_to_ct(ann)
            } else {
                match &a.default {
                    Some(d) => literal_type(d),
                    None => CT::Unknown,
                }
            };
            params.push((a.name.clone(), t));
        }
        let ret = match &f.returns {
            Some(r) => ann_to_ct(r),
            None => CT::Unknown,
        };
        let has_yield = scope::analyze_function(&f.params, &f.body).has_yield;
        let key = match &class {
            Some(c) => format!("{}.{}", c, f.name),
            None => f.name.clone(),
        };
        self.index.insert(key, self.funcs.len());
        self.funcs.push(FnRec { name: f.name.clone(), class, params, ret, is_method, is_static, def_line: f.header_lines.0, has_yield });
    }

    fn run(&mut self) {
        for _ in 0..4 {
            // module globals
            let mut env: HashMap<String, CT> = HashMap::new();
            let body = &self.module.body;
            self.walk_block(body, &mut env, None, &mut Vec::new(), &mut Vec::new());
            for (k, v) in env {
                self.globals.insert(k, v);
            }
            for st in body {
                match &st.kind {
                    StmtKind::FuncDef(f) => self.infer_fn(f, None),
                    StmtKind::ClassDef(c) => {
                        // __init__ first so attribute types are available to other methods
                        let mut methods: Vec<&FuncDef> = c.body.iter().filter_map(|s| if let StmtKind::FuncDef(f) = &s.kind { Some(&**f) } else { None }).collect();
                        methods.sort_by_key(|f| if f.name == "__init__" { 0 } else { 1 });
                        for f in methods {
                            self.infer_fn(f, Some(&c.name));
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn infer_fn(&mut self, f: &FuncDef, class: Option<&String>) {
        let key = match class {
            Some(c) => format!("{}.{}", c, f.name),
            None => f.name.clone(),
        };
        let Some(&idx) = self.index.get(&key) else { return };
        let mut env: HashMap<String, CT> = self.funcs[idx].params.iter().cloned().collect();
        let mut rets = Vec::new();
        let mut yields = Vec::new();
        self.walk_block(&f.body, &mut env, class, &mut rets, &mut yields);
        if f.returns.is_none() {
            let r = if self.funcs[idx].has_yield {
                let mut t = CT::Unknown;
                for y in &yields {
                    t = unify(&t, y);
                }
                CT::Iter(Box::new(t))
            } else if rets.is_empty() {
                CT::NoneT
            } else {
                let mut t = CT::Unknown;
                for r in &rets {
                    t = unify(&t, r);
                }
                if !crate::compiler::always_terminates(&f.body) && !rets.iter().all(|r| *r == CT::NoneT) {
                    t = unify(&t, &CT::NoneT);
                }
                t
            };
            if r.known() {
                self.funcs[idx].ret = r;
            }
        }
        // parameter types learned from usage inside the body (e.g. param.append(int))
        for (i, a) in f.params.all_positional().chain(f.params.kwonly.iter()).enumerate() {
            if a.ann.is_none() {
                if let Some(t) = env.get(&a.name) {
                    if t.known() && !self.funcs[idx].params[i].1.known() {
                        self.funcs[idx].params[i].1 = t.clone();
                    }
                }
            }
        }
    }

    fn walk_block(&mut self, body: &[Stmt], env: &mut HashMap<String, CT>, class: Option<&String>, rets: &mut Vec<CT>, yields: &mut Vec<CT>) {
        for st in body {
            self.walk_stmt(st, env, class, rets, yields);
        }
    }

    fn walk_stmt(&mut self, st: &Stmt, env: &mut HashMap<String, CT>, class: Option<&String>, rets: &mut Vec<CT>, yields: &mut Vec<CT>) {
        match &st.kind {
            StmtKind::Assign { targets, value } => {
                let t = self.eval(value, env, class);
                for tg in targets {
                    self.bind(tg, &t, env, class);
                }
                self.calls_in(value, env, class, yields);
            }
            StmtKind::AnnAssign { target, ann, value } => {
                let t = ann_to_ct(ann);
                self.bind(target, &t, env, class);
                if let Some(v) = value {
                    self.calls_in(v, env, class, yields);
                }
            }
            StmtKind::AugAssign { target, value, .. } => {
                let t = self.eval(value, env, class);
                if let ExprKind::Name(n) = &target.kind {
                    let cur = env.get(n).cloned().unwrap_or(CT::Unknown);
                    env.insert(n.clone(), unify(&cur, &t));
                }
                self.calls_in(value, env, class, yields);
            }
            StmtKind::Return(v) => match v {
                Some(e) => {
                    rets.push(self.eval(e, env, class));
                    self.calls_in(e, env, class, yields);
                }
                None => rets.push(CT::NoneT),
            },
            StmtKind::Expr(e) => {
                self.calls_in(e, env, class, yields);
                self.usage(e, env, class);
            }
            StmtKind::For { target, iter, body, orelse, .. } => {
                let it = self.eval(iter, env, class);
                let et = iter_elem(iter, &it, self, env, class);
                self.bind(target, &et, env, class);
                self.calls_in(iter, env, class, yields);
                self.walk_block(body, env, class, rets, yields);
                self.walk_block(orelse, env, class, rets, yields);
            }
            StmtKind::If { test, body, orelse } | StmtKind::While { test, body, orelse } => {
                self.calls_in(test, env, class, yields);
                self.walk_block(body, env, class, rets, yields);
                self.walk_block(orelse, env, class, rets, yields);
            }
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                self.walk_block(body, env, class, rets, yields);
                for h in handlers {
                    if let Some(n) = &h.name {
                        let t = match &h.typ {
                            Some(Expr { kind: ExprKind::Name(c), .. }) => CT::Class(c.clone()),
                            _ => CT::Class("Exception".into()),
                        };
                        env.insert(n.clone(), t);
                    }
                    self.walk_block(&h.body, env, class, rets, yields);
                }
                self.walk_block(orelse, env, class, rets, yields);
                self.walk_block(finalbody, env, class, rets, yields);
            }
            StmtKind::With { items, body, .. } => {
                for it in items {
                    if let Some(v) = &it.var {
                        let t = match &it.ctx.kind {
                            ExprKind::Call { func, .. } if matches!(&func.kind, ExprKind::Name(n) if n == "open") => CT::Class("TextFile".into()),
                            _ => self.eval(&it.ctx, env, class),
                        };
                        self.bind(v, &t, env, class);
                    }
                }
                self.walk_block(body, env, class, rets, yields);
            }
            StmtKind::Match { cases, .. } => {
                for c in cases {
                    self.walk_block(&c.body, env, class, rets, yields);
                }
            }
            _ => {}
        }
    }

    /// Learn types from method calls on locals: xs.append(1) -> list[int]; d[k] = v handled in bind.
    fn usage(&mut self, e: &Expr, env: &mut HashMap<String, CT>, class: Option<&String>) {
        if let ExprKind::Call { func, args, .. } = &e.kind {
            if let ExprKind::Attribute { value, attr } = &func.kind {
                if let ExprKind::Name(n) = &value.kind {
                    let cur = env.get(n).cloned().unwrap_or(CT::Unknown);
                    let a0 = args.first().map(|a| self.eval(a, env, class)).unwrap_or(CT::Unknown);
                    let new = match (attr.as_str(), &cur) {
                        ("append" | "insert" | "remove", CT::List(e)) | ("add" | "discard", CT::Set(e)) if attr != "insert" => {
                            Some(if attr == "add" || attr == "discard" { CT::Set(Box::new(unify(e, &a0))) } else { CT::List(Box::new(unify(e, &a0))) })
                        }
                        ("insert", CT::List(e)) => {
                            let a1 = args.get(1).map(|a| self.eval(a, env, class)).unwrap_or(CT::Unknown);
                            Some(CT::List(Box::new(unify(e, &a1))))
                        }
                        ("append", CT::Unknown) => Some(CT::List(Box::new(a0))),
                        ("extend", CT::List(e)) => Some(CT::List(Box::new(unify(e, &a0.elem())))),
                        _ => None,
                    };
                    if let Some(t) = new {
                        env.insert(n.clone(), t);
                    }
                }
                // self.items.append(x)
                if let (ExprKind::Attribute { value: v2, attr: field }, Some(c)) = (&value.kind, class) {
                    if matches!(&v2.kind, ExprKind::Name(s) if s == "self") && attr == "append" {
                        let a0 = args.first().map(|a| self.eval(a, env, class)).unwrap_or(CT::Unknown);
                        if let Some(fields) = self.classes.get_mut(c) {
                            let cur = fields.get(field).cloned().unwrap_or(CT::Unknown);
                            if let CT::List(e) = &cur {
                                fields.insert(field.clone(), CT::List(Box::new(unify(e, &a0))));
                            }
                        }
                    }
                }
            }
        }
    }

    /// Propagate argument types into callee parameter types (call-site inference).
    fn calls_in(&mut self, e: &Expr, env: &mut HashMap<String, CT>, class: Option<&String>, yields: &mut Vec<CT>) {
        let mut calls: Vec<&Expr> = Vec::new();
        fn collect<'x>(e: &'x Expr, out: &mut Vec<&'x Expr>) {
            if let ExprKind::Call { .. } = &e.kind {
                out.push(e);
            }
            if let ExprKind::Lambda { .. } = &e.kind {
                return;
            }
            scope::walk_children(e, &mut |c| collect(c, out));
        }
        collect(e, &mut calls);
        if let ExprKind::Yield(Some(v)) = &e.kind {
            yields.push(self.eval(v, env, class));
        }
        if let ExprKind::YieldFrom(v) = &e.kind {
            let t = self.eval(v, env, class);
            yields.push(t.elem());
        }
        for c in calls {
            if let ExprKind::Call { func, args, keywords } = &c.kind {
                let target = match &func.kind {
                    ExprKind::Name(n) => {
                        if self.classes.contains_key(n) {
                            self.index.get(&format!("{}.__init__", n)).copied().map(|i| (i, 1))
                        } else {
                            self.index.get(n).copied().map(|i| (i, 0))
                        }
                    }
                    ExprKind::Attribute { value, attr } => {
                        let rt = self.eval(value, env, class);
                        match rt {
                            CT::Class(cn) => self.index.get(&format!("{}.{}", cn, attr)).copied().map(|i| (i, if self.funcs[i].is_method { 1 } else { 0 })),
                            _ => None,
                        }
                    }
                    _ => None,
                };
                let Some((fi, skip)) = target else { continue };
                for (k, a) in args.iter().enumerate() {
                    if matches!(a.kind, ExprKind::Starred(_)) {
                        break;
                    }
                    let t = self.eval(a, env, class);
                    if let Some(p) = self.funcs[fi].params.get_mut(k + skip) {
                        if t.known() && t != CT::NoneT {
                            p.1 = unify(&p.1, &t);
                        } else if t == CT::NoneT {
                            p.1 = unify(&p.1, &CT::NoneT);
                        }
                    }
                }
                for kw in keywords {
                    if let Some(name) = &kw.name {
                        let t = self.eval(&kw.value, env, class);
                        if let Some(p) = self.funcs[fi].params.iter_mut().find(|p| &p.0 == name) {
                            p.1 = unify(&p.1, &t);
                        }
                    }
                }
            }
        }
    }

    fn bind(&mut self, t: &Expr, ty: &CT, env: &mut HashMap<String, CT>, class: Option<&String>) {
        match &t.kind {
            ExprKind::Name(n) => {
                let cur = env.get(n).cloned().unwrap_or(CT::Unknown);
                env.insert(n.clone(), unify(&cur, ty));
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                for (i, e) in elts.iter().enumerate() {
                    let et = match ty {
                        CT::Tuple(ts) => ts.get(i).cloned().unwrap_or(CT::Unknown),
                        other => other.elem(),
                    };
                    self.bind(e, &et, env, class);
                }
            }
            ExprKind::Attribute { value, attr } => {
                if let (ExprKind::Name(s), Some(c)) = (&value.kind, class) {
                    if s == "self" {
                        let fields = self.classes.entry(c.clone()).or_default();
                        let cur = fields.get(attr).cloned().unwrap_or(CT::Unknown);
                        fields.insert(attr.clone(), unify(&cur, ty));
                    }
                }
            }
            ExprKind::Subscript { value, index } => {
                // self.d[k] = v refines attribute dict types
                if let (ExprKind::Attribute { value: v2, attr }, Some(c)) = (&value.kind, class) {
                    if matches!(&v2.kind, ExprKind::Name(s) if s == "self") {
                        let kt = self.eval(index, env, class);
                        let fields = self.classes.entry(c.clone()).or_default();
                        if let Some(CT::Dict(k, v)) = fields.get(attr).cloned() {
                            fields.insert(attr.clone(), CT::Dict(Box::new(unify(&k, &kt)), Box::new(unify(&v, ty))));
                        }
                    }
                }
                // d[k] = v refines dict types
                if let ExprKind::Name(n) = &value.kind {
                    let kt = self.eval(index, env, class);
                    let cur = env.get(n).cloned().unwrap_or(CT::Unknown);
                    if let CT::Dict(k, v) = cur {
                        env.insert(n.clone(), CT::Dict(Box::new(unify(&k, &kt)), Box::new(unify(&v, ty))));
                    }
                }
            }
            _ => {}
        }
    }

    pub fn eval(&self, e: &Expr, env: &HashMap<String, CT>, class: Option<&String>) -> CT {
        match &e.kind {
            ExprKind::Int(_) | ExprKind::BigInt(_) => CT::Int,
            ExprKind::Float(_) => CT::Float,
            ExprKind::Bool(_) => CT::Bool,
            ExprKind::Str { .. } | ExprKind::FStr { .. } => CT::Str,
            ExprKind::Bytes { .. } => CT::Bytes,
            ExprKind::NoneLit => CT::NoneT,
            ExprKind::Name(n) => {
                if let Some(t) = env.get(n) {
                    return t.clone();
                }
                if let Some(t) = self.globals.get(n) {
                    return t.clone();
                }
                CT::Unknown
            }
            ExprKind::BinOp { left, op, right } => {
                let a = self.eval(left, env, class);
                let b = self.eval(right, env, class);
                match (op, &a, &b) {
                    (BinOp::Div, x, y) if is_num(x) && is_num(y) => CT::Float,
                    (BinOp::Mod, CT::Str, _) => CT::Str,
                    (BinOp::Mul, CT::Str, _) | (BinOp::Mul, _, CT::Str) => CT::Str,
                    (BinOp::Mul, CT::List(_), _) => a.clone(),
                    (BinOp::Add, CT::Str, CT::Str) => CT::Str,
                    (BinOp::Add, CT::List(_), CT::List(_)) => unify(&a, &b),
                    (_, CT::Unknown, y) if is_num(y) && *op != BinOp::Div => y.clone(),
                    (_, x, CT::Unknown) if is_num(x) && *op != BinOp::Div => x.clone(),
                    (_, x, y) if is_num(x) && is_num(y) => {
                        if *x == CT::Float || *y == CT::Float {
                            CT::Float
                        } else {
                            CT::Int
                        }
                    }
                    (BinOp::BitOr | BinOp::BitAnd | BinOp::Sub | BinOp::BitXor, CT::Set(_), _) => a.clone(),
                    _ => CT::Unknown,
                }
            }
            ExprKind::UnaryOp { op: UnOp::Not, .. } => CT::Bool,
            ExprKind::UnaryOp { operand, .. } => self.eval(operand, env, class),
            ExprKind::Compare { .. } => CT::Bool,
            ExprKind::BoolOp { values, .. } => {
                let mut t = CT::Unknown;
                for v in values {
                    t = unify(&t, &self.eval(v, env, class));
                }
                t
            }
            ExprKind::IfExp { body, orelse, .. } => unify(&self.eval(body, env, class), &self.eval(orelse, env, class)),
            ExprKind::List(elts) => {
                let mut t = CT::Unknown;
                for x in elts {
                    t = unify(&t, &self.eval(x, env, class));
                }
                CT::List(Box::new(t))
            }
            ExprKind::Set(elts) => {
                let mut t = CT::Unknown;
                for x in elts {
                    t = unify(&t, &self.eval(x, env, class));
                }
                CT::Set(Box::new(t))
            }
            ExprKind::Tuple { elts, .. } => CT::Tuple(elts.iter().map(|x| self.eval(x, env, class)).collect()),
            ExprKind::Dict(items) => {
                let (mut k, mut v) = (CT::Unknown, CT::Unknown);
                for (kk, vv) in items {
                    if let Some(kk) = kk {
                        k = unify(&k, &self.eval(kk, env, class));
                    }
                    v = unify(&v, &self.eval(vv, env, class));
                }
                CT::Dict(Box::new(k), Box::new(v))
            }
            ExprKind::ListComp { elt, gens } | ExprKind::SetComp { elt, gens } | ExprKind::GenExp { elt, gens } => {
                let mut env2 = env.clone();
                for g in gens {
                    let it = self.eval(&g.iter, &env2, class);
                    let et = iter_elem_static(&g.iter, &it, self, &env2, class);
                    bind_simple(&g.target, &et, &mut env2);
                }
                let t = self.eval(elt, &env2, class);
                match &e.kind {
                    ExprKind::ListComp { .. } => CT::List(Box::new(t)),
                    ExprKind::SetComp { .. } => CT::Set(Box::new(t)),
                    _ => CT::Iter(Box::new(t)),
                }
            }
            ExprKind::DictComp { key, value, gens } => {
                let mut env2 = env.clone();
                for g in gens {
                    let it = self.eval(&g.iter, &env2, class);
                    let et = iter_elem_static(&g.iter, &it, self, &env2, class);
                    bind_simple(&g.target, &et, &mut env2);
                }
                CT::Dict(Box::new(self.eval(key, &env2, class)), Box::new(self.eval(value, &env2, class)))
            }
            ExprKind::Subscript { value, index } => {
                let v = self.eval(value, env, class);
                if let ExprKind::Slice { .. } = index.kind {
                    return v;
                }
                match &v {
                    CT::List(e) => (**e).clone(),
                    CT::Dict(_, val) => (**val).clone(),
                    CT::Str => CT::Str,
                    CT::Bytes => CT::Int,
                    CT::Tuple(ts) => match &index.kind {
                        ExprKind::Int(i) => ts.get(*i as usize).cloned().unwrap_or(CT::Unknown),
                        _ => CT::Unknown,
                    },
                    CT::Any => CT::Any,
                    _ => CT::Unknown,
                }
            }
            ExprKind::Attribute { value, attr } => {
                if let ExprKind::Name(m) = &value.kind {
                    if m == "self" {
                        if let Some(c) = class {
                            return self.classes.get(c).and_then(|f| f.get(attr)).cloned().unwrap_or(CT::Unknown);
                        }
                    }
                    let mt = module_attr_type(m, attr);
                    if mt.known() && !env.contains_key(m) {
                        return mt;
                    }
                }
                match self.eval(value, env, class) {
                    CT::Class(c) => self.classes.get(&c).and_then(|f| f.get(attr)).cloned().unwrap_or(CT::Unknown),
                    _ => CT::Unknown,
                }
            }
            ExprKind::Call { func, args, .. } => self.eval_call(func, args, env, class),
            ExprKind::NamedExpr { value, .. } => self.eval(value, env, class),
            _ => CT::Unknown,
        }
    }

    fn eval_call(&self, func: &Expr, args: &[Expr], env: &HashMap<String, CT>, class: Option<&String>) -> CT {
        let a0 = || args.first().map(|a| self.eval(a, env, class)).unwrap_or(CT::Unknown);
        match &func.kind {
            ExprKind::Name(n) => match n.as_str() {
                "len" | "int" | "ord" | "hash" | "id" | "round" if !(n == "round" && args.len() == 2) => CT::Int,
                "round" => CT::Float,
                "str" | "repr" | "chr" | "input" | "hex" | "oct" | "bin" | "format" => CT::Str,
                "float" => CT::Float,
                "bool" | "isinstance" | "callable" | "any" | "all" | "hasattr" | "issubclass" => CT::Bool,
                "bytes" => CT::Bytes,
                "list" | "sorted" => CT::List(Box::new(a0().elem())),
                "set" | "frozenset" => CT::Set(Box::new(a0().elem())),
                "tuple" => CT::Unknown,
                "dict" => {
                    if args.is_empty() {
                        CT::Dict(Box::new(CT::Unknown), Box::new(CT::Unknown))
                    } else {
                        a0()
                    }
                }
                "abs" => a0(),
                "sum" => {
                    let e = a0().elem();
                    if is_num(&e) {
                        e
                    } else {
                        CT::Int
                    }
                }
                "min" | "max" => {
                    if args.len() == 1 {
                        a0().elem()
                    } else {
                        let mut t = CT::Unknown;
                        for a in args {
                            t = unify(&t, &self.eval(a, env, class));
                        }
                        t
                    }
                }
                "enumerate" => CT::Iter(Box::new(CT::Tuple(vec![CT::Int, a0().elem()]))),
                "zip" => CT::Iter(Box::new(CT::Tuple(args.iter().map(|a| self.eval(a, env, class).elem()).collect()))),
                "range" => CT::Iter(Box::new(CT::Int)),
                "reversed" => CT::Iter(Box::new(a0().elem())),
                "divmod" => CT::Tuple(vec![CT::Int, CT::Int]),
                "open" => CT::Class("TextFile".into()),
                _ => {
                    if self.classes.contains_key(n) {
                        return CT::Class(n.clone());
                    }
                    match self.index.get(n) {
                        Some(&i) => self.funcs[i].ret.clone(),
                        None => CT::Unknown,
                    }
                }
            },
            ExprKind::Attribute { value, attr } => {
                if let ExprKind::Name(m) = &value.kind {
                    if !env.contains_key(m) && m != "self" {
                        let mt = module_attr_type(m, attr);
                        if mt.known() {
                            return mt;
                        }
                    }
                }
                let recv = self.eval(value, env, class);
                match (&recv, attr.as_str()) {
                    (CT::Str, "upper" | "lower" | "strip" | "lstrip" | "rstrip" | "replace" | "join" | "title" | "capitalize" | "format" | "center" | "ljust" | "rjust" | "zfill" | "removeprefix" | "removesuffix" | "swapcase" | "casefold") => CT::Str,
                    (CT::Str, "split" | "rsplit" | "splitlines") => CT::List(Box::new(CT::Str)),
                    (CT::Str, "find" | "rfind" | "index" | "count") => CT::Int,
                    (CT::Str, "startswith" | "endswith" | "isdigit" | "isalpha" | "isalnum" | "isspace" | "isupper" | "islower") => CT::Bool,
                    (CT::Str, "encode") => CT::Bytes,
                    (CT::Bytes, "decode") => CT::Str,
                    (CT::List(e), "pop") => (**e).clone(),
                    (CT::List(_), "index" | "count") => CT::Int,
                    (CT::List(_), "copy") => recv.clone(),
                    (CT::Dict(_, v), "get") => {
                        if args.len() >= 2 {
                            unify(v, &self.eval(&args[1], env, class))
                        } else {
                            CT::Opt(v.clone())
                        }
                    }
                    (CT::Dict(_, v), "pop" | "setdefault") => (**v).clone(),
                    (CT::Dict(k, _), "keys") => CT::List(k.clone()),
                    (CT::Dict(_, v), "values") => CT::List(v.clone()),
                    (CT::Dict(k, v), "items") => CT::List(Box::new(CT::Tuple(vec![(**k).clone(), (**v).clone()]))),
                    (CT::Dict(..), "copy") => recv.clone(),
                    (CT::Set(e), "pop") => (**e).clone(),
                    (CT::Set(_), "union" | "intersection" | "difference" | "copy") => recv.clone(),
                    (CT::Class(c), m) => match self.index.get(&format!("{}.{}", c, m)) {
                        Some(&i) => self.funcs[i].ret.clone(),
                        None => CT::Unknown,
                    },
                    _ => CT::Unknown,
                }
            }
            _ => CT::Unknown,
        }
    }
}

fn is_num(t: &CT) -> bool {
    matches!(t, CT::Int | CT::Float | CT::Bool)
}

fn literal_type(e: &Expr) -> CT {
    match &e.kind {
        ExprKind::Int(_) => CT::Int,
        ExprKind::Float(_) => CT::Float,
        ExprKind::Bool(_) => CT::Bool,
        ExprKind::Str { .. } => CT::Str,
        ExprKind::Bytes { .. } => CT::Bytes,
        ExprKind::NoneLit => CT::NoneT,
        ExprKind::UnaryOp { operand, .. } => literal_type(operand),
        ExprKind::Tuple { elts, .. } => CT::Tuple(elts.iter().map(literal_type).collect()),
        _ => CT::Unknown,
    }
}

fn bind_simple(t: &Expr, ty: &CT, env: &mut HashMap<String, CT>) {
    match &t.kind {
        ExprKind::Name(n) => {
            env.insert(n.clone(), ty.clone());
        }
        ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
            for (i, e) in elts.iter().enumerate() {
                let et = match ty {
                    CT::Tuple(ts) => ts.get(i).cloned().unwrap_or(CT::Unknown),
                    other => other.elem(),
                };
                bind_simple(e, &et, env);
            }
        }
        _ => {}
    }
}

fn iter_elem(iter: &Expr, it: &CT, inf: &Infer, env: &HashMap<String, CT>, class: Option<&String>) -> CT {
    iter_elem_static(iter, it, inf, env, class)
}

fn iter_elem_static(iter: &Expr, it: &CT, inf: &Infer, env: &HashMap<String, CT>, class: Option<&String>) -> CT {
    if let ExprKind::Call { func, args, .. } = &iter.kind {
        if let ExprKind::Attribute { value, attr } = &func.kind {
            let d = inf.eval(value, env, class);
            if let CT::Dict(k, v) = d {
                return match attr.as_str() {
                    "items" => CT::Tuple(vec![*k, *v]),
                    "keys" => *k,
                    "values" => *v,
                    _ => CT::Unknown,
                };
            }
        }
        let _ = args;
    }
    it.elem()
}

/// Map an existing annotation to CT (for already-annotated code).
fn ann_to_ct(e: &Expr) -> CT {
    match &e.kind {
        ExprKind::Name(n) => match n.as_str() {
            "int" => CT::Int,
            "float" => CT::Float,
            "bool" => CT::Bool,
            "str" => CT::Str,
            "bytes" => CT::Bytes,
            "None" => CT::NoneT,
            "Any" | "object" => CT::Any,
            other => CT::Class(other.to_string()),
        },
        ExprKind::NoneLit => CT::NoneT,
        ExprKind::Subscript { value, index } => {
            let args: Vec<CT> = match &index.kind {
                ExprKind::Tuple { elts, .. } => elts.iter().map(ann_to_ct).collect(),
                _ => vec![ann_to_ct(index)],
            };
            match &value.kind {
                ExprKind::Name(n) => match n.as_str() {
                    "list" | "List" => CT::List(Box::new(args[0].clone())),
                    "set" | "Set" => CT::Set(Box::new(args[0].clone())),
                    "dict" | "Dict" => CT::Dict(Box::new(args[0].clone()), Box::new(args.get(1).cloned().unwrap_or(CT::Any))),
                    "tuple" | "Tuple" => CT::Tuple(args),
                    "Optional" => CT::Opt(Box::new(args[0].clone())),
                    "Iterator" | "Iterable" | "Generator" => CT::Iter(Box::new(args[0].clone())),
                    _ => CT::Any,
                },
                _ => CT::Any,
            }
        }
        ExprKind::BinOp { left, op: BinOp::BitOr, right } => unify(&ann_to_ct(left), &ann_to_ct(right)),
        _ => CT::Any,
    }
}

// ---------------------------------------------------------------------- rewriting

#[derive(Default, Debug)]
pub struct Report {
    pub annotated_params: usize,
    pub annotated_returns: usize,
    pub any_types: usize,
    pub fstrings: usize,
    pub notes: Vec<(usize, String)>,
}

struct Edit {
    pos: usize,
    end: usize,
    text: String,
}

pub struct ConvertOptions {
    pub trace: Option<HashMap<usize, (HashMap<String, CT>, CT)>>,
}

pub fn convert_source(src: &str, file: &str, opts: &ConvertOptions) -> Result<(String, Report), crate::diag::Diag> {
    let parsed = parser::parse(src, file)?;
    let m = &parsed.module;
    let mut inf = Infer::new(m);
    // seed from runtime trace
    if let Some(tr) = &opts.trace {
        for f in inf.funcs.iter_mut() {
            if let Some((ps, r)) = tr.get(&f.def_line) {
                for p in f.params.iter_mut() {
                    if let Some(t) = ps.get(&p.0) {
                        if !(f.is_method && p.0 == "self") {
                            p.1 = unify(&p.1, t);
                        }
                    }
                }
                if r.known() && !f.has_yield {
                    f.ret = unify(&f.ret, r);
                }
            }
        }
    }
    inf.run();
    let mut edits: Vec<Edit> = Vec::new();
    let mut rep = Report::default();
    let mut need_any = false;
    let mut need_iter = false;
    let mut need_callable = false;
    let _ = &mut need_callable;

    let mut visit_fn = |f: &FuncDef, class: Option<&str>, edits: &mut Vec<Edit>, rep: &mut Report| {
        let key = match class {
            Some(c) => format!("{}.{}", c, f.name),
            None => f.name.clone(),
        };
        let Some(&idx) = inf.index.get(&key) else { return };
        let rec = &inf.funcs[idx];
        for (i, a) in f.params.all_positional().chain(f.params.kwonly.iter()).enumerate() {
            if a.ann.is_some() || (rec.is_method && i == 0) {
                continue;
            }
            let mut t = rec.params[i].1.clone();
            if t == CT::NoneT {
                t = CT::Opt(Box::new(CT::Any));
            }
            let txt = t.render();
            if t.uses_any() {
                rep.any_types += 1;
                need_any = true;
                rep.notes.push((a.span.line, format!("parameter '{}' of '{}' typed as {} (could not infer; refine by hand or use --trace)", a.name, f.name, txt)));
            }
            let pos = a.span.start + a.name.len();
            edits.push(Edit { pos, end: pos, text: format!(": {}", txt) });
            rep.annotated_params += 1;
        }
        for a in f.params.vararg.iter().chain(f.params.kwarg.iter()) {
            if a.ann.is_none() {
                need_any = true;
                rep.any_types += 1;
                let name_pos = src[a.span.start..].find(&a.name).map(|o| a.span.start + o + a.name.len()).unwrap_or(a.span.start);
                edits.push(Edit { pos: name_pos, end: name_pos, text: ": Any".into() });
                rep.notes.push((a.span.line, format!("*{} of '{}' typed as Any", a.name, f.name)));
            }
        }
        if f.returns.is_none() && f.name != "__init__" {
            let t = rec.ret.clone();
            let txt = t.render();
            if matches!(t, CT::Iter(_)) {
                need_iter = true;
            }
            if t.uses_any() {
                rep.any_types += 1;
                need_any = true;
                rep.notes.push((f.header_lines.0, format!("return type of '{}' is {} (could not infer)", f.name, txt)));
            }
            edits.push(Edit { pos: f.close_paren_pos + 1, end: f.close_paren_pos + 1, text: format!(" -> {}", txt) });
            rep.annotated_returns += 1;
        }
    };

    // functions and methods
    for st in &m.body {
        match &st.kind {
            StmtKind::FuncDef(f) => visit_fn(f, None, &mut edits, &mut rep),
            StmtKind::ClassDef(c) => {
                for s in &c.body {
                    if let StmtKind::FuncDef(f) = &s.kind {
                        visit_fn(f, Some(&c.name), &mut edits, &mut rep);
                    }
                }
            }
            _ => {}
        }
    }
    // class attributes: annotate first `self.x = ...` in __init__ when not declared in the class body
    for st in &m.body {
        if let StmtKind::ClassDef(c) = &st.kind {
            let declared: Vec<String> = c.body.iter().filter_map(|s| if let StmtKind::AnnAssign { target, .. } = &s.kind { if let ExprKind::Name(n) = &target.kind { Some(n.clone()) } else { None } } else { None }).collect();
            let fields = inf.classes.get(&c.name).cloned().unwrap_or_default();
            if let Some(init) = c.body.iter().find_map(|s| if let StmtKind::FuncDef(f) = &s.kind { if f.name == "__init__" { Some(f) } else { None } } else { None }) {
                let mut done = Vec::new();
                annotate_self_assigns(&init.body, &fields, &declared, &mut done, &mut edits, &mut rep, &mut need_any);
                // attributes assigned only outside __init__: declare at class level
                let mut extra = Vec::new();
                for (n, t) in &fields {
                    if !done.contains(n) && !declared.contains(n) {
                        extra.push((n.clone(), t.clone()));
                    }
                }
                if !extra.is_empty() {
                    if let Some(first) = c.body.first() {
                        let line_start = line_start_offset(src, first.span.start);
                        let indent: String = src[line_start..first.span.start].chars().take_while(|ch| ch.is_whitespace()).collect();
                        let mut text = String::new();
                        for (n, t) in extra {
                            if t.uses_any() {
                                need_any = true;
                                rep.any_types += 1;
                            }
                            text.push_str(&format!("{}{}: {}\n", indent, n, t.render()));
                        }
                        // after a docstring, if any
                        let pos = match &first.kind {
                            StmtKind::Expr(Expr { kind: ExprKind::Str { .. }, .. }) => next_line_offset(src, first.span.end),
                            _ => line_start,
                        };
                        edits.push(Edit { pos, end: pos, text });
                    }
                }
            }
        }
    }
    // empty containers bound to names: annotate from usage
    collect_empty_literal_annotations(&m.body, &inf, None, &mut edits, &mut rep, &mut need_any);
    // % formatting and .format() -> f-strings
    let mut fstr_edits = Vec::new();
    find_format_rewrites(&m.body, src, &mut fstr_edits);
    rep.fstrings = fstr_edits.len();
    edits.extend(fstr_edits);
    // unsupported constructs
    notes_for_unsupported(&m.body, &mut rep);

    // apply edits (descending)
    edits.sort_by(|a, b| b.pos.cmp(&a.pos).then(b.end.cmp(&a.end)));
    let mut out = src.to_string();
    let mut last = usize::MAX;
    for e in edits {
        if e.end > last {
            continue; // overlapping edit
        }
        out.replace_range(e.pos..e.end, &e.text);
        last = e.pos;
    }
    // typing imports
    let mut imports = Vec::new();
    if need_any && !src.contains("import Any") {
        imports.push("Any");
    }
    if need_iter && !src.contains("Iterator") {
        imports.push("Iterator");
    }
    if !imports.is_empty() {
        let line = format!("from typing import {}\n", imports.join(", "));
        let pos = import_insert_pos(&out);
        out.insert_str(pos, &line);
    }
    let formatted = crate::fmt::format_source(&out, file).unwrap_or(out);
    rep.notes.sort();
    Ok((formatted, rep))
}

fn annotate_self_assigns(body: &[Stmt], fields: &BTreeMap<String, CT>, declared: &[String], done: &mut Vec<String>, edits: &mut Vec<Edit>, rep: &mut Report, need_any: &mut bool) {
    for st in body {
        match &st.kind {
            StmtKind::Assign { targets, value } if targets.len() == 1 => {
                if let ExprKind::Attribute { value: v, attr } = &targets[0].kind {
                    if matches!(&v.kind, ExprKind::Name(n) if n == "self") && !done.contains(attr) && !declared.contains(attr) {
                        done.push(attr.clone());
                        let simple = matches!(value.kind, ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str { .. } | ExprKind::Bool(_));
                        if simple {
                            continue;
                        }
                        let t = fields.get(attr).cloned().unwrap_or(CT::Unknown);
                        if t.uses_any() {
                            *need_any = true;
                            rep.any_types += 1;
                            rep.notes.push((st.span.line, format!("attribute '{}' typed as {}", attr, t.render())));
                        }
                        let pos = targets[0].span.end;
                        edits.push(Edit { pos, end: pos, text: format!(": {}", t.render()) });
                    }
                }
            }
            StmtKind::If { body, orelse, .. } => {
                annotate_self_assigns(body, fields, declared, done, edits, rep, need_any);
                annotate_self_assigns(orelse, fields, declared, done, edits, rep, need_any);
            }
            _ => {}
        }
    }
}

fn collect_empty_literal_annotations(body: &[Stmt], inf: &Infer, class: Option<&String>, edits: &mut Vec<Edit>, rep: &mut Report, need_any: &mut bool) {
    // per function: names first assigned an empty literal; infer from later usage in the same body
    for st in body {
        match &st.kind {
            StmtKind::FuncDef(f) => {
                let key = match class {
                    Some(c) => format!("{}.{}", c, f.name),
                    None => f.name.clone(),
                };
                let mut env: HashMap<String, CT> = match inf.index.get(&key) {
                    Some(&i) => inf.funcs[i].params.iter().cloned().collect(),
                    None => HashMap::new(),
                };
                // replay inference to get final local types
                let mut tmp = Infer { module: inf.module, funcs: inf.funcs.clone(), index: inf.index.clone(), classes: inf.classes.clone(), globals: inf.globals.clone() };
                tmp.walk_block(&f.body, &mut env, class, &mut Vec::new(), &mut Vec::new());
                annotate_empties(&f.body, &env, edits, rep, need_any, &mut Vec::new());
            }
            StmtKind::ClassDef(c) => collect_empty_literal_annotations(&c.body, inf, Some(&c.name), edits, rep, need_any),
            _ => {}
        }
    }
    // module level
    let mut genv = inf.globals.clone();
    let mut tmp = Infer { module: inf.module, funcs: inf.funcs.clone(), index: inf.index.clone(), classes: inf.classes.clone(), globals: HashMap::new() };
    if class.is_none() {
        tmp.walk_block(body, &mut genv, None, &mut Vec::new(), &mut Vec::new());
        annotate_empties(body, &genv, edits, rep, need_any, &mut Vec::new());
    }
}

fn annotate_empties(body: &[Stmt], env: &HashMap<String, CT>, edits: &mut Vec<Edit>, rep: &mut Report, need_any: &mut bool, seen: &mut Vec<String>) {
    for st in body {
        match &st.kind {
            StmtKind::Assign { targets, value } if targets.len() == 1 => {
                if let ExprKind::Name(n) = &targets[0].kind {
                    let empty = match &value.kind {
                        ExprKind::List(v) | ExprKind::Set(v) => v.is_empty(),
                        ExprKind::Dict(v) => v.is_empty(),
                        ExprKind::Call { func, args, .. } => args.is_empty() && matches!(&func.kind, ExprKind::Name(f) if matches!(f.as_str(), "list" | "dict" | "set")),
                        ExprKind::NoneLit => true,
                        _ => false,
                    };
                    if empty && !seen.contains(n) {
                        seen.push(n.clone());
                        let t = env.get(n).cloned().unwrap_or(CT::Unknown);
                        let t = match (&value.kind, t) {
                            (ExprKind::List(_), CT::Unknown) => CT::List(Box::new(CT::Any)),
                            (ExprKind::Dict(_), CT::Unknown) => CT::Dict(Box::new(CT::Any), Box::new(CT::Any)),
                            (ExprKind::NoneLit, CT::NoneT) => CT::Opt(Box::new(CT::Any)),
                            (_, t) => t,
                        };
                        if t.uses_any() {
                            *need_any = true;
                            rep.any_types += 1;
                            rep.notes.push((st.span.line, format!("'{}' typed as {}", n, t.render())));
                        }
                        let pos = targets[0].span.end;
                        edits.push(Edit { pos, end: pos, text: format!(": {}", t.render()) });
                    }
                }
            }
            StmtKind::If { body, orelse, .. } | StmtKind::While { body, orelse, .. } | StmtKind::For { body, orelse, .. } => {
                annotate_empties(body, env, edits, rep, need_any, seen);
                annotate_empties(orelse, env, edits, rep, need_any, seen);
            }
            StmtKind::With { body, .. } => annotate_empties(body, env, edits, rep, need_any, seen),
            StmtKind::Try { body, handlers, orelse, finalbody, .. } => {
                annotate_empties(body, env, edits, rep, need_any, seen);
                for h in handlers {
                    annotate_empties(&h.body, env, edits, rep, need_any, seen);
                }
                annotate_empties(orelse, env, edits, rep, need_any, seen);
                annotate_empties(finalbody, env, edits, rep, need_any, seen);
            }
            _ => {}
        }
    }
}

/// Rewrite `"fmt" % args` and `"fmt".format(args)` into f-strings when the arguments are simple.
fn find_format_rewrites(body: &[Stmt], src: &str, out: &mut Vec<Edit>) {
    fn visit(e: &Expr, src: &str, out: &mut Vec<Edit>) {
        if let Some(edit) = rewrite_percent(e, src).or_else(|| rewrite_format(e, src)) {
            out.push(edit);
            return;
        }
        scope::walk_children(e, &mut |c| visit(c, src, out));
    }
    for st in body {
        match &st.kind {
            StmtKind::FuncDef(f) => find_format_rewrites(&f.body, src, out),
            StmtKind::ClassDef(c) => find_format_rewrites(&c.body, src, out),
            _ => {
                let mut nodes = Vec::new();
                scope::walk_stmt(st, &mut |n| nodes.push(n));
                for n in nodes {
                    match n {
                        scope::Node::E(e) => visit(e, src, out),
                        scope::Node::B(b) => find_format_rewrites(b, src, out),
                    }
                }
            }
        }
    }
}

fn simple_arg_src(e: &Expr, src: &str) -> Option<String> {
    let s = src.get(e.span.start..e.span.end)?;
    if s.contains('\n') || s.contains('"') || s.contains('\'') || s.contains('{') || s.contains('}') || s.contains('\\') {
        return None;
    }
    Some(s.to_string())
}

fn single_literal(e: &Expr) -> Option<(String, char)> {
    if let ExprKind::Str { value, raw } = &e.kind {
        if raw.len() == 1 {
            let r = &raw[0];
            let prefix_ok = r.starts_with('"') || r.starts_with('\'');
            if prefix_ok && !r.starts_with("\"\"\"") && !r.starts_with("'''") && !value.contains('{') && !value.contains('}') {
                return Some((value.clone(), r.chars().next().unwrap()));
            }
        }
    }
    None
}

fn escape_lit(s: &str, q: char) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if c == q => {
                o.push('\\');
                o.push(c)
            }
            c => o.push(c),
        }
    }
    o
}

fn rewrite_percent(e: &Expr, src: &str) -> Option<Edit> {
    let ExprKind::BinOp { left, op: BinOp::Mod, right } = &e.kind else { return None };
    let (fmt, q) = single_literal(left)?;
    let args: Vec<&Expr> = match &right.kind {
        ExprKind::Tuple { elts, .. } => elts.iter().collect(),
        _ => vec![&**right],
    };
    let mut argsrc = Vec::new();
    for a in &args {
        argsrc.push(simple_arg_src(a, src)?);
    }
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut lit = String::new();
    let mut ai = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c != '%' {
            lit.push(c);
            i += 1;
            continue;
        }
        if chars.get(i + 1) == Some(&'%') {
            lit.push('%');
            i += 2;
            continue;
        }
        // %[flags][width][.prec]type
        let mut j = i + 1;
        let mut spec = String::new();
        while j < chars.len() && "-+ #0123456789.".contains(chars[j]) {
            spec.push(chars[j]);
            j += 1;
        }
        let ty = *chars.get(j)?;
        let a = argsrc.get(ai)?.clone();
        ai += 1;
        out.push_str(&escape_lit(&lit, q));
        lit.clear();
        let field = match ty {
            's' if spec.is_empty() => format!("{{{}}}", a),
            'r' if spec.is_empty() => format!("{{{}!r}}", a),
            'd' | 'i' => format!("{{{}:{}d}}", a, spec.replace('-', "<")).replace(":d}", "}"),
            'f' | 'e' | 'g' | 'x' | 'X' | 'o' => format!("{{{}:{}{}}}", a, spec.replace('-', "<"), ty),
            's' => format!("{{{}!s:{}}}", a, spec.replace('-', "<")),
            _ => return None,
        };
        out.push_str(&field);
        i = j + 1;
    }
    if ai != argsrc.len() {
        return None;
    }
    out.push_str(&escape_lit(&lit, q));
    Some(Edit { pos: e.span.start, end: e.span.end, text: format!("f{}{}{}", q, out, q) })
}

fn rewrite_format(e: &Expr, src: &str) -> Option<Edit> {
    let ExprKind::Call { func, args, keywords } = &e.kind else { return None };
    let ExprKind::Attribute { value, attr } = &func.kind else { return None };
    if attr != "format" {
        return None;
    }
    let ExprKind::Str { value: fmt, raw } = &value.kind else { return None };
    if raw.len() != 1 || raw[0].starts_with("\"\"\"") || raw[0].starts_with("'''") || !(raw[0].starts_with('"') || raw[0].starts_with('\'')) {
        return None;
    }
    let q = raw[0].chars().next().unwrap();
    let mut pos_args = Vec::new();
    for a in args {
        pos_args.push(simple_arg_src(a, src)?);
    }
    let mut kw: HashMap<String, String> = HashMap::new();
    for k in keywords {
        kw.insert(k.name.clone()?, simple_arg_src(&k.value, src)?);
    }
    let chars: Vec<char> = fmt.chars().collect();
    let mut out = String::new();
    let mut lit = String::new();
    let mut auto = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '{' && chars.get(i + 1) == Some(&'{') {
            lit.push_str("{{");
            i += 2;
            continue;
        }
        if c == '}' && chars.get(i + 1) == Some(&'}') {
            lit.push_str("}}");
            i += 2;
            continue;
        }
        if c == '{' {
            let end = chars[i..].iter().position(|&x| x == '}')? + i;
            let field: String = chars[i + 1..end].iter().collect();
            let (name, rest) = match field.find(|ch| ch == ':' || ch == '!') {
                Some(p) => (field[..p].to_string(), field[p..].to_string()),
                None => (field.clone(), String::new()),
            };
            let expr = if name.is_empty() {
                let a = pos_args.get(auto)?.clone();
                auto += 1;
                a
            } else if let Ok(n) = name.parse::<usize>() {
                pos_args.get(n)?.clone()
            } else {
                kw.get(&name)?.clone()
            };
            out.push_str(&escape_lit(&lit, q).replace("\\\\{", "{"));
            lit.clear();
            out.push_str(&format!("{{{}{}}}", expr, rest));
            i = end + 1;
            continue;
        }
        lit.push(c);
        i += 1;
    }
    out.push_str(&escape_lit(&lit, q));
    Some(Edit { pos: e.span.start, end: e.span.end, text: format!("f{}{}{}", q, out, q) })
}

fn notes_for_unsupported(body: &[Stmt], rep: &mut Report) {
    for st in body {
        match &st.kind {
            StmtKind::FuncDef(f) => {
                if f.is_async {
                    rep.notes.push((st.span.line, format!("async function '{}' returns an Any-typed coroutine (awaited through asyncio)", f.name)));
                }
                for d in &f.decorators {
                    let known = match &d.kind {
                        ExprKind::Name(n) => matches!(n.as_str(), "staticmethod" | "classmethod" | "property" | "dataclass" | "extern" | "overload"),
                        _ => false,
                    };
                    if !known {
                        rep.notes.push((d.span.line, format!("decorator on '{}' is not supported by the native compiler (consider the CPython fallback)", f.name)));
                    }
                }
                notes_for_unsupported(&f.body, rep);
            }
            StmtKind::ClassDef(c) => {
                if c.bases.len() > 1 {
                    rep.notes.push((st.span.line, format!("class '{}' uses multiple inheritance", c.name)));
                }
                if !c.keywords.is_empty() {
                    rep.notes.push((st.span.line, format!("class '{}' uses a metaclass/keywords", c.name)));
                }
                notes_for_unsupported(&c.body, rep);
            }
            _ => {
                let mut nodes = Vec::new();
                scope::walk_stmt(st, &mut |n| nodes.push(n));
                for n in nodes {
                    match n {
                        scope::Node::E(e) => dynamic_calls(e, rep),
                        scope::Node::B(b) => notes_for_unsupported(b, rep),
                    }
                }
            }
        }
    }
}

fn dynamic_calls(e: &Expr, rep: &mut Report) {
    if let ExprKind::Call { func, .. } = &e.kind {
        if let ExprKind::Name(n) = &func.kind {
            if matches!(n.as_str(), "eval" | "exec" | "globals" | "locals" | "vars" | "__import__") {
                rep.notes.push((e.span.line, format!("dynamic builtin '{}' requires Any-typed code or the CPython fallback", n)));
            }
        }
    }
    scope::walk_children(e, &mut |c| dynamic_calls(c, rep));
}

fn line_start_offset(src: &str, pos: usize) -> usize {
    src[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn next_line_offset(src: &str, pos: usize) -> usize {
    src[pos..].find('\n').map(|i| pos + i + 1).unwrap_or(src.len())
}

/// Position after the module docstring and `from __future__` imports.
fn import_insert_pos(src: &str) -> usize {
    let parsed = match parser::parse(src, "<convert>") {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let mut pos = 0;
    for st in &parsed.module.body {
        match &st.kind {
            StmtKind::Expr(Expr { kind: ExprKind::Str { .. }, .. }) if pos == 0 => pos = next_line_offset(src, st.span.end),
            StmtKind::ImportFrom { module, .. } if module == "__future__" => pos = next_line_offset(src, st.span.end),
            _ => break,
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conv(src: &str) -> String {
        convert_source(src, "t.py", &ConvertOptions { trace: None }).unwrap().0
    }

    #[test]
    fn infers_simple_functions() {
        let out = conv("def add(a, b):\n    return a + b\n\nprint(add(1, 2))\n");
        assert!(out.contains("def add(a: int, b: int) -> int:"), "{}", out);
    }

    #[test]
    fn infers_from_defaults_and_strings() {
        let out = conv("def greet(name, punct='!'):\n    return 'Hello ' + name + punct\n\ngreet('x')\n");
        assert!(out.contains("def greet(name: str, punct: str = \"!\") -> str:"), "{}", out);
    }

    #[test]
    fn none_return_and_lists() {
        let out = conv("def collect(n):\n    out = []\n    for i in range(n):\n        out.append(i * 2)\n    return out\n\ncollect(3)\n");
        assert!(out.contains("def collect(n: int) -> list[int]:"), "{}", out);
        assert!(out.contains("out: list[int] = []"), "{}", out);
    }

    #[test]
    fn percent_and_format_to_fstring() {
        let out = conv("def f(x: int, y: str) -> str:\n    a = 'x=%d y=%s' % (x, y)\n    return '{} and {y}'.format(x, y=y) + a\n");
        assert!(out.contains("f\"x={x} y={y}\""), "{}", out);
        assert!(out.contains("f\"{x} and {y}\""), "{}", out);
    }

    #[test]
    fn classes_and_attributes() {
        let out = conv("class P:\n    def __init__(self, x, y):\n        self.x = x\n        self.y = y\n        self.tags = []\n    def norm(self):\n        return self.x * self.x + self.y * self.y\n\np = P(1.5, 2.0)\n");
        assert!(out.contains("def __init__(self, x: float, y: float):"), "{}", out);
        assert!(out.contains("def norm(self) -> float:"), "{}", out);
    }

    #[test]
    fn unknowns_become_any() {
        let (out, rep) = convert_source("def f(x):\n    return x.thing()\n", "t.py", &ConvertOptions { trace: None }).unwrap();
        assert!(out.contains("from typing import Any"), "{}", out);
        assert!(out.contains("def f(x: Any) -> Any:"), "{}", out);
        assert!(rep.any_types >= 2);
    }

    #[test]
    fn generators() {
        let out = conv("def gen(n):\n    for i in range(n):\n        yield i\n\nlist(gen(3))\n");
        assert!(out.contains("def gen(n: int) -> Iterator[int]:"), "{}", out);
        assert!(out.contains("from typing import Iterator"), "{}", out);
    }

    #[test]
    fn trace_json_parsing() {
        let t = parse_trace_json(r#"{"1": {"args": {"x": ["int", "float"]}, "ret": ["list[str]"]}}"#).unwrap();
        let (ps, r) = &t[&1];
        assert_eq!(ps["x"], CT::Float);
        assert_eq!(*r, CT::List(Box::new(CT::Str)));
    }

    fn conv_report(src: &str) -> (String, Report) {
        convert_source(src, "t.py", &ConvertOptions { trace: None }).unwrap()
    }

    /// Inference across many expression and statement forms.
    #[test]
    fn infers_across_constructs() {
        let src = r#"import math
import os
import sys


def kinds(flag, data, raw):
    a = not flag
    b = -3 if flag else 4
    c = 7 / 2
    d = "ab" * 3
    e = [1] + [2]
    f = {1, 2} | {3}
    g = {k: len(k) for k in ["x", "yy"]}
    h = {n for n in range(3)}
    i = (n * 2 for n in range(3))
    j = (w := 5) + w
    k = math.floor(2.5) + math.gcd(4, 6)
    l = os.getenv("HOME")
    m = sys.platform
    n = data[0]
    o = "abc"[1:]
    p = raw.decode()
    q = round(2.5)
    r = "hello".find("l") + "x".count("x")
    s = {"a": 1}.get("a")
    t = list({"a": 1}.items())
    u = b"bytes"
    v = True
    x = (1, "two")
    y = sorted([3, 1])
    z = max([1, 2])
    print(a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p, q, r, s, t, u, v, x, y, z)
    return x


def maybe(n):
    if n > 0:
        return n
    return None


def loops(items):
    total = 0
    for idx, item in enumerate(items):
        total += idx * item
    pairs = []
    pairs.insert(0, (1, 2))
    try:
        total += 1
    except ValueError as err:
        print(err)
    else:
        total += 2
    with open("f") as fh:
        text = fh.read()
    match total:
        case 1:
            pass
    return total, pairs, text


kinds(True, [1, 2], b"x")
maybe(3)
loops([1, 2, 3])
"#;
        let (out, rep) = conv_report(src);
        assert!(out.contains("def maybe(n: int) -> int | None:"), "{}", out);
        assert!(out.contains("def loops(items: list[int])"), "{}", out);
        assert!(out.contains("def kinds(flag: bool, data: list[int], raw: bytes) -> tuple[int, str]:"), "{}", out);
        assert!(rep.annotated_params >= 5 && rep.annotated_returns >= 3, "{:?}", rep.notes);
        // the output is valid fpython syntax
        crate::parser::parse(&out, "t.fpy").unwrap();
    }

    #[test]
    fn notes_for_dynamic_code() {
        let src = "import functools\n\n\nclass A:\n    pass\n\n\nclass B:\n    pass\n\n\nclass C(A, B, metaclass=type):\n    pass\n\n\n@functools.cache\ndef f(x):\n    return eval(x)\n\n\nasync def g():\n    return 1\n";
        let (_, rep) = conv_report(src);
        let notes: Vec<&str> = rep.notes.iter().map(|n| n.1.as_str()).collect();
        for want in ["multiple inheritance", "metaclass", "decorator on 'f'", "dynamic builtin 'eval'", "async function 'g'"] {
            assert!(notes.iter().any(|n| n.contains(want)), "missing {:?} in {:?}", want, notes);
        }
    }

    #[test]
    fn format_rewrites_edge_cases() {
        let out = conv("def f(x: int, s: str) -> str:\n    a = '%5.2f%%' % x\n    b = '%r and %-5s' % (s, s)\n    c = '{{literal}} {0} {0}'.format(x)\n    d = 'quote \"%s\" \\\\ %s' % (s, s)\n    return a + b + c + d\n");
        assert!(out.contains("f\"{x:5.2f}%\""), "{}", out);
        assert!(out.contains("{s!r}"), "{}", out);
        assert!(out.contains("{{literal}} {x} {x}"), "{}", out);
        // unsupported forms are left alone
        let kept = conv("def f(t: tuple[int, int]) -> str:\n    return '%d %d' % t\n");
        assert!(kept.contains("'%d %d' % t") || kept.contains("\"%d %d\" % t"), "{}", kept);
    }

    #[test]
    fn unify_and_render() {
        use CT::*;
        assert_eq!(unify(&Bool, &Int), Int);
        assert_eq!(unify(&Opt(Box::new(Int)), &Int), Opt(Box::new(Int)));
        assert_eq!(unify(&Set(Box::new(Int)), &Set(Box::new(Bool))), Set(Box::new(Int)));
        assert_eq!(unify(&Tuple(vec![Int, Str]), &Tuple(vec![Bool, Str])), Tuple(vec![Int, Str]));
        assert_eq!(Tuple(vec![Int, Bytes, Bool]).render(), "tuple[int, bytes, bool]");
        assert_eq!(Set(Box::new(Str)).render(), "set[str]");
        assert!(Tuple(vec![Int, Any]).uses_any());
        assert_eq!(Dict(Box::new(Str), Box::new(Int)).elem(), Str);
        assert_eq!(parse_trace_type("set[int]"), Set(Box::new(Int)));
        assert_eq!(parse_trace_type("tuple[int, str]"), Tuple(vec![Int, Str]));
        assert_eq!(parse_trace_type("dict[str, list[int]]"), Dict(Box::new(Str), Box::new(List(Box::new(Int)))));
        assert_eq!(split_top("a[b, c], d"), vec!["a[b, c]".to_string(), "d".to_string()]);
    }

    #[test]
    fn trace_json_more() {
        let t = parse_trace_json(r#"{"3": {"args": {"x": ["int"], "y": ["list[str]", "NoneType"]}, "ret": ["float"], "extra": 1}, "9": {"args": {}, "ret": ["NoneType"]}}"#).unwrap();
        assert_eq!(t[&3].0["x"], CT::Int);
        assert_eq!(t[&3].1, CT::Float);
        assert!(parse_trace_json(r#"{"x": {}}"#).is_err());
        assert!(parse_trace_json("not json").unwrap().is_empty());
        assert!(parse_trace_json("{").is_err());
        assert!(mini_json(r#"{"a": [1, 2.5, true, false, null, "s\"q"]}"#).is_some());
        assert!(mini_json("{").is_none());
    }

    #[test]
    fn infers_more_forms() {
        let src = r#""""Module docstring."""
from __future__ import annotations
import math
import os
import random
import sys
import time
from typing import Optional


class Account:
    """An account."""

    def __init__(self, owner, balance=0.0):
        self.owner = owner
        self.balance = balance

    def open(self):
        self.history = []
        self.meta = {}
        self.history.append(self.balance)


def checks(x, items, mapping, label: Optional[str], pair: int | str):
    flags = [math.isnan(x), math.isfinite(x), x < 2, label is None]
    nums = [random.randint(1, 2), time.time_ns(), sys.maxsize, len(os.listdir("."))]
    first, second = items[0], items[-1]
    last = items.pop()
    words = "a b".split()
    ok = words[0].isdigit() or "x".startswith("y")
    copy = mapping.copy()
    doubled = items * 2
    rounded = round(x, 2)
    biggest = max(items)
    total = sum(items)
    absval = abs(x)
    with open("f") as fh:
        data = fh.read()
    try:
        pass
    except (KeyError, ValueError) as err:
        print(err)
    except OSError:
        pass
    unknown = []
    unknown.append(first)
    print(flags, nums, second, last, ok, copy, doubled, rounded, biggest, total, absval, data, unknown, pair)
    return "%s\n%r" % (label, x)


def kw_user(a, b=None):
    return a


acct = Account("bob")
acct.open()
checks(1.5, [1, 2, 3], {"k": 1}, None, 3)
kw_user(a=1, b="x")
kw_user(2)
"#;
        let (out, rep) = conv_report(src);
        assert!(out.contains("def __init__(self, owner: str, balance: float = 0.0):"), "{}", out);
        assert!(out.contains("    history: list[float]") || out.contains("    history: list["), "{}", out);
        assert!(out.contains("def kw_user(a: int, b: str | None = None) -> int:"), "{}", out);
        assert!(out.contains("items: list[int]"), "{}", out);
        assert!(rep.fstrings >= 1, "{}", out);
        crate::parser::parse(&out, "t.fpy").unwrap();
    }

    #[test]
    fn annotation_and_trace_types() {
        use CT::*;
        let e = |s: &str| crate::parser::parse_expr_str(s, "t", crate::diag::Span::default()).unwrap();
        assert_eq!(ann_to_ct(&e("Optional[int]")), Opt(Box::new(Int)));
        assert_eq!(ann_to_ct(&e("int | None")), Opt(Box::new(Int)));
        assert_eq!(ann_to_ct(&e("bytes")), Bytes);
        assert_eq!(ann_to_ct(&e("None")), NoneT);
        assert_eq!(ann_to_ct(&e("Weird[int]")), Any);
        assert_eq!(parse_trace_type("tuple[()]"), Tuple(vec![]));
        assert_eq!(Tuple(vec![]).render(), "tuple[()]");
        assert_eq!(unify(&NoneT, &Opt(Box::new(Str))), Opt(Box::new(Str)));
        assert_eq!(unify(&Iter(Box::new(Int)), &Iter(Box::new(Bool))), Iter(Box::new(Int)));
        assert_eq!(Str.elem(), Str);
        assert_eq!(import_insert_pos("def (:\n"), 0);
    }

    #[test]
    fn infers_methods_and_literals() {
        let src = r#"import math
import os


DEFAULTS = (True, b"raw", -1)


def methods(s, xs, st):
    raw = s.encode()
    where = xs.index(3) + xs.count(3)
    one = st.pop()
    names = os.listdir(".")
    root = math.sqrt(4.0)
    flag, data, neg = DEFAULTS
    if flag:
        mode = "a"
    else:
        mode = "b"
    lookup: dict[str, int] = {}
    seen = []
    for ch in s:
        seen.append(ch)
    return raw, where, one, names, root, data, neg, mode, lookup, seen


def opt(x=None):
    return x


methods("text", [1, 2, 3], {1, 2})
opt()
"#;
        let (out, _) = conv_report(src);
        assert!(out.contains("    s: str,\n    xs: list[int],\n    st: set[int],\n"), "{}", out);
        assert!(out.contains("seen: list[str] = []"), "{}", out);
        crate::parser::parse(&out, "t.fpy").unwrap();
        use CT::*;
        assert_eq!(Bytes.elem(), Int);
        assert_eq!(parse_trace_type("Iterator[int]"), Iter(Box::new(Int)));
    }

    #[test]
    fn infers_builtins_and_varargs() {
        let src = r#"def stats(*args, **kwargs):
    pairs = [(1, "a"), (2, "b")]
    for n, label in pairs:
        print(n, label)
    empty = dict()
    lo = min(3, 4.5)
    hi = max([1, 2])
    tot = sum([1.5, 2.5])
    backup = list(args).copy()
    s = {1, 2}
    both = s.union({3})
    flag = not args
    neg = -1
    print(empty, lo, hi, tot, backup, both, flag, neg, kwargs)
    return hi


def defaults(on=True, raw=b"x", n=-2, name=None):
    return on, raw, n, name


stats(1, 2, k=3)
defaults()
"#;
        let (out, rep) = conv_report(src);
        assert!(out.contains("*args: Any") && out.contains("**kwargs: Any"), "{}", out);
        assert!(out.contains("on: bool = True") && out.contains("raw: bytes = b\"x\"") && out.contains("n: int = -2"), "{}", out);
        assert!(rep.notes.iter().any(|n| n.1.contains("*args of 'stats' typed as Any")), "{:?}", rep.notes);
        crate::parser::parse(&out, "t.fpy").unwrap();
    }
}
