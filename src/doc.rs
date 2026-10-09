//! `fpython doc`: API documentation generated from module, class and function docstrings.
//!
//! Signatures are printed exactly as `fpython fmt` would write them. Docstrings may use plain
//! paragraphs, bullet lists, fenced or indented code, `>>>` examples, inline `code`, and
//! Google-style (`Args:`) or NumPy-style (`Parameters` + `----------`) sections.

use crate::ast::*;
use crate::compiler::looks_like_type_expr;
use crate::fmt::{class_header, def_header, expr_source};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Html,
    Markdown,
}

#[derive(Debug, Clone)]
pub struct DocOptions {
    pub format: Format,
    /// include `_private` names
    pub private: bool,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FuncDoc {
    pub name: String,
    /// "function", "async function", "method", "staticmethod", "classmethod" or "property"
    pub kind: &'static str,
    pub header: String,
    pub doc: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDoc {
    pub name: String,
    pub ann: Option<String>,
    pub value: Option<String>,
    pub doc: Option<String>,
    /// `type X = ...` / `X: TypeAlias = ...`
    pub is_alias: bool,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassDoc {
    pub name: String,
    pub header: String,
    pub doc: Option<String>,
    pub fields: Vec<VarDoc>,
    pub methods: Vec<FuncDoc>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModuleDoc {
    pub name: String,
    pub path: String,
    pub doc: Option<String>,
    pub vars: Vec<VarDoc>,
    pub functions: Vec<FuncDoc>,
    pub classes: Vec<ClassDoc>,
}

// ---------------------------------------------------------------------- extraction

fn docstring(body: &[Stmt]) -> Option<String> {
    match body.first().map(|s| &s.kind) {
        Some(StmtKind::Expr(Expr { kind: ExprKind::Str { value, .. }, .. })) => Some(cleandoc(value)),
        _ => None,
    }
}

/// `inspect.cleandoc`: strip blank edge lines and the common indentation of lines after the first.
pub fn cleandoc(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let indent = lines.iter().skip(1).filter(|l| !l.trim().is_empty()).map(|l| l.len() - l.trim_start().len()).min().unwrap_or(0);
    let mut out: Vec<String> = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if i == 0 {
            out.push(l.trim().to_string());
        } else {
            out.push(if l.len() >= indent { l[indent..].trim_end().to_string() } else { l.trim().to_string() });
        }
    }
    while out.first().map(|l| l.is_empty()).unwrap_or(false) {
        out.remove(0);
    }
    while out.last().map(|l| l.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out.join("\n")
}

/// Names listed in a module-level `__all__`, if any.
fn dunder_all(body: &[Stmt]) -> Option<BTreeSet<String>> {
    for st in body {
        let value = match &st.kind {
            StmtKind::Assign { targets, value } if matches!(&targets[..], [Expr { kind: ExprKind::Name(n), .. }] if n == "__all__") => value,
            StmtKind::AnnAssign { target, value: Some(value), .. } if matches!(&target.kind, ExprKind::Name(n) if n == "__all__") => value,
            _ => continue,
        };
        if let ExprKind::List(items) | ExprKind::Tuple { elts: items, .. } = &value.kind {
            return Some(items.iter().filter_map(|i| if let ExprKind::Str { value, .. } = &i.kind { Some(value.clone()) } else { None }).collect());
        }
    }
    None
}

fn shorten(s: String) -> String {
    if s.chars().count() > 80 {
        format!("{}...", s.chars().take(77).collect::<String>())
    } else {
        s
    }
}

/// A string expression right after an assignment documents it (Sphinx attribute docstring).
fn attribute_doc(body: &[Stmt], i: usize) -> Option<String> {
    match body.get(i + 1).map(|s| &s.kind) {
        Some(StmtKind::Expr(Expr { kind: ExprKind::Str { value, .. }, .. })) => Some(cleandoc(value)),
        _ => None,
    }
}

fn var_docs(body: &[Stmt], src: &str, visible: &dyn Fn(&str) -> bool) -> Vec<VarDoc> {
    let mut out = Vec::new();
    for (i, st) in body.iter().enumerate() {
        let (name, ann, value, is_alias) = match &st.kind {
            StmtKind::AnnAssign { target: Expr { kind: ExprKind::Name(n), .. }, ann, value } => {
                let alias = matches!(&ann.kind, ExprKind::Name(a) if a == "TypeAlias");
                (n.clone(), if alias { None } else { Some(expr_source(ann, src)) }, value.as_ref().map(|v| shorten(expr_source(v, src))), alias)
            }
            StmtKind::Assign { targets, value } if targets.len() == 1 => match &targets[0].kind {
                // `X = list[int]` is an implicit type alias
                ExprKind::Name(n) if n != "__all__" => (n.clone(), None, Some(shorten(expr_source(value, src))), looks_like_type_expr(value) && matches!(value.kind, ExprKind::Subscript { .. } | ExprKind::BinOp { .. })),
                _ => continue,
            },
            StmtKind::TypeAlias { name, value, .. } => (name.clone(), None, Some(expr_source(value, src)), true),
            _ => continue,
        };
        if visible(&name) {
            out.push(VarDoc { name, ann, value, doc: attribute_doc(body, i), is_alias, line: st.span.line });
        }
    }
    out
}

fn func_doc(f: &FuncDef, src: &str, line: usize, in_class: bool) -> FuncDoc {
    let deco = |n: &str| f.decorators.iter().any(|d| matches!(&d.kind, ExprKind::Name(x) if x == n));
    let kind = if !in_class {
        if f.is_async {
            "async function"
        } else {
            "function"
        }
    } else if deco("property") {
        "property"
    } else if deco("staticmethod") {
        "staticmethod"
    } else if deco("classmethod") {
        "classmethod"
    } else {
        "method"
    };
    FuncDoc { name: f.name.clone(), kind, header: def_header(f, src, 88), doc: docstring(&f.body), line: f.header_lines.0.max(line) }
}

fn is_public(name: &str) -> bool {
    !name.starts_with('_') || (name.starts_with("__") && name.ends_with("__"))
}

pub fn extract(src: &str, path: &str, name: &str, private: bool) -> Result<ModuleDoc, crate::diag::Diag> {
    let parsed = crate::parser::parse(src, path)?;
    let body = &parsed.module.body;
    let all = dunder_all(body);
    let visible = |n: &str| match &all {
        Some(names) => names.contains(n) || (private && n.starts_with('_')),
        None => private || !n.starts_with('_'),
    };
    let mut m = ModuleDoc { name: name.to_string(), path: path.to_string(), doc: docstring(body), vars: var_docs(body, src, &visible), functions: Vec::new(), classes: Vec::new() };
    for st in body {
        match &st.kind {
            StmtKind::FuncDef(f) if visible(&f.name) => m.functions.push(func_doc(f, src, st.span.line, false)),
            StmtKind::ClassDef(c) if visible(&c.name) => {
                let member_visible = |n: &str| private || is_public(n);
                let mut methods = Vec::new();
                for s in &c.body {
                    if let StmtKind::FuncDef(f) = &s.kind {
                        // dunders other than __init__ are listed only when documented
                        let dunder = f.name.starts_with("__") && f.name != "__init__";
                        if member_visible(&f.name) && (!dunder || docstring(&f.body).is_some()) {
                            methods.push(func_doc(f, src, s.span.line, true));
                        }
                    }
                }
                m.classes.push(ClassDoc {
                    name: c.name.clone(),
                    header: class_header(c, src),
                    doc: docstring(&c.body),
                    fields: var_docs(&c.body, src, &|n| member_visible(n) && !n.starts_with("__")),
                    methods,
                    line: st.span.line,
                });
            }
            _ => {}
        }
    }
    Ok(m)
}

/// Dotted module name of `file` relative to `root` (`pkg/__init__.fpy` is `pkg`).
pub fn module_name(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let mut parts: Vec<String> = rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    if parts.last().map(|p| p == "__init__").unwrap_or(false) && parts.len() > 1 {
        parts.pop();
    }
    parts.join(".")
}

/// Public items without a docstring, as `path:line: message` (for `fpython doc --check`).
pub fn missing_docs(m: &ModuleDoc) -> Vec<String> {
    let mut out = Vec::new();
    if m.doc.is_none() {
        out.push(format!("{}:1: module '{}' has no docstring", m.path, m.name));
    }
    for f in &m.functions {
        if f.doc.is_none() {
            out.push(format!("{}:{}: function '{}' has no docstring", m.path, f.line, f.name));
        }
    }
    for c in &m.classes {
        if c.doc.is_none() {
            out.push(format!("{}:{}: class '{}' has no docstring", m.path, c.line, c.name));
        }
        for f in &c.methods {
            if f.doc.is_none() && f.name != "__init__" {
                out.push(format!("{}:{}: method '{}.{}' has no docstring", m.path, f.line, c.name, f.name));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------- docstring markup

#[derive(Debug, PartialEq)]
enum Block {
    Para(String),
    Code(String),
    List(Vec<String>),
    Section(String),
    /// `name (type): description` entries of an Args/Returns/Raises section
    Entries(Vec<(String, String)>),
}

const SECTIONS: &[&str] = &[
    "Args", "Arguments", "Parameters", "Params", "Keyword Args", "Keyword Arguments", "Other Parameters", "Returns", "Return", "Yields", "Yield",
    "Raises", "Attributes", "Example", "Examples", "Note", "Notes", "See Also", "Warning", "Warnings", "References", "Todo",
];

fn section_name(line: &str, next: Option<&str>) -> Option<String> {
    let t = line.trim();
    // Google style: "Args:"
    if let Some(n) = t.strip_suffix(':') {
        if SECTIONS.contains(&n) {
            return Some(n.to_string());
        }
    }
    // NumPy style: "Parameters" underlined with dashes
    if SECTIONS.contains(&t) && next.map(|n| !n.trim().is_empty() && n.trim().chars().all(|c| c == '-')).unwrap_or(false) {
        return Some(t.to_string());
    }
    None
}

fn indent_of(l: &str) -> usize {
    l.len() - l.trim_start().len()
}

/// Split a cleaned docstring into blocks.
fn blocks(doc: &str) -> Vec<Block> {
    let lines: Vec<&str> = doc.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let l = lines[i];
        let t = l.trim();
        if t.is_empty() {
            i += 1;
            continue;
        }
        if t.starts_with("```") {
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1;
            out.push(Block::Code(code.join("\n")));
            continue;
        }
        if t.starts_with(">>>") {
            let mut code = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                code.push(lines[i].trim());
                i += 1;
            }
            out.push(Block::Code(code.join("\n")));
            continue;
        }
        if let Some(name) = section_name(l, lines.get(i + 1).copied()) {
            let numpy = !t.ends_with(':');
            i += if numpy { 2 } else { 1 };
            // the section body: following lines until a blank line followed by a dedent, or the next section
            let base = indent_of(l);
            let mut body: Vec<&str> = Vec::new();
            while i < lines.len() {
                let x = lines[i];
                if section_name(x, lines.get(i + 1).copied()).is_some() && indent_of(x) <= base {
                    break;
                }
                if !x.trim().is_empty() && indent_of(x) <= base && !numpy {
                    break;
                }
                body.push(x);
                i += 1;
            }
            while body.last().map(|x| x.trim().is_empty()).unwrap_or(false) {
                body.pop();
            }
            out.push(Block::Section(name.clone()));
            if matches!(name.as_str(), "Example" | "Examples") {
                let ind = body.iter().filter(|x| !x.trim().is_empty()).map(|x| indent_of(x)).min().unwrap_or(0);
                out.push(Block::Code(body.iter().map(|x| if x.len() >= ind { &x[ind..] } else { x.trim() }).collect::<Vec<_>>().join("\n")));
                continue;
            }
            // entries: lines at the minimum indent start an entry, deeper lines continue it
            let ind = body.iter().filter(|x| !x.trim().is_empty()).map(|x| indent_of(x)).min().unwrap_or(0);
            let mut entries: Vec<(String, String)> = Vec::new();
            let mut plain: Vec<String> = Vec::new();
            for x in &body {
                if x.trim().is_empty() {
                    continue;
                }
                if indent_of(x) == ind {
                    let xt = x.trim();
                    let split = if numpy { xt.split_once(" : ").map(|(a, b)| (a.to_string(), format!("({})", b.trim()))) } else { xt.split_once(": ").map(|(a, b)| (a.to_string(), b.to_string())) };
                    match split {
                        Some((k, v)) => entries.push((k, v)),
                        None if numpy && !xt.contains(' ') => entries.push((xt.to_string(), String::new())),
                        None => plain.push(xt.to_string()),
                    }
                } else if let Some(e) = entries.last_mut() {
                    if !e.1.is_empty() {
                        e.1.push(' ');
                    }
                    e.1.push_str(x.trim());
                } else {
                    plain.push(x.trim().to_string());
                }
            }
            if !plain.is_empty() {
                out.push(Block::Para(plain.join(" ")));
            }
            if !entries.is_empty() {
                out.push(Block::Entries(entries));
            }
            continue;
        }
        if t.starts_with("- ") || t.starts_with("* ") {
            let mut items: Vec<String> = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                let x = lines[i].trim();
                if x.starts_with("- ") || x.starts_with("* ") {
                    items.push(x[2..].to_string());
                } else if let Some(last) = items.last_mut() {
                    last.push(' ');
                    last.push_str(x);
                }
                i += 1;
            }
            out.push(Block::List(items));
            continue;
        }
        if indent_of(l) >= 4 {
            let mut code = Vec::new();
            while i < lines.len() && (lines[i].trim().is_empty() || indent_of(lines[i]) >= 4) {
                code.push(if lines[i].len() >= 4 { &lines[i][4..] } else { "" });
                i += 1;
            }
            while code.last().map(|c| c.trim().is_empty()).unwrap_or(false) {
                code.pop();
            }
            out.push(Block::Code(code.join("\n")));
            continue;
        }
        let mut para = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && section_name(lines[i], lines.get(i + 1).copied()).is_none() && !lines[i].trim().starts_with("```") && !lines[i].trim().starts_with(">>>") {
            para.push(lines[i].trim());
            i += 1;
        }
        out.push(Block::Para(para.join(" ")));
    }
    out
}

pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Inline markup: `code` and **bold** (on escaped text).
fn inline(s: &str) -> String {
    let e = escape(s);
    let mut out = String::new();
    let mut rest = e.as_str();
    while let Some(i) = rest.find('`') {
        match rest[i + 1..].find('`') {
            Some(j) => {
                out.push_str(&bold(&rest[..i]));
                out.push_str(&format!("<code>{}</code>", &rest[i + 1..i + 1 + j]));
                rest = &rest[i + 2 + j..];
            }
            None => break,
        }
    }
    out.push_str(&bold(rest));
    out
}

fn bold(s: &str) -> String {
    let parts: Vec<&str> = s.split("**").collect();
    if parts.len() < 3 {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 && i < parts.len() - (parts.len() % 2 == 0) as usize {
            out.push_str(if i % 2 == 1 { "<strong>" } else { "</strong>" });
        } else if i > 0 {
            out.push_str("**");
        }
        out.push_str(p);
    }
    out
}

pub fn doc_to_html(doc: &str) -> String {
    let mut out = String::new();
    for b in blocks(doc) {
        match b {
            Block::Para(p) => out.push_str(&format!("<p>{}</p>\n", inline(&p))),
            Block::Code(c) => out.push_str(&format!("<pre><code>{}</code></pre>\n", escape(&c))),
            Block::List(items) => {
                out.push_str("<ul>\n");
                for it in items {
                    out.push_str(&format!("<li>{}</li>\n", inline(&it)));
                }
                out.push_str("</ul>\n");
            }
            Block::Section(s) => out.push_str(&format!("<h4>{}</h4>\n", escape(&s))),
            Block::Entries(es) => {
                out.push_str("<dl>\n");
                for (k, v) in es {
                    out.push_str(&format!("<dt><code>{}</code></dt><dd>{}</dd>\n", escape(&k), inline(&v)));
                }
                out.push_str("</dl>\n");
            }
        }
    }
    out
}

pub fn doc_to_markdown(doc: &str) -> String {
    let mut out = String::new();
    for b in blocks(doc) {
        match b {
            Block::Para(p) => out.push_str(&format!("{}\n\n", p)),
            Block::Code(c) => out.push_str(&format!("```python\n{}\n```\n\n", c)),
            Block::List(items) => {
                for it in items {
                    out.push_str(&format!("- {}\n", it));
                }
                out.push('\n');
            }
            Block::Section(s) => out.push_str(&format!("**{}:**\n\n", s)),
            Block::Entries(es) => {
                for (k, v) in es {
                    out.push_str(&format!("- `{}`: {}\n", k, v));
                }
                out.push('\n');
            }
        }
    }
    out
}

/// First sentence/line of a docstring, for index tables.
pub fn summary(doc: &Option<String>) -> String {
    doc.as_deref().and_then(|d| d.lines().next()).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------- Markdown output

pub fn render_markdown(m: &ModuleDoc) -> String {
    let mut s = format!("# `{}`\n\n", m.name);
    if let Some(d) = &m.doc {
        s.push_str(&doc_to_markdown(d));
    }
    let item_doc = |d: &Option<String>| d.as_deref().map(doc_to_markdown).unwrap_or_default();
    let aliases: Vec<&VarDoc> = m.vars.iter().filter(|v| v.is_alias).collect();
    let consts: Vec<&VarDoc> = m.vars.iter().filter(|v| !v.is_alias).collect();
    let var_line = |v: &VarDoc| {
        let mut l = format!("`{}", v.name);
        if let Some(a) = &v.ann {
            l.push_str(&format!(": {}", a));
        }
        if let Some(x) = &v.value {
            l.push_str(&format!(" = {}", x));
        }
        l.push('`');
        l
    };
    if !aliases.is_empty() {
        s.push_str("## Type aliases\n\n");
        for v in aliases {
            s.push_str(&format!("- `{} = {}`", v.name, v.value.clone().unwrap_or_default()));
            s.push_str(&v.doc.as_ref().map(|d| format!(": {}", summary(&Some(d.clone())))).unwrap_or_default());
            s.push('\n');
        }
        s.push('\n');
    }
    if !consts.is_empty() {
        s.push_str("## Variables\n\n");
        for v in consts {
            s.push_str(&format!("- {}", var_line(v)));
            s.push_str(&v.doc.as_ref().map(|d| format!(": {}", summary(&Some(d.clone())))).unwrap_or_default());
            s.push('\n');
        }
        s.push('\n');
    }
    if !m.classes.is_empty() {
        s.push_str("## Classes\n\n");
        for c in &m.classes {
            s.push_str(&format!("### `{}`\n\n```python\n{}\n```\n\n{}", c.name, c.header, item_doc(&c.doc)));
            if !c.fields.is_empty() {
                s.push_str("**Fields:**\n\n");
                for f in &c.fields {
                    s.push_str(&format!("- {}", var_line(f)));
                    s.push_str(&f.doc.as_ref().map(|d| format!(": {}", summary(&Some(d.clone())))).unwrap_or_default());
                    s.push('\n');
                }
                s.push('\n');
            }
            for f in &c.methods {
                s.push_str(&format!("#### `{}.{}` ({})\n\n```python\n{}\n```\n\n{}", c.name, f.name, f.kind, f.header, item_doc(&f.doc)));
            }
        }
    }
    if !m.functions.is_empty() {
        s.push_str("## Functions\n\n");
        for f in &m.functions {
            s.push_str(&format!("### `{}`\n\n```python\n{}\n```\n\n{}", f.name, f.header, item_doc(&f.doc)));
        }
    }
    while s.ends_with("\n\n") {
        s.pop();
    }
    s
}

// ---------------------------------------------------------------------- HTML output

/// Where each documented class lives: name -> (page, anchor). Used to link signatures.
pub type LinkMap = BTreeMap<String, (String, String)>;

fn page_name(module: &str) -> String {
    format!("{}.html", module)
}

pub fn link_map(mods: &[ModuleDoc]) -> LinkMap {
    let mut map = LinkMap::new();
    for m in mods {
        for c in &m.classes {
            map.entry(c.name.clone()).or_insert((page_name(&m.name), c.name.clone()));
        }
    }
    map
}

/// Escape a code snippet and link identifiers that name documented classes.
fn code_with_links(code: &str, links: &LinkMap, current_page: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        if word.is_empty() {
            return;
        }
        match links.get(word.as_str()) {
            Some((page, anchor)) => {
                let href = if page == current_page { format!("#{}", anchor) } else { format!("{}#{}", page, anchor) };
                out.push_str(&format!("<a href=\"{}\">{}</a>", escape(&href), escape(word)));
            }
            None => out.push_str(&escape(word)),
        }
        word.clear();
    };
    let mut in_str: Option<char> = None;
    for ch in code.chars() {
        if let Some(q) = in_str {
            out.push_str(&escape(&ch.to_string()));
            if ch == q {
                in_str = None;
            }
            continue;
        }
        if ch.is_alphanumeric() || ch == '_' {
            word.push(ch);
        } else {
            flush(&mut word, &mut out);
            if ch == '"' || ch == '\'' {
                in_str = Some(ch);
            }
            out.push_str(&escape(&ch.to_string()));
        }
    }
    flush(&mut word, &mut out);
    out
}

const CSS: &str = r#":root{--bg:#fff;--fg:#1f2328;--muted:#59636e;--line:#d1d9e0;--code:#f6f8fa;--accent:#0969da;--side:#f6f8fa}
@media (prefers-color-scheme:dark){:root{--bg:#0d1117;--fg:#e6edf3;--muted:#9198a1;--line:#30363d;--code:#161b22;--accent:#4493f8;--side:#010409}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:16px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Helvetica,Arial,sans-serif}
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}
.wrap{display:flex;min-height:100vh}nav{width:270px;flex:none;background:var(--side);border-right:1px solid var(--line);padding:20px 16px;position:sticky;top:0;height:100vh;overflow:auto;font-size:14px}
nav h2{font-size:15px;margin:0 0 12px}nav ul{list-style:none;padding:0;margin:0 0 16px}nav li{margin:2px 0}nav .cur{font-weight:600}nav .sub{padding-left:12px;color:var(--muted)}
main{flex:1;min-width:0;max-width:980px;padding:28px 40px}h1{font-size:30px;margin:0 0 12px}h2{border-bottom:1px solid var(--line);padding-bottom:6px;margin-top:40px}
h3{margin:28px 0 8px;font-size:19px}h4{margin:14px 0 6px;font-size:15px;color:var(--muted)}
pre,code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:13.5px}code{background:var(--code);padding:1px 5px;border-radius:5px}
pre{background:var(--code);padding:12px 14px;border-radius:8px;overflow:auto;border:1px solid var(--line)}pre code{background:none;padding:0}
.sig{border-left:3px solid var(--accent)}.kind{font-size:12px;color:var(--muted);border:1px solid var(--line);border-radius:10px;padding:1px 8px;margin-left:8px;vertical-align:middle;font-weight:400}
.item{margin-left:4px}.member{margin-left:22px}dl{margin:6px 0 12px}dt{margin-top:6px}dd{margin:2px 0 0 22px}
table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid var(--line);padding:8px;text-align:left;vertical-align:top}
#q{width:100%;padding:8px 10px;border:1px solid var(--line);border-radius:6px;background:var(--bg);color:var(--fg);margin-bottom:14px;font-size:14px}
.muted{color:var(--muted)}.foot{margin-top:60px;color:var(--muted);font-size:13px}
@media (max-width:760px){.wrap{display:block}nav{width:auto;height:auto;position:static}main{padding:20px 16px}}"#;

fn page(title: &str, nav: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<style>{}</style>\n</head>\n<body>\n<div class=\"wrap\">\n<nav>{}</nav>\n<main>\n{}<p class=\"foot\">Generated by <code>fpython doc</code>.</p>\n</main>\n</div>\n</body>\n</html>\n",
        escape(title),
        CSS,
        nav,
        body
    )
}

fn nav(title: &str, mods: &[ModuleDoc], current: Option<&ModuleDoc>) -> String {
    let mut s = format!("<h2><a href=\"index.html\">{}</a></h2>\n<ul>\n", escape(title));
    for m in mods {
        let cur = current.map(|c| c.name == m.name).unwrap_or(false);
        s.push_str(&format!("<li{}><a href=\"{}\">{}</a></li>\n", if cur { " class=\"cur\"" } else { "" }, escape(&page_name(&m.name)), escape(&m.name)));
        if cur {
            for c in &m.classes {
                s.push_str(&format!("<li class=\"sub\"><a href=\"#{0}\">{0}</a></li>\n", escape(&c.name)));
            }
            for f in &m.functions {
                s.push_str(&format!("<li class=\"sub\"><a href=\"#{0}\">{0}()</a></li>\n", escape(&f.name)));
            }
        }
    }
    s.push_str("</ul>\n");
    s
}

fn html_doc(d: &Option<String>) -> String {
    d.as_deref().map(doc_to_html).unwrap_or_default()
}

fn html_vars(vars: &[&VarDoc], links: &LinkMap, pg: &str) -> String {
    let mut s = String::from("<table>\n");
    for v in vars {
        let mut code = v.name.clone();
        if let Some(a) = &v.ann {
            code.push_str(&format!(": {}", a));
        }
        if let Some(x) = &v.value {
            code.push_str(&format!(" = {}", x));
        }
        s.push_str(&format!("<tr id=\"{}\"><td><code>{}</code></td><td>{}</td></tr>\n", escape(&v.name), code_with_links(&code, links, pg), html_doc(&v.doc)));
    }
    s.push_str("</table>\n");
    s
}

pub fn render_html_module(m: &ModuleDoc, mods: &[ModuleDoc], links: &LinkMap, title: &str) -> String {
    let pg = page_name(&m.name);
    let mut b = format!("<h1><code>{}</code></h1>\n<p class=\"muted\">{}</p>\n{}", escape(&m.name), escape(&m.path), html_doc(&m.doc));
    let aliases: Vec<&VarDoc> = m.vars.iter().filter(|v| v.is_alias).collect();
    let consts: Vec<&VarDoc> = m.vars.iter().filter(|v| !v.is_alias).collect();
    if !aliases.is_empty() {
        b.push_str("<h2>Type aliases</h2>\n");
        b.push_str(&html_vars(&aliases, links, &pg));
    }
    if !consts.is_empty() {
        b.push_str("<h2>Variables</h2>\n");
        b.push_str(&html_vars(&consts, links, &pg));
    }
    if !m.classes.is_empty() {
        b.push_str("<h2>Classes</h2>\n");
        for c in &m.classes {
            b.push_str(&format!(
                "<div class=\"item\"><h3 id=\"{0}\">{0}<span class=\"kind\">class</span></h3>\n<pre class=\"sig\"><code>{1}</code></pre>\n{2}",
                escape(&c.name),
                // the class's own name is not linked; its bases are
                format!("class {}{}", escape(&c.name), code_with_links(c.header.get(6 + c.name.len()..).unwrap_or(""), links, &pg)),
                html_doc(&c.doc)
            ));
            if !c.fields.is_empty() {
                b.push_str("<h4>Fields</h4>\n");
                b.push_str(&html_vars(&c.fields.iter().collect::<Vec<_>>(), links, &pg));
            }
            for f in &c.methods {
                b.push_str(&format!(
                    "<div class=\"member\"><h3 id=\"{0}.{1}\">{1}<span class=\"kind\">{2}</span></h3>\n<pre class=\"sig\"><code>{3}</code></pre>\n{4}</div>\n",
                    escape(&c.name),
                    escape(&f.name),
                    f.kind,
                    code_with_links(&f.header, links, &pg),
                    html_doc(&f.doc)
                ));
            }
            b.push_str("</div>\n");
        }
    }
    if !m.functions.is_empty() {
        b.push_str("<h2>Functions</h2>\n");
        for f in &m.functions {
            b.push_str(&format!(
                "<div class=\"item\"><h3 id=\"{0}\">{0}<span class=\"kind\">{1}</span></h3>\n<pre class=\"sig\"><code>{2}</code></pre>\n{3}</div>\n",
                escape(&f.name),
                f.kind,
                code_with_links(&f.header, links, &pg),
                html_doc(&f.doc)
            ));
        }
    }
    page(&format!("{} — {}", m.name, title), &nav(title, mods, Some(m)), &b)
}

/// JSON string literal (for the search index).
fn json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '<' => o.push_str("\\u003c"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

pub fn render_html_index(mods: &[ModuleDoc], title: &str) -> String {
    let mut b = format!("<h1>{}</h1>\n<input id=\"q\" type=\"search\" placeholder=\"Search modules, classes and functions\" autocomplete=\"off\">\n<div id=\"results\"></div>\n<table id=\"mods\">\n<tr><th>Module</th><th>Summary</th></tr>\n", escape(title));
    let mut entries = Vec::new();
    for m in mods {
        let pg = page_name(&m.name);
        b.push_str(&format!("<tr><td><a href=\"{}\"><code>{}</code></a></td><td>{}</td></tr>\n", escape(&pg), escape(&m.name), inline(&summary(&m.doc))));
        entries.push(format!("[{},{},{}]", json_str(&m.name), json_str(&pg), json_str(&summary(&m.doc))));
        for c in &m.classes {
            entries.push(format!("[{},{},{}]", json_str(&format!("{}.{}", m.name, c.name)), json_str(&format!("{}#{}", pg, c.name)), json_str(&summary(&c.doc))));
            for f in &c.methods {
                entries.push(format!("[{},{},{}]", json_str(&format!("{}.{}.{}", m.name, c.name, f.name)), json_str(&format!("{}#{}.{}", pg, c.name, f.name)), json_str(&summary(&f.doc))));
            }
        }
        for f in &m.functions {
            entries.push(format!("[{},{},{}]", json_str(&format!("{}.{}", m.name, f.name)), json_str(&format!("{}#{}", pg, f.name)), json_str(&summary(&f.doc))));
        }
    }
    b.push_str("</table>\n");
    b.push_str(&format!(
        "<script>\nconst IDX=[{}];\nconst q=document.getElementById('q'),res=document.getElementById('results'),mods=document.getElementById('mods');\nconst esc=s=>s.replace(/[&<>\"]/g,c=>({{'&':'&amp;','<':'&lt;','>':'&gt;','\"':'&quot;'}})[c]);\nq.addEventListener('input',()=>{{const t=q.value.trim().toLowerCase();if(!t){{res.innerHTML='';mods.style.display='';return;}}mods.style.display='none';const hits=IDX.filter(e=>e[0].toLowerCase().includes(t)).slice(0,100);res.innerHTML='<table>'+hits.map(e=>'<tr><td><a href=\"'+esc(e[1])+'\"><code>'+esc(e[0])+'</code></a></td><td>'+esc(e[2])+'</td></tr>').join('')+'</table>'+(hits.length?'':'<p class=\"muted\">No matches.</p>');}});\n</script>\n",
        entries.join(",")
    ));
    page(title, &nav(title, mods, None), &b)
}

// ---------------------------------------------------------------------- driver

pub struct DocReport {
    pub written: Vec<PathBuf>,
    pub errors: Vec<String>,
}

/// Document `files` (module names relative to `root`) into `out_dir`.
pub fn generate(files: &[(PathBuf, PathBuf)], out_dir: &Path, opts: &DocOptions) -> DocReport {
    let mut mods = Vec::new();
    let mut errors = Vec::new();
    for (root, f) in files {
        let name = module_name(root, f);
        match std::fs::read_to_string(f) {
            Ok(src) => match extract(&src, &f.to_string_lossy(), &name, opts.private) {
                Ok(m) => mods.push(m),
                Err(d) => errors.push(d.render(Some(&src))),
            },
            Err(e) => errors.push(format!("{}: {}\n", f.display(), e)),
        }
    }
    mods.sort_by(|a, b| a.name.cmp(&b.name));
    let mut written = Vec::new();
    if let Err(e) = std::fs::create_dir_all(out_dir) {
        errors.push(format!("{}: {}\n", out_dir.display(), e));
        return DocReport { written, errors };
    }
    let mut write = |p: PathBuf, content: String, errors: &mut Vec<String>| match std::fs::write(&p, content) {
        Ok(()) => written.push(p),
        Err(e) => errors.push(format!("{}: {}\n", p.display(), e)),
    };
    match opts.format {
        Format::Markdown => {
            let mut index = format!("# {}\n\n| Module | Summary |\n|---|---|\n", opts.title);
            for m in &mods {
                index.push_str(&format!("| [`{0}`]({0}.md) | {1} |\n", m.name, summary(&m.doc).replace('|', "\\|")));
                write(out_dir.join(format!("{}.md", m.name)), render_markdown(m) + "\n", &mut errors);
            }
            write(out_dir.join("index.md"), index, &mut errors);
        }
        Format::Html => {
            let links = link_map(&mods);
            for m in &mods {
                write(out_dir.join(page_name(&m.name)), render_html_module(m, &mods, &links, &opts.title), &mut errors);
            }
            write(out_dir.join("index.html"), render_html_index(&mods, &opts.title), &mut errors);
        }
    }
    DocReport { written, errors }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#""""Geometry helpers.

Use `area()` for **anything** with a size.
"""
from dataclasses import dataclass

__all__ = ["Point", "area", "ORIGIN", "Coord", "Shape"]

type Coord = tuple[float, float]
Shape = list[Coord]

ORIGIN: Coord = (0.0, 0.0)
"""The origin."""

_cache: dict[str, int] = {}


@dataclass
class Point:
    """A point in the plane.

    Attributes:
        x: horizontal position
        y: vertical position
    """

    x: float
    y: float = 0.0
    """Defaults to the x axis."""

    def norm(self) -> float:
        """Distance to the origin.

        >>> Point(3, 4).norm()
        5.0
        """
        return (self.x**2 + self.y**2) ** 0.5

    @property
    def flipped(self) -> "Point":
        return Point(self.y, self.x)

    def _secret(self) -> None:
        pass

    def __eq__(self, other: object) -> bool:
        return True


def area(points: list[Point], scale: float = 1.0) -> float:
    """Polygon area (shoelace formula).

    Args:
        points: the vertices, in order
        scale: multiplied into the
            result

    Returns:
        The area.

    Example:
        area([Point(0, 0), Point(1, 0), Point(0, 1)])
    """
    return 0.0


def hidden() -> None:
    pass
"#;

    #[test]
    fn extracts_public_api() {
        let m = extract(SRC, "geo.fpy", "geo", false).unwrap();
        assert_eq!(summary(&m.doc), "Geometry helpers.");
        let names: Vec<&str> = m.vars.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["Coord", "Shape", "ORIGIN"]);
        assert!(m.vars[0].is_alias && m.vars[0].value.as_deref() == Some("tuple[float, float]"));
        assert!(m.vars[1].is_alias && !m.vars[2].is_alias);
        assert_eq!(m.vars[2].doc.as_deref(), Some("The origin."));
        assert_eq!(m.functions.len(), 1, "__all__ hides `hidden`");
        assert_eq!(m.functions[0].header, "def area(points: list[Point], scale: float = 1.0) -> float");
        let c = &m.classes[0];
        assert_eq!(c.header, "class Point");
        assert_eq!(c.fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["x", "y"]);
        assert_eq!(c.fields[1].doc.as_deref(), Some("Defaults to the x axis."));
        let methods: Vec<(&str, &str)> = c.methods.iter().map(|f| (f.name.as_str(), f.kind)).collect();
        assert_eq!(methods, [("norm", "method"), ("flipped", "property")], "private and undocumented dunders are skipped");
        let missing = missing_docs(&m);
        assert_eq!(missing, vec!["geo.fpy:40: method 'Point.flipped' has no docstring".to_string()]);
        // --private shows everything
        let p = extract(SRC, "geo.fpy", "geo", true).unwrap();
        assert!(p.vars.iter().any(|v| v.name == "_cache"));
        assert!(p.classes[0].methods.iter().any(|f| f.name == "_secret"));
    }

    #[test]
    fn without_all_underscore_names_are_private() {
        let m = extract("def f() -> None:\n    pass\n\n\ndef _g() -> None:\n    pass\n\n\nclass _H:\n    pass\n\n\nasync def co() -> None:\n    pass\n", "m.fpy", "m", false).unwrap();
        let names: Vec<(&str, &str)> = m.functions.iter().map(|f| (f.name.as_str(), f.kind)).collect();
        assert_eq!(names, [("f", "function"), ("co", "async function")]);
        assert!(m.classes.is_empty());
        assert!(missing_docs(&m).iter().any(|l| l.contains("module 'm' has no docstring")));
        assert!(extract("def (:\n", "bad.fpy", "bad", false).is_err());
    }

    #[test]
    fn docstring_blocks() {
        let d = cleandoc("Summary line.\n\n    Paragraph one\n    continues.\n\n    - first\n    - second\n      wraps\n\n    Indented code::\n\n        x = 1\n        y = 2\n\n    ```\n    fenced\n    ```\n\n    Parameters\n    ----------\n    x : int\n        The value.\n    flag\n        A flag.\n\n    Raises:\n        ValueError: when bad\n        free text line\n    ");
        let html = doc_to_html(&d);
        assert!(html.starts_with("<p>Summary line.</p>\n<p>Paragraph one continues.</p>\n<ul>\n<li>first</li>\n<li>second wraps</li>\n</ul>\n"), "{}", html);
        assert!(html.contains("<pre><code>x = 1\ny = 2</code></pre>"), "{}", html);
        assert!(html.contains("<pre><code>fenced</code></pre>"), "{}", html);
        assert!(html.contains("<h4>Parameters</h4>\n<dl>\n<dt><code>x</code></dt><dd>(int) The value.</dd>\n<dt><code>flag</code></dt><dd>A flag.</dd>"), "{}", html);
        assert!(html.contains("<h4>Raises</h4>\n<p>free text line</p>\n<dl>\n<dt><code>ValueError</code></dt><dd>when bad</dd>"), "{}", html);
        let md = doc_to_markdown(&d);
        assert!(md.contains("- first\n- second wraps\n"), "{}", md);
        assert!(md.contains("**Parameters:**\n\n- `x`: (int) The value.\n"), "{}", md);
        assert!(md.contains("```python\nx = 1\ny = 2\n```"), "{}", md);
    }

    #[test]
    fn inline_markup_and_escaping() {
        assert_eq!(inline("use `a<b` and **bold** text"), "use <code>a&lt;b</code> and <strong>bold</strong> text");
        assert_eq!(inline("unclosed `tick and **star"), "unclosed `tick and **star");
        assert_eq!(bold("a **b** c **d"), "a <strong>b</strong> c **d");
        assert_eq!(json_str("a\"b\\c\n<\u{1}"), "\"a\\\"b\\\\c\\n\\u003c\\u0001\"");
        assert_eq!(cleandoc("\n\n   \n"), "");
    }

    #[test]
    fn renders_markdown_and_html() {
        let m = extract(SRC, "geo.fpy", "geo", false).unwrap();
        let md = render_markdown(&m);
        assert!(md.starts_with("# `geo`\n\nGeometry helpers.\n\nUse `area()` for **anything** with a size.\n\n## Type aliases\n\n- `Coord = tuple[float, float]`\n"), "{}", md);
        assert!(md.contains("- `ORIGIN: Coord = (0.0, 0.0)`: The origin.\n"), "{}", md);
        assert!(md.contains("#### `Point.norm` (method)\n\n```python\ndef norm(self) -> float\n```"), "{}", md);
        assert!(md.contains("**Args:**\n\n- `points`: the vertices, in order\n- `scale`: multiplied into the result\n"), "{}", md);
        let mods = vec![m];
        let links = link_map(&mods);
        let html = render_html_module(&mods[0], &mods, &links, "API");
        assert!(html.contains("def area(points: list[<a href=\"#Point\">Point</a>], scale: float = 1.0) -&gt; float"), "signatures link classes");
        assert!(html.contains("<pre class=\"sig\"><code>class Point</code></pre>"), "{}", html);
        assert!(html.contains("<h3 id=\"Point.flipped\">flipped<span class=\"kind\">property</span></h3>"));
        assert!(html.contains("<pre><code>&gt;&gt;&gt; Point(3, 4).norm()\n5.0</code></pre>"), "doctest");
        assert!(html.contains("<li class=\"sub\"><a href=\"#area\">area()</a></li>"));
        let idx = render_html_index(&mods, "API");
        assert!(idx.contains("[\"geo.Point.norm\",\"geo.html#Point.norm\",\"Distance to the origin.\"]"), "search index");
        // a string in a signature is not linked
        assert_eq!(code_with_links("f(x: \"Point\") -> Point", &links, "other.html"), "f(x: &quot;Point&quot;) -&gt; <a href=\"geo.html#Point\">Point</a>");
    }

    #[test]
    fn module_names_and_generation() {
        assert_eq!(module_name(Path::new("src"), Path::new("src/pkg/__init__.fpy")), "pkg");
        assert_eq!(module_name(Path::new("src"), Path::new("src/pkg/mod.fpy")), "pkg.mod");
        assert_eq!(module_name(Path::new("/x"), Path::new("/x/__init__.fpy")), "__init__");
        let d = std::env::temp_dir().join(format!("fpy-doc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src/pkg")).unwrap();
        std::fs::write(d.join("src/pkg/geo.fpy"), SRC).unwrap();
        std::fs::write(d.join("src/bad.fpy"), "def (:\n").unwrap();
        let files = vec![(d.join("src"), d.join("src/pkg/geo.fpy")), (d.join("src"), d.join("src/bad.fpy")), (d.join("src"), d.join("src/missing.fpy"))];
        let opts = DocOptions { format: Format::Html, private: false, title: "Docs".into() };
        let r = generate(&files, &d.join("html"), &opts);
        assert_eq!(r.errors.len(), 2, "{:?}", r.errors);
        assert!(d.join("html/pkg.geo.html").exists() && d.join("html/index.html").exists());
        let r = generate(&files[..1], &d.join("md"), &DocOptions { format: Format::Markdown, ..opts.clone() });
        assert!(r.errors.is_empty());
        assert!(std::fs::read_to_string(d.join("md/index.md")).unwrap().contains("| [`pkg.geo`](pkg.geo.md) | Geometry helpers. |"));
        // an output path that is a file cannot be created
        std::fs::write(d.join("taken"), "").unwrap();
        assert!(!generate(&files[..1], &d.join("taken"), &opts).errors.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
