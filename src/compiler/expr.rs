//! Expressions: typing and lowering to C.

use super::func::{Local, CE};
use super::*;

pub type Narrow = Vec<(String, Ty)>;

fn binop_code(op: BinOp) -> i32 {
    match op {
        BinOp::Add => 0,
        BinOp::Sub => 1,
        BinOp::Mul => 2,
        BinOp::Div => 3,
        BinOp::FloorDiv => 4,
        BinOp::Mod => 5,
        BinOp::Pow => 6,
        BinOp::LShift => 7,
        BinOp::RShift => 8,
        BinOp::BitAnd => 9,
        BinOp::BitOr => 10,
        BinOp::BitXor => 11,
        BinOp::MatMul => 2,
    }
}

impl Compiler {
    pub fn expr(&mut self, e: &Expr, want: Option<&Ty>) -> R<CE> {
        // Builtin callables used as values (key=len, key=str.lower, map(int, ...)) become lambdas.
        if let Some(Ty::Func(ps, _)) = want {
            if let Some(lam) = self.builtin_as_lambda(e, ps.len()) {
                return self.expr(&lam, want);
            }
        }
        let want = want.filter(|w| !has_unknown(w));
        match &e.kind {
            ExprKind::Int(v) => {
                if want == Some(&Ty::Float) {
                    return Ok(CE::new(Ty::Float, format!("{:?}", *v as f64)));
                }
                if *v == i64::MIN {
                    return Ok(CE::new(Ty::Int, "INT64_MIN"));
                }
                Ok(CE::new(Ty::Int, format!("INT64_C({})", v)))
            }
            ExprKind::BigInt(s) => Err(self.e(e.span, format!("integer literal {} does not fit in a 64-bit int", s))),
            ExprKind::Float(v) => Ok(CE::new(Ty::Float, float_lit(*v))),
            ExprKind::Imag(_) => Err(self.e(e.span, "complex numbers are not supported")),
            ExprKind::Bool(b) => Ok(CE::new(Ty::Bool, if *b { "true" } else { "false" })),
            ExprKind::NoneLit => Ok(CE::new(Ty::NoneT, "NULL")),
            ExprKind::Ellipsis => Err(self.e(e.span, "'...' is not a value in fpython")),
            ExprKind::Str { value, .. } => Ok(CE::new(Ty::Str, self.string_lit(value))),
            ExprKind::Bytes { value, .. } => Ok(CE::new(Ty::Bytes, format!("fpy_bytes_new((const uint8_t*){}, {})", c_string(value), value.len()))),
            ExprKind::FStr { parts, .. } => self.fstring(parts),
            ExprKind::Name(n) => self.load_name(n, e.span),
            ExprKind::BinOp { left, op, right } => {
                // string/list literal repetition etc. all handled in binop_ce
                let lw = match (want, op) {
                    (Some(Ty::Float), _) => Some(Ty::Float),
                    (Some(w @ Ty::List(_)), BinOp::Add) => Some(w.clone()),
                    _ => None,
                };
                let l = self.expr(left, lw.as_ref())?;
                let rw = match (&l.ty, op) {
                    (Ty::List(_), BinOp::Add) | (Ty::Set(_), _) | (Ty::Dict(..), _) => Some(l.ty.clone()),
                    (Ty::Float, _) => None,
                    _ => None,
                };
                let r = self.expr(right, rw.as_ref())?;
                self.binop_ce(l, *op, r, e.span)
            }
            ExprKind::UnaryOp { op, operand } => {
                let v = self.expr(operand, if *op == UnOp::Not { None } else { want })?;
                match op {
                    UnOp::Not => {
                        let t = self.truthy(&v)?;
                        Ok(CE::new(Ty::Bool, format!("(!{})", t)))
                    }
                    UnOp::Neg => match &v.ty {
                        Ty::Int | Ty::Bool => Ok(CE::new(Ty::Int, format!("fpy_neg({})", self.coerce(&v.c, &v.ty, &Ty::Int)))),
                        Ty::Float => Ok(CE::new(Ty::Float, format!("(-({}))", v.c))),
                        Ty::Any => Ok(CE::new(Ty::Any, format!("fpy_dyn_neg({})", v.c))),
                        Ty::Class(c) => {
                            let f = self.find_method(*c, "__neg__").ok_or_else(|| self.e(e.span, "bad operand type for unary -"))?;
                            self.call_method_ce(v.clone(), f, vec![], None, e.span)
                        }
                        t => Err(self.e(e.span, format!("bad operand type for unary -: {}", self.tyname(t)))),
                    },
                    UnOp::Pos => match &v.ty {
                        Ty::Int | Ty::Float => Ok(v),
                        Ty::Bool => Ok(CE::new(Ty::Int, format!("((fpy_int){})", v.c))),
                        t => Err(self.e(e.span, format!("bad operand type for unary +: {}", self.tyname(t)))),
                    },
                    UnOp::Invert => match &v.ty {
                        Ty::Int | Ty::Bool => Ok(CE::new(Ty::Int, format!("(~(fpy_int)({}))", v.c))),
                        t => Err(self.e(e.span, format!("bad operand type for unary ~: {}", self.tyname(t)))),
                    },
                }
            }
            ExprKind::BoolOp { op, values } => self.boolop(*op, values, want, e.span),
            ExprKind::Compare { left, ops, comparators } => self.compare(left, ops, comparators),
            ExprKind::Call { func, args, keywords } => self.call(func, args, keywords, want, e.span),
            ExprKind::Attribute { value, attr } => self.attribute(value, attr, e.span),
            ExprKind::Subscript { value, index } => self.subscript(value, index, e.span),
            ExprKind::List(elts) => self.list_lit(elts, want, e.span),
            ExprKind::Tuple { elts, .. } => self.tuple_lit(elts, want, e.span),
            ExprKind::Set(elts) => self.set_lit(elts, want, e.span),
            ExprKind::Dict(items) => self.dict_lit(items, want, e.span),
            ExprKind::ListComp { elt, gens } => self.comprehension(CompKind::List, elt, None, gens, want, e.span),
            ExprKind::SetComp { elt, gens } => self.comprehension(CompKind::Set, elt, None, gens, want, e.span),
            ExprKind::DictComp { key, value, gens } => self.comprehension(CompKind::Dict, key, Some(value), gens, want, e.span),
            ExprKind::GenExp { elt, gens } => self.genexp(elt, gens, e.span),
            ExprKind::IfExp { test, body, orelse } => {
                let (c, pos, neg) = self.cond3(test)?;
                let saved = self.fxr().narrow.len();
                self.fx().narrow.extend(pos);
                let a = self.expr(body, want);
                self.fx().narrow.truncate(saved);
                let a = a?;
                self.fx().narrow.extend(neg);
                let b = self.expr(orelse, want.or(Some(&a.ty)));
                self.fx().narrow.truncate(saved);
                let b = b?;
                let t = self.unify(&a.ty, &b.ty).or_else(|| want.cloned().filter(|w| self.assignable(&a.ty, w) && self.assignable(&b.ty, w))).ok_or_else(|| {
                    self.e(e.span, format!("conditional expression branches have incompatible types {} and {}", self.tyname(&a.ty), self.tyname(&b.ty)))
                })?;
                let ac = self.coerce(&a.c, &a.ty, &t);
                let bc = self.coerce(&b.c, &b.ty, &t);
                Ok(CE::new(t, format!("(({}) ? ({}) : ({}))", c, ac, bc)))
            }
            ExprKind::Lambda { params, body } => self.lambda(params, body, want, e.span),
            ExprKind::NamedExpr { target, value } => {
                let tw = self.target_type(&Expr::new(ExprKind::Name(target.clone()), e.span));
                let v = self.expr(value, tw.as_ref())?;
                let t = match tw {
                    Some(t) => t,
                    None => self.infer_var_type(&v.ty, e.span, target)?,
                };
                let tmp = self.tmp_var(&t, "nx");
                let vc = self.check_coerce(&v, &t, value.span)?;
                // The assignment must happen inside the expression for correct evaluation order.
                let saved_body = std::mem::take(&mut self.fx().body);
                let saved_indent = self.fxr().indent;
                self.fx().indent = 0;
                self.store_name(target, CE::new(t.clone(), tmp.clone()), e.span)?;
                let assign = std::mem::replace(&mut self.fx().body, saved_body);
                self.fx().indent = saved_indent;
                Ok(CE::new(t, format!("({{ {} = {}; {} {}; }})", tmp, vc, assign.trim(), tmp)))
            }
            ExprKind::Yield(v) => {
                let (g, elem) = self.fxr().gen.clone().ok_or_else(|| self.e(e.span, "'yield' outside generator function"))?;
                let val = match v {
                    Some(v) => self.expr(v, if has_unknown(&elem) { None } else { Some(&elem) })?,
                    None => CE::new(Ty::NoneT, "NULL"),
                };
                let elem = if has_unknown(&elem) {
                    let t = self.infer_var_type(&val.ty, e.span, "<yield>")?;
                    self.fx().gen = Some((g.clone(), t.clone()));
                    t
                } else {
                    elem
                };
                let c = self.check_coerce(&val, &elem, e.span)?;
                // the expression's value is what send() passed (boxed; NULL for next())
                let recv = format!("fpy_gen_yield_recv({}, {})", g, elem.to_word(&c));
                let st = self.fxr().gen_send.clone();
                Ok(match st {
                    Ty::NoneT => CE::new(Ty::NoneT, format!("((void)({}), (FpyObj*)NULL)", recv)),
                    Ty::Any => CE::new(Ty::Any, format!("((FpyObj*)fpy_pw({}))", recv)),
                    t if t.is_scalar() => {
                        // next() sends None: a scalar send type reads it as zero
                        let un = self.unbox("_s", &t);
                        let ct = self.ctype(&t);
                        CE::new(t.clone(), format!("({{ FpyObj *_s = (FpyObj*)fpy_pw({}); _s ? {} : ({})0; }})", recv, un, ct))
                    }
                    // `X | None` / unions: unboxing already accepts None
                    t @ (Ty::Optional(_) | Ty::Union(_)) => {
                        let un = self.unbox("_s", &t);
                        CE::new(t.clone(), format!("({{ FpyObj *_s = (FpyObj*)fpy_pw({}); {}; }})", recv, un))
                    }
                    // other types: None (from next()) makes the value optional
                    t => {
                        let un = self.unbox("_s", &t);
                        let ct = self.ctype(&t);
                        CE::new(Ty::Optional(Box::new(t.clone())), format!("({{ FpyObj *_s = (FpyObj*)fpy_pw({}); _s ? {} : ({})NULL; }})", recv, un, ct))
                    }
                })
            }
            ExprKind::YieldFrom(v) => {
                let (g, elem) = self.fxr().gen.clone().ok_or_else(|| self.e(e.span, "'yield from' outside generator function"))?;
                let it = self.expr(v, None)?;
                let et = self.elem_type(&it.ty).ok_or_else(|| self.e(v.span, "yield from needs an iterable"))?;
                let elem = if has_unknown(&elem) {
                    self.fx().gen = Some((g.clone(), et.clone()));
                    et.clone()
                } else {
                    elem
                };
                if !self.assignable(&et, &elem) {
                    return Err(self.e(v.span, format!("yield from produces {}, generator yields {}", self.tyname(&et), self.tyname(&elem))));
                }
                let ic = self.coerce(&it.c, &it.ty, &Ty::iter(et.clone()));
                let ect = self.ctype(&et);
                let conv = self.coerce(&et.from_word("_w", &ect), &et, &elem);
                Ok(CE::new(Ty::NoneT, format!("({{ FpyIter *_yi = {}; fpy_word _w; while (_yi->next(_yi, &_w)) fpy_gen_yield({}, {}); }})", ic, g, elem.to_word(&conv))))
            }
            ExprKind::Await(x) => {
                if self.fxr().async_ret.is_none() {
                    return Err(self.e(e.span, "'await' outside async function"));
                }
                let v = self.expr(x, None)?;
                let c = self.coerce(&v.c, &v.ty, &Ty::Any);
                Ok(CE::new(Ty::Any, format!("fpy_await(gen_, {})", c)))
            }
            ExprKind::Starred(_) => Err(self.e(e.span, "starred expression is not allowed here")),
            ExprKind::Slice { .. } => Err(self.e(e.span, "slice is only valid inside []")),
        }
    }

    fn builtin_as_lambda(&mut self, e: &Expr, nparams: usize) -> Option<Expr> {
        let sp = e.span;
        let names: Vec<String> = (0..nparams).map(|i| format!("\u{2}a{}", i)).collect();
        let args: Vec<Expr> = names.iter().map(|n| Expr::new(ExprKind::Name(n.clone()), sp)).collect();
        let call = match &e.kind {
            ExprKind::Name(n) if super::call::BUILTIN_FUNCS.contains(&n.as_str()) && self.is_builtin_name(n) => {
                Expr::new(ExprKind::Call { func: Box::new(e.clone()), args: args.clone(), keywords: vec![] }, sp)
            }
            ExprKind::Attribute { value, attr } if nparams >= 1 => match &value.kind {
                ExprKind::Name(n) if matches!(n.as_str(), "str" | "int" | "float" | "list" | "dict" | "set" | "bytes") && self.is_builtin_name(n) => {
                    let recv = args[0].clone();
                    let m = Expr::new(ExprKind::Attribute { value: Box::new(recv), attr: attr.clone() }, sp);
                    Expr::new(ExprKind::Call { func: Box::new(m), args: args[1..].to_vec(), keywords: vec![] }, sp)
                }
                _ => return None,
            },
            _ => return None,
        };
        let params = Params { args: names.iter().map(|n| Arg { name: n.clone(), ann: None, default: None, span: sp }).collect(), ..Default::default() };
        Some(Expr::new(ExprKind::Lambda { params: Box::new(params), body: Box::new(call) }, sp))
    }

    // ------------------------------------------------------------------ names

    pub fn is_builtin_name(&self, n: &str) -> bool {
        let fx = self.fxr();
        if fx.comp.iter().any(|c| c.contains_key(n)) || fx.locals.contains_key(n) || fx.env.contains_key(n) {
            return false;
        }
        if fx.scope.locals.contains(n) {
            return false;
        }
        !self.modules[fx.module].scope.contains_key(n) || matches!(self.modules[fx.module].scope.get(n), Some(Sym::Builtin(_)))
    }

    pub fn load_name(&mut self, n: &str, span: Span) -> R<CE> {
        let base = self.load_name_raw(n, span)?;
        // apply narrowing
        let nar = self.fxr().narrow.iter().rev().find(|(x, _)| x == n).map(|(_, t)| t.clone());
        if let Some(t) = nar {
            if t != base.ty {
                let c = self.narrow_conv(&base, &t);
                return Ok(CE::new(t, c));
            }
        }
        Ok(base)
    }

    pub fn narrow_conv(&mut self, v: &CE, to: &Ty) -> String {
        match (&v.ty, to) {
            (Ty::Optional(i), t) if **i == *t => {
                if t.is_scalar() {
                    self.unbox(&v.c, t)
                } else if i.is_boxed() {
                    v.c.clone()
                } else {
                    v.c.clone()
                }
            }
            (f, t) if f.is_boxed() && !t.is_boxed() => self.unbox_unchecked(&v.c, t),
            (Ty::Class(_), Ty::Class(c)) | (Ty::Optional(_), Ty::Class(c)) => format!("((struct {}*)({}))", self.classes[*c].cname, v.c),
            (_, t) => format!("(({})({}))", self.ctype(t), v.c),
        }
    }

    fn unbox_unchecked(&mut self, c: &str, t: &Ty) -> String {
        match t {
            Ty::Int => format!("fpy_unbox_int({})", c),
            Ty::Float => format!("fpy_unbox_float({})", c),
            Ty::Bool => format!("(((FpyBox*)({}))->v != 0)", c),
            Ty::Optional(i) if i.is_scalar() => c.to_string(),
            _ => format!("(({})({}))", self.ctype(t), c),
        }
    }

    fn load_name_raw(&mut self, n: &str, span: Span) -> R<CE> {
        for i in (0..self.fxr().comp.len()).rev() {
            if let Some(l) = self.fxr().comp[i].get(n).cloned() {
                return Ok(CE::new(l.ty, l.cname));
            }
        }
        let is_global_decl = self.fxr().scope.globals.contains(n);
        if !is_global_decl {
            if let Some(l) = self.fxr().locals.get(n).cloned() {
                if l.cell {
                    let ct = self.ctype(&l.ty);
                    return Ok(CE::new(l.ty.clone(), l.ty.from_word(&format!("{}->v", l.cname), &ct)));
                }
                return Ok(CE::new(l.ty, l.cname));
            }
            if let Some((slot, t)) = self.fxr().env.get(n).cloned() {
                let ct = self.ctype(&t);
                return Ok(CE::new(t.clone(), t.from_word(&format!("((FpyCell*)fpy_pw(env->items[{}]))->v", slot), &ct)));
            }
            if self.fxr().scope.locals.contains(n) && !self.fxr().is_module {
                return Err(self.e(span, format!("local variable '{}' is referenced before assignment", n)));
            }
        }
        let m = self.cur_mod();
        match self.lookup_sym(m, n) {
            Some(Sym::Global(g)) => {
                let ty = match &self.globals[g].ty {
                    Some(t) => t.clone(),
                    None => return Err(self.e(span, format!("global '{}' is used before its type is known; annotate it at module level", n))),
                };
                Ok(CE::new(ty, format!("G.{}", self.globals[g].cname)))
            }
            Some(Sym::Func(f)) => self.func_value(f, span),
            Some(Sym::Class(c)) => Ok(CE::new(Ty::Type(c), format!("fpy_class_obj(&T_{})", self.classes[c].cname))),
            Some(Sym::Module(_)) => Err(self.e(span, format!("module '{}' cannot be used as a value", n))),
            Some(Sym::Alias(_)) | Some(Sym::ClassTemplate(_)) => Err(self.e(span, format!("type '{}' cannot be used as a value here", n))),
            Some(Sym::Builtin(b)) => Err(self.e(span, format!("'{}' cannot be used as a value", b))),
            None => match n {
                "__name__" => {
                    let s = if m == self.mod_index.get("__main__").copied().unwrap_or(usize::MAX) { "__main__".to_string() } else { self.modules[m].name.clone() };
                    Ok(CE::new(Ty::Str, self.string_lit(&s)))
                }
                "__file__" => {
                    let p = self.modules[m].path.clone();
                    Ok(CE::new(Ty::Str, self.string_lit(&p)))
                }
                "NotImplemented" => Err(self.e(span, "NotImplemented is not supported; raise TypeError instead")),
                _ => {
                    if super::call::BUILTIN_FUNCS.contains(&n) {
                        return Err(self.e(span, format!("builtin '{}' cannot be used as a first-class value; wrap it in a lambda", n)));
                    }
                    Err(self.e(span, format!("name '{}' is not defined", n)))
                }
            },
        }
    }

    /// A first-class function value for a top-level function.
    pub fn func_value(&mut self, f: usize, span: Span) -> R<CE> {
        if !self.funcs[f].def.type_params.is_empty() {
            return Err(self.e(span, format!("generic function '{}' cannot be used as a value", self.funcs[f].name)));
        }
        let sig = self.func_sig(f)?;
        if sig.vararg.is_some() || sig.kwarg.is_some() {
            return Err(self.e(span, "functions with *args/**kwargs cannot be used as values"));
        }
        let target = self.func_instance(f, vec![])?;
        let adapter = format!("FV_{}", target);
        if !self.value_adapters.contains_key(&target) {
            self.value_adapters.insert(target.clone(), adapter.clone());
            let ret_c = if sig.ret == Ty::NoneT { "void".to_string() } else { self.ctype(&sig.ret) };
            let mut ps = vec!["void *env".to_string()];
            let mut args = Vec::new();
            for (i, p) in sig.params.iter().enumerate() {
                ps.push(format!("{} a{}", self.ctype(&p.ty), i));
                args.push(format!("a{}", i));
            }
            let ret = if ret_c == "void" { "" } else { "return " };
            self.out.protos.push_str(&format!("static {} {}({});\n", ret_c, adapter, ps.join(", ")));
            self.out.funcs.push_str(&format!("static {} {}({}) {{ (void)env; {}{}({}); }}\n", ret_c, adapter, ps.join(", "), ret, target, args.join(", ")));
        }
        let dw = self.dyn_wrapper(f).unwrap_or_else(|_| "NULL".into());
        let ty = Ty::Func(sig.params.iter().map(|p| p.ty.clone()).collect(), Box::new(sig.ret.clone()));
        let name = c_string(self.funcs[f].name.as_bytes());
        Ok(CE::new(ty, format!("fpy_func_new((void*){}, NULL, {}, {})", adapter, dw, name)))
    }

    // ------------------------------------------------------------------ truthiness & conditions

    pub fn truthy(&mut self, v: &CE) -> R<String> {
        Ok(match &v.ty {
            Ty::Bool => v.c.clone(),
            Ty::Int => format!("(({}) != 0)", v.c),
            Ty::Float => format!("(({}) != 0.0)", v.c),
            Ty::Str => format!("(({})->len != 0)", v.c),
            Ty::Bytes => format!("(({})->len != 0)", v.c),
            Ty::List(_) => format!("(({})->len != 0)", v.c),
            Ty::Dict(..) | Ty::Set(_) | Ty::VarTuple(_) => format!("(({})->len != 0)", v.c),
            Ty::Tuple(ts) => {
                if ts.is_empty() {
                    "false".into()
                } else {
                    format!("((void)({}), true)", v.c)
                }
            }
            Ty::NoneT => format!("((void)({}), false)", v.c),
            Ty::Optional(i) if !i.is_scalar() && !i.is_boxed() => {
                let t = self.fresh("tv");
                let ct = self.ctype(&v.ty);
                let inner = self.truthy(&CE::new((**i).clone(), t.clone()))?;
                if inner == format!("(({}) != NULL)", t) || matches!(**i, Ty::Class(_)) && !self.class_has_bool(i) {
                    format!("(({}) != NULL)", v.c)
                } else {
                    format!("({{ {} {} = {}; {} != NULL && {}; }})", ct, t, v.c, t, inner)
                }
            }
            Ty::Class(c) => {
                if let Some(f) = self.find_method(*c, "__bool__").or_else(|| self.find_method(*c, "__len__")) {
                    let is_len = self.funcs[f].name == "__len__";
                    let call = self.call_method_ce(v.clone(), f, vec![], None, Span::default())?;
                    if is_len {
                        format!("(({}) != 0)", call.c)
                    } else {
                        call.c
                    }
                } else {
                    format!("(({}) != NULL)", v.c)
                }
            }
            Ty::Func(..) | Ty::Iter(_) | Ty::Type(_) => format!("(({}) != NULL)", v.c),
            Ty::Any | Ty::Union(_) | Ty::Optional(_) => format!("fpy_dyn_truthy((FpyObj*)({}))", v.c),
            Ty::Never | Ty::Var(_) => v.c.clone(),
        })
    }

    fn class_has_bool(&self, t: &Ty) -> bool {
        if let Ty::Class(c) = t {
            return self.find_method(*c, "__bool__").is_some() || self.find_method(*c, "__len__").is_some();
        }
        false
    }

    pub fn cond(&mut self, e: &Expr) -> R<(String, Narrow)> {
        let (c, p, _) = self.cond3(e)?;
        Ok((c, p))
    }

    /// Compile a condition, returning (C expr, narrowing if true, narrowing if false).
    pub fn cond3(&mut self, e: &Expr) -> R<(String, Narrow, Narrow)> {
        match &e.kind {
            ExprKind::UnaryOp { op: UnOp::Not, operand } => {
                let (c, p, n) = self.cond3(operand)?;
                Ok((format!("(!{})", c), n, p))
            }
            ExprKind::BoolOp { op: BoolOp::And, values } => {
                let saved = self.fxr().narrow.len();
                let mut parts = Vec::new();
                let mut pos = Vec::new();
                for v in values {
                    let (c, p, _) = self.cond3(v)?;
                    parts.push(c);
                    self.fx().narrow.extend(p.clone());
                    pos.extend(p);
                }
                self.fx().narrow.truncate(saved);
                Ok((format!("({})", parts.join(" && ")), pos, vec![]))
            }
            ExprKind::BoolOp { op: BoolOp::Or, values } => {
                let saved = self.fxr().narrow.len();
                let mut parts = Vec::new();
                let mut neg = Vec::new();
                for v in values {
                    let (c, _, n) = self.cond3(v)?;
                    parts.push(c);
                    self.fx().narrow.extend(n.clone());
                    neg.extend(n);
                }
                self.fx().narrow.truncate(saved);
                Ok((format!("({})", parts.join(" || ")), vec![], neg))
            }
            ExprKind::Compare { left, ops, comparators } if ops.len() == 1 && matches!(ops[0], CmpOp::Is | CmpOp::IsNot) && matches!(comparators[0].kind, ExprKind::NoneLit) => {
                let v = self.expr(left, None)?;
                let mut pos = vec![];
                let mut neg = vec![];
                if let (ExprKind::Name(n), Ty::Optional(inner)) = (&left.kind, &v.ty) {
                    // `x is None` -> false branch narrows to inner
                    neg.push((n.clone(), (**inner).clone()));
                }
                if let (ExprKind::Name(n), Ty::Any) = (&left.kind, &v.ty) {
                    let _ = n;
                }
                let c = match &v.ty {
                    t if t.is_scalar() => "false".to_string(),
                    _ => format!("(((const void*)({})) == NULL)", v.c),
                };
                if ops[0] == CmpOp::IsNot {
                    Ok((format!("(!{})", c), neg, pos.drain(..).collect()))
                } else {
                    Ok((c, pos, neg))
                }
            }
            ExprKind::Call { func, args, keywords } if keywords.is_empty() && args.len() == 2 && matches!(&func.kind, ExprKind::Name(n) if n == "isinstance" && self.is_builtin_name(n)) => {
                let v = self.expr(&args[0], None)?;
                let (c, target) = self.isinstance_check(&v, &args[1])?;
                let mut pos = vec![];
                let mut neg = vec![];
                if let ExprKind::Name(n) = &args[0].kind {
                    if let Some(t) = target {
                        pos.push((n.clone(), t.clone()));
                        // negative narrowing for 2-member unions / optionals
                        match &v.ty {
                            Ty::Union(ms) if ms.len() == 2 => {
                                if let Some(other) = ms.iter().find(|m| **m != t) {
                                    neg.push((n.clone(), other.clone()));
                                }
                            }
                            Ty::Optional(i) if **i == t => {}
                            _ => {}
                        }
                    }
                }
                Ok((c, pos, neg))
            }
            ExprKind::Name(n) => {
                let v = self.expr(e, None)?;
                let c = self.truthy(&v)?;
                let pos = match &v.ty {
                    Ty::Optional(inner) => vec![(n.clone(), (**inner).clone())],
                    _ => vec![],
                };
                Ok((c, pos, vec![]))
            }
            _ => {
                let v = self.expr(e, Some(&Ty::Bool))?;
                let c = self.truthy(&v)?;
                Ok((c, vec![], vec![]))
            }
        }
    }

    pub fn isinstance_builtin(&mut self, c: &str, t: &Ty) -> String {
        match t {
            Ty::Int => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_int))", c),
            Ty::Float => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_float))", c),
            Ty::Bool => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_bool))", c),
            Ty::Str => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_str))", c),
            Ty::Bytes => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_bytes))", c),
            Ty::List(_) => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_list))", c),
            Ty::Dict(..) => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_dict))", c),
            Ty::Set(_) => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_set))", c),
            Ty::Tuple(_) | Ty::VarTuple(_) => format!("(({0}) && (((FpyObj*)({0}))->ty == &fpy_type_tuple))", c),
            Ty::Class(id) => format!("fpy_isinstance_t((FpyObj*)({}), &T_{})", c, self.classes[*id].cname),
            Ty::NoneT => format!("(({}) == NULL)", c),
            _ => "0".into(),
        }
    }

    /// isinstance(v, T): returns (C condition, narrowed type when true).
    pub fn isinstance_check(&mut self, v: &CE, te: &Expr) -> R<(String, Option<Ty>)> {
        let m = self.cur_mod();
        // isinstance(x, PythonClass): runtime check through the bridge
        if let ExprKind::Name(n) | ExprKind::Attribute { attr: n, .. } = &te.kind {
            let is_py = match &te.kind {
                ExprKind::Name(_) => matches!(self.lookup_sym(m, n), Some(Sym::Global(g)) if self.globals[g].ty == Some(Ty::Any)),
                _ => self.module_path(te).is_none() && !matches!(self.resolve_ann(te, m, &HashMap::new()), Ok(_)),
            };
            if is_py {
                let cls = self.expr(te, None)?;
                let o = self.coerce(&v.c, &v.ty, &Ty::Any);
                return Ok((format!("fpy_dyn_isinstance({}, {})", o, cls.c), None));
            }
        }
        let subst = self.fxr().subst.clone();
        let targets: Vec<Expr> = match &te.kind {
            ExprKind::Tuple { elts, .. } => elts.clone(),
            _ => vec![te.clone()],
        };
        let mut conds = Vec::new();
        let mut tys = Vec::new();
        for t in &targets {
            let ty = match &t.kind {
                ExprKind::Name(n) if matches!(n.as_str(), "list" | "dict" | "set" | "tuple") => match n.as_str() {
                    "list" => match &v.ty {
                        Ty::List(_) => v.ty.clone(),
                        _ => Ty::list(Ty::Any),
                    },
                    "dict" => match &v.ty {
                        Ty::Dict(..) => v.ty.clone(),
                        _ => Ty::dict(Ty::Any, Ty::Any),
                    },
                    "set" => Ty::set(Ty::Any),
                    _ => Ty::Tuple(vec![]),
                },
                _ => self.resolve_ann(t, m, &subst)?,
            };
            let c = match (&v.ty, &ty) {
                (a, b) if a == b => "true".to_string(),
                (Ty::Int, Ty::Float) => "false".into(),
                (Ty::Bool, Ty::Int) => "true".into(),
                (a, _) if a.is_scalar() => "false".into(),
                _ => {
                    let obj = if v.ty.is_boxed() || matches!(v.ty, Ty::Class(_) | Ty::Optional(_)) {
                        format!("(FpyObj*)({})", v.c)
                    } else {
                        format!("(FpyObj*)({})", v.c)
                    };
                    self.isinstance_builtin(&obj, &ty)
                }
            };
            conds.push(c);
            tys.push(ty);
        }
        let narrowed = if tys.len() == 1 {
            let t = tys.pop().unwrap();
            match (&v.ty, &t) {
                (Ty::Class(a), Ty::Class(b)) if self.is_subclass(*a, *b) => None,
                (_, Ty::Tuple(ts)) if ts.is_empty() => None,
                (Ty::List(_), _) | (Ty::Dict(..), _) => None,
                (_, Ty::List(e)) | (_, Ty::Set(e)) if **e == Ty::Any => None,
                (_, Ty::Dict(_, e)) if **e == Ty::Any => None,
                _ => Some(t),
            }
        } else {
            None
        };
        Ok((format!("({})", conds.join(" || ")), narrowed))
    }

    // ------------------------------------------------------------------ operators

    pub fn binop_ce(&mut self, l: CE, op: BinOp, r: CE, span: Span) -> R<CE> {
        let num = |t: &Ty| matches!(t, Ty::Int | Ty::Bool | Ty::Float);
        if num(&l.ty) && num(&r.ty) {
            let is_float = l.ty == Ty::Float || r.ty == Ty::Float || op == BinOp::Div;
            let bitop = matches!(op, BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor);
            if bitop && l.ty == Ty::Bool && r.ty == Ty::Bool {
                let o = match op {
                    BinOp::BitAnd => "&",
                    BinOp::BitOr => "|",
                    _ => "^",
                };
                return Ok(CE::new(Ty::Bool, format!("((bool)(({}) {} ({})))", l.c, o, r.c)));
            }
            if is_float {
                if matches!(op, BinOp::LShift | BinOp::RShift | BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::MatMul) {
                    return Err(self.e(span, format!("unsupported operand types for {}: float", op.symbol())));
                }
                let a = self.coerce(&l.c, &l.ty, &Ty::Float);
                let b = self.coerce(&r.c, &r.ty, &Ty::Float);
                let c = match op {
                    BinOp::Add => format!("(({}) + ({}))", a, b),
                    BinOp::Sub => format!("(({}) - ({}))", a, b),
                    BinOp::Mul => format!("(({}) * ({}))", a, b),
                    BinOp::Div => format!("fpy_truediv({}, {})", a, b),
                    BinOp::FloorDiv => format!("fpy_ffloordiv({}, {})", a, b),
                    BinOp::Mod => format!("fpy_fmod({}, {})", a, b),
                    BinOp::Pow => format!("pow({}, {})", a, b),
                    _ => unreachable!(),
                };
                return Ok(CE::new(Ty::Float, c));
            }
            let a = self.coerce(&l.c, &l.ty, &Ty::Int);
            let b = self.coerce(&r.c, &r.ty, &Ty::Int);
            let c = match op {
                BinOp::Add => format!("fpy_add({}, {})", a, b),
                BinOp::Sub => format!("fpy_sub({}, {})", a, b),
                BinOp::Mul => format!("fpy_mul({}, {})", a, b),
                BinOp::FloorDiv => format!("fpy_floordiv({}, {})", a, b),
                BinOp::Mod => format!("fpy_mod({}, {})", a, b),
                BinOp::Pow => format!("fpy_ipow({}, {})", a, b),
                BinOp::LShift => format!("fpy_lshift({}, {})", a, b),
                BinOp::RShift => format!("fpy_rshift({}, {})", a, b),
                BinOp::BitAnd => format!("(({}) & ({}))", a, b),
                BinOp::BitOr => format!("(({}) | ({}))", a, b),
                BinOp::BitXor => format!("(({}) ^ ({}))", a, b),
                BinOp::MatMul => return Err(self.e(span, "unsupported operand types for @: int")),
                BinOp::Div => unreachable!(),
            };
            return Ok(CE::new(Ty::Int, c));
        }
        match (&l.ty, op, &r.ty) {
            (Ty::Str, BinOp::Add, Ty::Str) => return Ok(CE::new(Ty::Str, format!("fpy_str_concat({}, {})", l.c, r.c))),
            (Ty::Str, BinOp::Mul, Ty::Int) => return Ok(CE::new(Ty::Str, format!("fpy_str_repeat({}, {})", l.c, r.c))),
            (Ty::Int, BinOp::Mul, Ty::Str) => return Ok(CE::new(Ty::Str, format!("fpy_str_repeat({}, {})", r.c, l.c))),
            (Ty::Bytes, BinOp::Mul, Ty::Int) => return Ok(CE::new(Ty::Bytes, format!("fpy_bytes_repeat({}, {})", l.c, r.c))),
            (Ty::Int, BinOp::Mul, Ty::Bytes) => return Ok(CE::new(Ty::Bytes, format!("fpy_bytes_repeat({}, {})", r.c, l.c))),
            (Ty::Str, BinOp::Mod, _) => return Err(self.e(span, "printf-style '%' formatting is not supported; use an f-string")),
            (Ty::Bytes, BinOp::Add, Ty::Bytes) => return Ok(CE::new(Ty::Bytes, format!("fpy_bytes_concat({}, {})", l.c, r.c))),
            (Ty::List(a), BinOp::Add, Ty::List(b)) => {
                if has_unknown(b) {
                    return Ok(CE::new(l.ty.clone(), format!("fpy_list_copy({})", l.c)));
                }
                if has_unknown(a) {
                    return Ok(CE::new(r.ty.clone(), format!("fpy_list_copy({})", r.c)));
                }
                if a != b {
                    return Err(self.e(span, format!("cannot concatenate {} and {}", self.tyname(&l.ty), self.tyname(&r.ty))));
                }
                return Ok(CE::new(l.ty.clone(), format!("fpy_list_concat({}, {})", l.c, r.c)));
            }
            (Ty::List(_), BinOp::Mul, Ty::Int) => return Ok(CE::new(l.ty.clone(), format!("fpy_list_repeat({}, {})", l.c, r.c))),
            (Ty::Int, BinOp::Mul, Ty::List(_)) => return Ok(CE::new(r.ty.clone(), format!("fpy_list_repeat({}, {})", r.c, l.c))),
            // concatenation involving a tuple[T, ...]: both sides converted to tuple[T, ...]
            (Ty::VarTuple(e), BinOp::Add, Ty::Tuple(_) | Ty::VarTuple(_)) | (Ty::Tuple(_), BinOp::Add, Ty::VarTuple(e)) => {
                let vt = Ty::VarTuple(e.clone());
                let lc = self.check_coerce(&l, &vt, span)?;
                let rc = self.check_coerce(&r, &vt, span)?;
                return Ok(CE::new(vt, format!("fpy_tuple_concat({}, {})", lc, rc)));
            }
            (Ty::Tuple(a), BinOp::Add, Ty::Tuple(b)) => {
                let mut ts = a.clone();
                ts.extend(b.iter().cloned());
                let kinds: Vec<u8> = ts.iter().map(|t| t.kind()).collect();
                let karr = self.kinds_array(&kinds);
                let c = format!(
                    "({{ FpyTuple *_a = {}, *_b = {}; FpyTuple *_t = fpy_tuple_new({}, {}); memcpy(_t->items, _a->items, {} * 8); memcpy(_t->items + {}, _b->items, {} * 8); _t; }})",
                    l.c,
                    r.c,
                    ts.len(),
                    karr,
                    a.len(),
                    a.len(),
                    b.len()
                );
                return Ok(CE::new(Ty::Tuple(ts), c));
            }
            (Ty::Set(a), _, Ty::Set(b)) if a == b || has_unknown(b) => {
                let f = match op {
                    BinOp::BitOr => "fpy_set_union",
                    BinOp::BitAnd => "fpy_set_intersection",
                    BinOp::Sub => "fpy_set_difference",
                    BinOp::BitXor => "fpy_set_symdiff",
                    _ => return Err(self.e(span, format!("unsupported operand for sets: {}", op.symbol()))),
                };
                return Ok(CE::new(l.ty.clone(), format!("{}({}, {})", f, l.c, r.c)));
            }
            (Ty::Dict(..), BinOp::BitOr, Ty::Dict(..)) if l.ty == r.ty => {
                return Ok(CE::new(l.ty.clone(), format!("({{ FpyDict *_d = fpy_dict_copy({}); fpy_dict_update(_d, {}); _d; }})", l.c, r.c)));
            }
            _ => {}
        }
        if let Ty::Class(c) = &l.ty {
            let name = format!("__{}__", op.dunder());
            if let Some(f) = self.find_method(*c, &name) {
                let rr = r.clone();
                return self.call_method_with_ces(l, f, vec![rr], span);
            }
        }
        if let Ty::Class(c) = &r.ty {
            let name = format!("__r{}__", op.dunder());
            if let Some(f) = self.find_method(*c, &name) {
                return self.call_method_with_ces(r, f, vec![l], span);
            }
        }
        if l.ty == Ty::Any || r.ty == Ty::Any {
            let a = self.coerce(&l.c, &l.ty, &Ty::Any);
            let b = self.coerce(&r.c, &r.ty, &Ty::Any);
            return Ok(CE::new(Ty::Any, format!("fpy_dyn_binop({}, {}, {})", binop_code(op), a, b)));
        }
        Err(self.e(span, format!("unsupported operand types for {}: {} and {}", op.symbol(), self.tyname(&l.ty), self.tyname(&r.ty))))
    }

    fn boolop(&mut self, op: BoolOp, values: &[Expr], want: Option<&Ty>, span: Span) -> R<CE> {
        // Fast path: all boolean -> C logical operators.
        if want == Some(&Ty::Bool) || values.iter().all(|v| self.is_boolish(v)) {
            let (c, _, _) = self.cond3(&Expr::new(ExprKind::BoolOp { op, values: values.to_vec() }, span))?;
            return Ok(CE::new(Ty::Bool, c));
        }
        // Value semantics: `a or b` returns a if truthy else b.
        // For `and`, later operands only run when earlier ones were truthy: narrow accordingly
        // (`name and name.upper()` with `name: str | None`).
        let saved_narrow = self.fxr().narrow.len();
        let mut ces = Vec::new();
        for (i, v) in values.iter().enumerate() {
            let w = if i == 0 { want.cloned() } else { want.cloned().or_else(|| ces.first().map(|c: &CE| match &c.ty { Ty::Optional(i) => (**i).clone(), t => t.clone() })) };
            if i > 0 && op == BoolOp::And {
                let dry = self.fxr().dry_run;
                self.fx().dry_run = true;
                let narrowed = self.cond3(&values[i - 1]).map(|(_, p, _)| p);
                self.fx().dry_run = dry;
                if let Ok(p) = narrowed {
                    self.fx().narrow.extend(p);
                }
            }
            match self.expr(v, w.as_ref()) {
                Ok(ce) => ces.push(ce),
                Err(e) => {
                    self.fx().narrow.truncate(saved_narrow);
                    return Err(e);
                }
            }
        }
        self.fx().narrow.truncate(saved_narrow);
        // Result type: last operand's type, with earlier Optional[T] collapsing to T for `or`.
        let mut ty = ces.last().unwrap().ty.clone();
        for c in &ces[..ces.len() - 1] {
            let base = match (&c.ty, op) {
                (Ty::Optional(i), BoolOp::Or) => (**i).clone(),
                (t, _) => t.clone(),
            };
            ty = self.unify(&base, &ty).ok_or_else(|| self.e(span, format!("operands of '{}' have incompatible types {} and {}", if op == BoolOp::And { "and" } else { "or" }, self.tyname(&c.ty), self.tyname(&ty))))?;
        }
        let tmp = self.fresh("bo");
        let ct = self.ctype(&ty);
        let mut code = String::new();
        let n = ces.len();
        // ({ T t = a; if (truthy(a)) ... })
        let first = &ces[0];
        let fty = first.ty.clone();
        let ft = self.fresh("bf");
        let fct = self.ctype(&fty);
        code.push_str(&format!("({{ {} {}; {} {} = {}; ", ct, tmp, fct, ft, first.c));
        let truth = self.truthy(&CE::new(fty.clone(), ft.clone()))?;
        let first_conv = match (&fty, op) {
            (Ty::Optional(i), BoolOp::Or) if **i == ty || self.assignable(i, &ty) => {
                let inner = self.narrow_conv(&CE::new(fty.clone(), ft.clone()), i);
                self.coerce(&inner, i, &ty)
            }
            _ => {
                if self.assignable(&fty, &ty) {
                    self.coerce(&ft, &fty, &ty)
                } else {
                    return Err(self.e(span, format!("cannot combine {} and {}", self.tyname(&fty), self.tyname(&ty))));
                }
            }
        };
        let rest_c = if n == 2 {
            let c = &ces[1];
            self.coerce(&c.c, &c.ty, &ty)
        } else {
            let rest = Expr::new(ExprKind::BoolOp { op, values: values[1..].to_vec() }, span);
            if op == BoolOp::And {
                let dry = self.fxr().dry_run;
                self.fx().dry_run = true;
                let narrowed = self.cond3(&values[0]).map(|(_, p, _)| p);
                self.fx().dry_run = dry;
                if let Ok(p) = narrowed {
                    self.fx().narrow.extend(p);
                }
            }
            let ce = self.expr(&rest, Some(&ty));
            self.fx().narrow.truncate(saved_narrow);
            let ce = ce?;
            self.coerce(&ce.c, &ce.ty, &ty)
        };
        match op {
            BoolOp::Or => code.push_str(&format!("if ({}) {} = {}; else {} = {}; {}; }})", truth, tmp, first_conv, tmp, rest_c, tmp)),
            BoolOp::And => code.push_str(&format!("if (!{}) {} = {}; else {} = {}; {}; }})", truth, tmp, first_conv, tmp, rest_c, tmp)),
        }
        Ok(CE::new(ty, code))
    }

    fn is_boolish(&mut self, e: &Expr) -> bool {
        match &e.kind {
            ExprKind::Compare { .. } | ExprKind::Bool(_) => true,
            ExprKind::UnaryOp { op: UnOp::Not, .. } => true,
            ExprKind::BoolOp { values, .. } => values.iter().all(|v| self.is_boolish(v)),
            ExprKind::Call { func, .. } => matches!(&func.kind, ExprKind::Name(n) if n == "isinstance" || n == "callable" || n == "hasattr"),
            _ => {
                let saved = self.fxr().dry_run;
                self.fx().dry_run = true;
                let r = self.expr(e, None);
                self.fx().dry_run = saved;
                matches!(r, Ok(CE { ty: Ty::Bool, .. }))
            }
        }
    }

    /// Least upper bound of two types, if one exists.
    pub fn unify(&self, a: &Ty, b: &Ty) -> Option<Ty> {
        if a == b {
            return Some(a.clone());
        }
        if has_unknown(a) && std::mem::discriminant(a) == std::mem::discriminant(b) {
            return Some(b.clone());
        }
        if has_unknown(b) && std::mem::discriminant(a) == std::mem::discriminant(b) {
            return Some(a.clone());
        }
        match (a, b) {
            (Ty::Int, Ty::Float) | (Ty::Float, Ty::Int) => Some(Ty::Float),
            (Ty::Bool, Ty::Int) | (Ty::Int, Ty::Bool) => Some(Ty::Int),
            (Ty::NoneT, t) | (t, Ty::NoneT) => Some(Ty::optional(t.clone())),
            (Ty::Never, t) | (t, Ty::Never) => Some(t.clone()),
            (Ty::Optional(i), t) | (t, Ty::Optional(i)) if self.assignable(t, i) => Some(Ty::Optional(i.clone())),
            (Ty::Class(x), Ty::Class(y)) => {
                let cb = self.common_base(&[*x, *y]);
                if self.is_subclass(*x, cb) && self.is_subclass(*y, cb) {
                    Some(Ty::Class(cb))
                } else {
                    None
                }
            }
            (Ty::Any, _) | (_, Ty::Any) => Some(Ty::Any),
            _ => None,
        }
    }

    fn compare(&mut self, left: &Expr, ops: &[CmpOp], comps: &[Expr]) -> R<CE> {
        if ops.len() == 1 {
            let l = self.expr(left, None)?;
            let rw = match (&l.ty, ops[0]) {
                (Ty::Int, _) | (Ty::Float, _) => None,
                (t, CmpOp::In | CmpOp::NotIn) => match t {
                    _ => None,
                },
                (t, _) => Some(t.clone()),
            };
            let r = self.expr(&comps[0], rw.as_ref())?;
            let c = self.compare_one(l, ops[0], r, comps[0].span)?;
            return Ok(CE::new(Ty::Bool, c));
        }
        // chained: a < b < c  ->  ({ ta = a; tb = b; (ta < tb) && ({ tc = c; tb < tc; }) })
        let l = self.expr(left, None)?;
        let lt = self.fresh("c");
        let lct = self.ctype(&l.ty);
        let mut code = format!("({{ {} {} = {}; ", lct, lt, l.c);
        let mut prev = CE::new(l.ty.clone(), lt);
        let mut conds = Vec::new();
        for (op, ce) in ops.iter().zip(comps) {
            let r = self.expr(ce, None)?;
            let rt = self.fresh("c");
            let rct = self.ctype(&r.ty);
            code.push_str(&format!("{} {} = {}; ", rct, rt, r.c));
            let cur = CE::new(r.ty.clone(), rt);
            conds.push(self.compare_one(prev, *op, cur.clone(), ce.span)?);
            prev = cur;
        }
        // NB: Python short-circuits evaluation of later operands; we evaluate all (no side effects expected).
        code.push_str(&format!("{}; }})", conds.join(" && ")));
        Ok(CE::new(Ty::Bool, code))
    }

    pub fn compare_one(&mut self, l: CE, op: CmpOp, r: CE, span: Span) -> R<String> {
        let num = |t: &Ty| matches!(t, Ty::Int | Ty::Bool | Ty::Float);
        match op {
            CmpOp::In | CmpOp::NotIn => {
                let c = self.contains(&r, &l, span)?;
                return Ok(if op == CmpOp::NotIn { format!("(!{})", c) } else { c });
            }
            CmpOp::Is | CmpOp::IsNot => {
                let eq = if l.ty.is_scalar() && r.ty.is_scalar() {
                    format!("(({}) == ({}))", l.c, r.c)
                } else if l.ty.is_scalar() || r.ty.is_scalar() {
                    if l.ty == Ty::NoneT || r.ty == Ty::NoneT {
                        "false".into()
                    } else {
                        return Err(self.e(span, "'is' comparison with a number; use =="));
                    }
                } else {
                    format!("(((const void*)({})) == ((const void*)({})))", l.c, r.c)
                };
                return Ok(if op == CmpOp::IsNot { format!("(!{})", eq) } else { eq });
            }
            _ => {}
        }
        let sym = match op {
            CmpOp::Eq => "==",
            CmpOp::NotEq => "!=",
            CmpOp::Lt => "<",
            CmpOp::LtE => "<=",
            CmpOp::Gt => ">",
            CmpOp::GtE => ">=",
            _ => unreachable!(),
        };
        let is_eq = matches!(op, CmpOp::Eq | CmpOp::NotEq);
        let neg = op == CmpOp::NotEq;
        let wrap = |c: String| if neg { format!("(!{})", c) } else { c };
        if num(&l.ty) && num(&r.ty) {
            if l.ty == Ty::Float || r.ty == Ty::Float {
                let a = self.coerce(&l.c, &l.ty, &Ty::Float);
                let b = self.coerce(&r.c, &r.ty, &Ty::Float);
                return Ok(format!("(({}) {} ({}))", a, sym, b));
            }
            return Ok(format!("(({}) {} ({}))", l.c, sym, r.c));
        }
        match (&l.ty, &r.ty) {
            (Ty::Str, Ty::Str) => {
                return Ok(if is_eq { wrap(format!("fpy_str_eq({}, {})", l.c, r.c)) } else { format!("(fpy_str_cmp({}, {}) {} 0)", l.c, r.c, sym) });
            }
            (Ty::Bytes, Ty::Bytes) if is_eq => return Ok(wrap(format!("fpy_bytes_eq({}, {})", l.c, r.c))),
            (Ty::Class(c), _) if is_eq && self.find_method(*c, "__eq__").is_some() => {
                let f = self.find_method(*c, "__eq__").unwrap();
                let call = self.call_method_with_ces(l, f, vec![r], span)?;
                return Ok(wrap(call.c));
            }
            (Ty::Class(c), Ty::Class(_)) if !is_eq => {
                let (name, swap) = match op {
                    CmpOp::Lt => ("__lt__", false),
                    CmpOp::Gt => ("__gt__", false),
                    CmpOp::LtE => ("__le__", false),
                    CmpOp::GtE => ("__ge__", false),
                    _ => unreachable!(),
                };
                if let Some(f) = self.find_method(*c, name) {
                    return Ok(self.call_method_with_ces(l, f, vec![r], span)?.c);
                }
                // derive from __lt__
                let rc = if let Ty::Class(rc) = &r.ty { *rc } else { unreachable!() };
                let _ = swap;
                match op {
                    CmpOp::Gt => {
                        if let Some(f) = self.find_method(rc, "__lt__") {
                            return Ok(self.call_method_with_ces(r, f, vec![l], span)?.c);
                        }
                    }
                    CmpOp::GtE => {
                        if let Some(f) = self.find_method(*c, "__lt__") {
                            return Ok(format!("(!{})", self.call_method_with_ces(l, f, vec![r], span)?.c));
                        }
                    }
                    CmpOp::LtE => {
                        if let Some(f) = self.find_method(rc, "__lt__") {
                            return Ok(format!("(!{})", self.call_method_with_ces(r, f, vec![l], span)?.c));
                        }
                    }
                    _ => {}
                }
                return Err(self.e(span, format!("'{}' not supported between {} and {}", sym, self.tyname(&l.ty), self.tyname(&r.ty))));
            }
            (Ty::Class(_), Ty::Class(_)) | (Ty::Class(_), Ty::NoneT) | (Ty::NoneT, Ty::Class(_)) | (Ty::Optional(_), Ty::NoneT) if is_eq && !l.ty.is_boxed() => {
                return Ok(wrap(format!("(((const void*)({})) == ((const void*)({})))", l.c, r.c)));
            }
            _ => {}
        }
        // generic: box both and use runtime protocols (lists, tuples, dicts, optionals, Any)
        let comparable = l.ty == r.ty || self.assignable(&l.ty, &r.ty) || self.assignable(&r.ty, &l.ty) || l.ty == Ty::NoneT || r.ty == Ty::NoneT;
        if !comparable {
            if is_eq {
                return Ok(if neg { "true".into() } else { "false".into() });
            }
            return Err(self.e(span, format!("'{}' not supported between {} and {}", sym, self.tyname(&l.ty), self.tyname(&r.ty))));
        }
        let a = self.coerce(&l.c, &l.ty, &Ty::Any);
        let b = self.coerce(&r.c, &r.ty, &Ty::Any);
        Ok(if is_eq {
            wrap(format!("fpy_obj_eq({}, {})", a, b))
        } else {
            format!("(fpy_obj_cmp({}, {}) {} 0)", a, b, sym)
        })
    }

    fn contains(&mut self, container: &CE, item: &CE, span: Span) -> R<String> {
        match &container.ty {
            Ty::List(e) | Ty::Set(e) => {
                let ic = self.check_coerce(item, e, span)?;
                let f = if matches!(container.ty, Ty::List(_)) { "fpy_list_contains" } else { "fpy_set_contains" };
                Ok(format!("{}({}, {})", f, container.c, e.to_word(&ic)))
            }
            Ty::Dict(k, _) => {
                let ic = self.check_coerce(item, k, span)?;
                Ok(format!("fpy_dict_contains({}, {})", container.c, k.to_word(&ic)))
            }
            Ty::Str => {
                let ic = self.check_coerce(item, &Ty::Str, span)?;
                Ok(format!("fpy_str_contains({}, {})", container.c, ic))
            }
            Ty::Tuple(ts) => {
                let t = self.fresh("tc");
                let it = self.fresh("ti");
                let ict = self.ctype(&item.ty);
                let mut conds = Vec::new();
                for (i, et) in ts.iter().enumerate() {
                    let ect = self.ctype(et);
                    let ev = CE::new(et.clone(), et.from_word(&format!("{}->items[{}]", t, i), &ect));
                    conds.push(self.compare_one(CE::new(item.ty.clone(), it.clone()), CmpOp::Eq, ev, span)?);
                }
                if conds.is_empty() {
                    conds.push("false".into());
                }
                Ok(format!("({{ FpyTuple *{} = {}; {} {} = {}; {}; }})", t, container.c, ict, it, item.c, conds.join(" || ")))
            }
            Ty::Class(c) => {
                let f = self.find_method(*c, "__contains__").ok_or_else(|| self.e(span, format!("'{}' does not support 'in'", self.classes[*c].name)))?;
                Ok(self.call_method_with_ces(container.clone(), f, vec![item.clone()], span)?.c)
            }
            Ty::Any => {
                let ic = self.coerce(&item.c, &item.ty, &Ty::Any);
                Ok(format!("fpy_dyn_contains({}, {})", container.c, ic))
            }
            Ty::Bytes => match &item.ty {
                Ty::Int | Ty::Bool => Ok(format!("fpy_bytes_contains_byte({}, {})", container.c, item.c)),
                _ => {
                    let ic = self.check_coerce(item, &Ty::Bytes, span)?;
                    Ok(format!("fpy_bytes_contains({}, {})", container.c, ic))
                }
            },
            Ty::VarTuple(e) => {
                let ic = self.check_coerce(item, e, span)?;
                Ok(format!("fpy_tuple_contains({}, {}, {})", container.c, e.to_word(&ic), e.kind()))
            }
            // like Python, `in` on an iterator (e.g. range()) consumes it up to the match
            Ty::Iter(e) => {
                let ic = self.check_coerce(item, e, span)?;
                Ok(format!("fpy_iter_contains({}, {}, {})", container.c, e.kind(), e.to_word(&ic)))
            }
            t => Err(self.e(span, format!("argument of type {} is not a container", self.tyname(t)))),
        }
    }

    // ------------------------------------------------------------------ attributes & subscripts

    fn attribute(&mut self, value: &Expr, attr: &str, span: Span) -> R<CE> {
        // type(x).__name__ / x.__class__.__name__: runtime class name
        if attr == "__name__" {
            let inner = match &value.kind {
                ExprKind::Call { func, args, .. } if args.len() == 1 && matches!(&func.kind, ExprKind::Name(n) if n == "type" && self.is_builtin_name(n)) => Some(&args[0]),
                ExprKind::Attribute { value: v, attr: a } if a == "__class__" => Some(&**v),
                _ => None,
            };
            if let Some(x) = inner {
                let v = self.expr(x, None)?;
                let c = match &v.ty {
                    Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Bytes | Ty::NoneT => {
                        let n = match &v.ty {
                            Ty::Int => "int",
                            Ty::Float => "float",
                            Ty::Bool => "bool",
                            Ty::Str => "str",
                            Ty::Bytes => "bytes",
                            _ => "NoneType",
                        };
                        format!("((void)({}), {})", v.c, self.string_lit(n))
                    }
                    _ => format!("fpy_str_from_cstr(fpy_type_name((FpyObj*)({})))", v.c),
                };
                return Ok(CE::new(Ty::Str, c));
            }
        }
        // module attribute
        if let ExprKind::Name(n) = &value.kind {
            if self.is_module_name(n) {
                let mid = match self.lookup_sym(self.cur_mod(), n) {
                    Some(Sym::Module(mid)) => mid,
                    _ => return self.virtual_module_attr(n, attr, span),
                };
                return self.module_attr(mid, attr, span);
            }
        }
        if let ExprKind::Attribute { .. } = &value.kind {
            if let Some(mid) = self.module_path(value) {
                return self.module_attr(mid, attr, span);
            }
        }
        let obj = self.expr(value, None)?;
        self.attr_of(obj, attr, span)
    }

    pub fn is_module_name(&self, n: &str) -> bool {
        if !self.is_builtin_name(n) && !matches!(self.modules[self.cur_mod()].scope.get(n), Some(Sym::Module(_))) {
            return false;
        }
        matches!(self.lookup_sym(self.cur_mod(), n), Some(Sym::Module(_))) || matches!(self.lookup_sym(self.cur_mod(), n), Some(Sym::Builtin(b)) if b.starts_with("module:"))
    }

    /// Resolve `a.b.c` chains naming a (sub)module.
    pub fn module_path(&self, e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::Name(n) => match self.lookup_sym(self.cur_mod(), n) {
                Some(Sym::Module(m)) if self.is_builtin_name(n) || matches!(self.modules[self.cur_mod()].scope.get(n), Some(Sym::Module(_))) => Some(m),
                _ => None,
            },
            ExprKind::Attribute { value, attr } => {
                let m = self.module_path(value)?;
                match self.modules[m].scope.get(attr) {
                    Some(Sym::Module(s)) => Some(*s),
                    _ => {
                        let full = format!("{}.{}", self.modules[m].name, attr);
                        self.mod_index.get(&full).copied()
                    }
                }
            }
            _ => None,
        }
    }

    fn virtual_module_attr(&mut self, n: &str, attr: &str, span: Span) -> R<CE> {
        Err(self.e(span, format!("'{}.{}' is not available as a value", n, attr)))
    }

    pub fn module_attr(&mut self, mid: usize, attr: &str, span: Span) -> R<CE> {
        match self.modules[mid].scope.get(attr).cloned() {
            Some(Sym::Global(g)) => {
                let ty = self.globals[g].ty.clone().ok_or_else(|| self.e(span, format!("type of '{}.{}' is unknown", self.modules[mid].name, attr)))?;
                Ok(CE::new(ty, format!("G.{}", self.globals[g].cname)))
            }
            Some(Sym::Func(f)) => self.func_value(f, span),
            Some(Sym::Class(c)) => Ok(CE::new(Ty::Type(c), format!("fpy_class_obj(&T_{})", self.classes[c].cname))),
            Some(Sym::Module(_)) => Err(self.e(span, "module cannot be used as a value")),
            _ => {
                let full = format!("{}.{}", self.modules[mid].name, attr);
                if self.mod_index.contains_key(&full) {
                    return Err(self.e(span, "module cannot be used as a value"));
                }
                Err(self.e(span, format!("module '{}' has no attribute '{}'", self.modules[mid].name, attr)))
            }
        }
    }

    pub fn attr_of(&mut self, obj: CE, attr: &str, span: Span) -> R<CE> {
        match obj.ty.clone() {
            Ty::Class(c) => {
                if let Some(f) = self.find_field(c, attr) {
                    return Ok(CE::new(f.ty.clone(), format!("({})->f_{}", obj.c, c_ident(attr))));
                }
                if let Some(m) = self.find_method(c, attr) {
                    if self.funcs[m].kind == FnKind::Property {
                        return self.call_method_ce(obj, m, vec![], None, span);
                    }
                    return self.bound_method_value(obj, m, span);
                }
                if let Some(g) = self.find_class_var(c, attr) {
                    let ty = self.globals[g].ty.clone().ok_or_else(|| self.e(span, format!("type of class variable '{}' is unknown", attr)))?;
                    return Ok(CE::new(ty, format!("G.{}", self.globals[g].cname)));
                }
                if self.classes[c].is_enum {
                    // members accessed through an instance
                }
                Err(self.e(span, format!("'{}' object has no attribute '{}'", self.classes[c].name, attr)))
            }
            Ty::Type(c) => {
                if let Some(g) = self.find_class_var(c, attr) {
                    let ty = self.globals[g].ty.clone().ok_or_else(|| self.e(span, format!("type of class variable '{}' is unknown", attr)))?;
                    return Ok(CE::new(ty, format!("G.{}", self.globals[g].cname)));
                }
                if attr == "__name__" {
                    let n = self.classes[c].name.clone();
                    return Ok(CE::new(Ty::Str, self.string_lit(&n)));
                }
                if let Some(m) = self.find_method(c, attr) {
                    if self.funcs[m].kind == FnKind::Static {
                        return self.func_value(m, span);
                    }
                }
                // `C.x` for an instance field declared `x: T = v` reads the class-level default
                if let Some(Field { ty, default: Some(d), owner, .. }) = self.find_field(c, attr) {
                    let saved = self.fxr().module;
                    self.fx().module = self.classes[owner].module;
                    let r = self.expr(&d, Some(&ty)).and_then(|ce| self.check_coerce(&ce, &ty, span));
                    self.fx().module = saved;
                    return Ok(CE::new(ty, r?));
                }
                Err(self.e(span, format!("class '{}' has no attribute '{}'", self.classes[c].name, attr)))
            }
            Ty::Any => Ok(CE::new(Ty::Any, format!("fpy_dyn_getattr({}, {})", obj.c, c_string(attr.as_bytes())))),
            Ty::Float if attr == "real" => Ok(obj),
            Ty::Float if attr == "imag" => Ok(CE::new(Ty::Float, "0.0")),
            Ty::Int if attr == "real" || attr == "numerator" => Ok(obj),
            Ty::Int if attr == "denominator" => Ok(CE::new(Ty::Int, "INT64_C(1)")),
            Ty::Optional(i) => Err(self.e(span, format!("'{}' may be None; check `is not None` before accessing '.{}'", self.tyname(&Ty::Optional(i.clone())), attr))),
            t => Err(self.e(span, format!("'{}' has no attribute '{}' (methods must be called)", self.tyname(&t), attr))),
        }
    }

    fn bound_method_value(&mut self, obj: CE, m: usize, span: Span) -> R<CE> {
        let sig = self.func_sig(m)?;
        if sig.vararg.is_some() || sig.kwarg.is_some() || !self.funcs[m].def.type_params.is_empty() {
            return Err(self.e(span, "this method cannot be used as a value"));
        }
        let target = self.func_instance(m, vec![])?;
        let adapter = format!("BM_{}", target);
        if !self.value_adapters.contains_key(&adapter) {
            self.value_adapters.insert(adapter.clone(), adapter.clone());
            let ret_c = if sig.ret == Ty::NoneT { "void".to_string() } else { self.ctype(&sig.ret) };
            let owner = self.funcs[m].owner.unwrap();
            let oc = self.classes[owner].cname.clone();
            let mut ps = vec!["void *env".to_string()];
            let mut args = vec![format!("(struct {}*)env", oc)];
            for (i, p) in sig.params.iter().enumerate().skip(1) {
                ps.push(format!("{} a{}", self.ctype(&p.ty), i));
                args.push(format!("a{}", i));
            }
            let ret = if ret_c == "void" { "" } else { "return " };
            self.out.protos.push_str(&format!("static {} {}({});\n", ret_c, adapter, ps.join(", ")));
            self.out.funcs.push_str(&format!("static {} {}({}) {{ {}{}({}); }}\n", ret_c, adapter, ps.join(", "), ret, target, args.join(", ")));
        }
        let ty = Ty::Func(sig.params.iter().skip(1).map(|p| p.ty.clone()).collect(), Box::new(sig.ret.clone()));
        let name = c_string(self.funcs[m].qualname.as_bytes());
        Ok(CE::new(ty, format!("fpy_func_new((void*){}, (void*)({}), NULL, {})", adapter, obj.c, name)))
    }

    fn subscript(&mut self, value: &Expr, index: &Expr, span: Span) -> R<CE> {
        let obj = self.expr(value, None)?;
        if let ExprKind::Slice { lower, upper, step } = &index.kind {
            let (hl, l) = self.opt_int(lower.as_deref())?;
            let (hu, u) = self.opt_int(upper.as_deref())?;
            let st = match step {
                Some(s) => {
                    let ce = self.expr(s, Some(&Ty::Int))?;
                    self.check_coerce(&ce, &Ty::Int, s.span)?
                }
                None => "1".into(),
            };
            return match &obj.ty {
                Ty::List(_) => Ok(CE::new(obj.ty.clone(), format!("fpy_list_slice({}, {}, {}, {}, {}, {})", obj.c, hl, l, hu, u, st))),
                Ty::Str => Ok(CE::new(Ty::Str, format!("fpy_str_slice({}, {}, {}, {}, {}, {})", obj.c, hl, l, hu, u, st))),
                Ty::Bytes => Ok(CE::new(Ty::Bytes, format!("fpy_bytes_slice({}, {}, {}, {}, {}, {})", obj.c, hl, l, hu, u, st))),
                Ty::VarTuple(_) => Ok(CE::new(obj.ty.clone(), format!("fpy_tuple_slice({}, {}, {}, {}, {}, {})", obj.c, hl, l, hu, u, st))),
                Ty::Tuple(ts) if ts.iter().all(|t| *t == ts[0]) && !ts.is_empty() && (step.is_some() || !matches!(lower.as_deref().map(|x| &x.kind), None | Some(ExprKind::Int(_)) | Some(ExprKind::UnaryOp { .. })) || !matches!(upper.as_deref().map(|x| &x.kind), None | Some(ExprKind::Int(_)) | Some(ExprKind::UnaryOp { .. }))) => {
                    // a dynamic slice of a homogeneous tuple is a tuple[T, ...]
                    Ok(CE::new(Ty::VarTuple(Box::new(ts[0].clone())), format!("fpy_tuple_slice({}, {}, {}, {}, {}, {})", obj.c, hl, l, hu, u, st)))
                }
                Ty::Tuple(ts) => {
                    // static slice of a tuple with literal bounds
                    let lit = |e: &Option<Box<Expr>>| -> Option<i64> {
                        match e.as_deref().map(|x| &x.kind) {
                            None => None,
                            Some(ExprKind::Int(v)) => Some(*v),
                            Some(ExprKind::UnaryOp { op: UnOp::Neg, operand }) => match operand.kind {
                                ExprKind::Int(v) => Some(-v),
                                _ => Some(i64::MAX),
                            },
                            _ => Some(i64::MAX),
                        }
                    };
                    let n = ts.len() as i64;
                    let norm = |v: i64| if v < 0 { (v + n).max(0) } else { v.min(n) };
                    let (a, b) = (lit(lower), lit(upper));
                    if a == Some(i64::MAX) || b == Some(i64::MAX) || step.is_some() {
                        return Err(self.e(span, "tuple slices need literal bounds"));
                    }
                    let a = norm(a.unwrap_or(0)) as usize;
                    let b = norm(b.unwrap_or(n)) as usize;
                    let sub: Vec<Ty> = if a < b { ts[a..b].to_vec() } else { vec![] };
                    let kinds: Vec<u8> = sub.iter().map(|t| t.kind()).collect();
                    let karr = self.kinds_array(&kinds);
                    Ok(CE::new(
                        Ty::Tuple(sub.clone()),
                        format!("({{ FpyTuple *_s = {}; FpyTuple *_t = fpy_tuple_new({}, {}); memcpy(_t->items, _s->items + {}, {} * 8); _t; }})", obj.c, sub.len(), karr, a, sub.len()),
                    ))
                }
                Ty::Class(c) => {
                    let f = self.find_method(*c, "__getitem__").ok_or_else(|| self.e(span, "object is not subscriptable"))?;
                    self.call_method_ce(obj.clone(), f, vec![index.clone()], None, span)
                }
                t => Err(self.e(span, format!("{} cannot be sliced", self.tyname(t)))),
            };
        }
        match obj.ty.clone() {
            Ty::List(e) => {
                let i = self.expr(index, Some(&Ty::Int))?;
                let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                let ct = self.ctype(&e);
                Ok(CE::new((*e).clone(), e.from_word(&format!("fpy_list_get({}, {})", obj.c, ic), &ct)))
            }
            Ty::Str => {
                let i = self.expr(index, Some(&Ty::Int))?;
                let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                Ok(CE::new(Ty::Str, format!("fpy_str_getitem({}, {})", obj.c, ic)))
            }
            Ty::Bytes => {
                let i = self.expr(index, Some(&Ty::Int))?;
                let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                Ok(CE::new(Ty::Int, format!("fpy_bytes_getitem({}, {})", obj.c, ic)))
            }
            Ty::Dict(k, v) => {
                let kc = self.expr(index, Some(&k))?;
                let kc = self.check_coerce(&kc, &k, index.span)?;
                let ct = self.ctype(&v);
                Ok(CE::new((*v).clone(), v.from_word(&format!("fpy_dict_get({}, {})", obj.c, k.to_word(&kc)), &ct)))
            }
            Ty::VarTuple(e) => {
                let i = self.expr(index, Some(&Ty::Int))?;
                let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                let ct = self.ctype(&e);
                Ok(CE::new((*e).clone(), e.from_word(&format!("fpy_tuple_get({}, {})", obj.c, ic), &ct)))
            }
            Ty::Tuple(ts) => {
                let idx = match &index.kind {
                    ExprKind::Int(i) => *i,
                    ExprKind::UnaryOp { op: UnOp::Neg, operand } => match operand.kind {
                        ExprKind::Int(i) => -i,
                        _ => i64::MAX,
                    },
                    _ => i64::MAX,
                };
                if idx == i64::MAX {
                    if !ts.is_empty() && ts.iter().all(|t| t == &ts[0]) {
                        let i = self.expr(index, Some(&Ty::Int))?;
                        let ic = self.check_coerce(&i, &Ty::Int, index.span)?;
                        let ct = self.ctype(&ts[0]);
                        let t0 = ts[0].clone();
                        return Ok(CE::new(t0.clone(), t0.from_word(&format!("({{ FpyTuple *_t = {}; _t->items[fpy_norm_index({}, _t->len, \"tuple\")]; }})", obj.c, ic), &ct)));
                    }
                    return Err(self.e(index.span, "heterogeneous tuples must be indexed with an integer literal"));
                }
                let n = ts.len() as i64;
                let i = if idx < 0 { idx + n } else { idx };
                if i < 0 || i >= n {
                    return Err(self.e(index.span, "tuple index out of range"));
                }
                let t = ts[i as usize].clone();
                let ct = self.ctype(&t);
                Ok(CE::new(t.clone(), t.from_word(&format!("({})->items[{}]", obj.c, i), &ct)))
            }
            Ty::Class(c) => {
                let f = self.find_method(c, "__getitem__").ok_or_else(|| self.e(span, format!("'{}' object is not subscriptable", self.classes[c].name)))?;
                self.call_method_ce(obj, f, vec![index.clone()], None, span)
            }
            Ty::Any => {
                let k = self.expr(index, None)?;
                let kc = self.coerce(&k.c, &k.ty, &Ty::Any);
                Ok(CE::new(Ty::Any, format!("fpy_dyn_getitem({}, {})", obj.c, kc)))
            }
            Ty::Optional(_) => Err(self.e(span, "value may be None; check `is not None` before indexing")),
            t => Err(self.e(span, format!("'{}' object is not subscriptable", self.tyname(&t)))),
        }
    }

    // ------------------------------------------------------------------ literals

    fn list_lit(&mut self, elts: &[Expr], want: Option<&Ty>, span: Span) -> R<CE> {
        let want_e = match want {
            Some(Ty::List(e)) => Some((**e).clone()),
            Some(Ty::Optional(i)) => match &**i {
                Ty::List(e) => Some((**e).clone()),
                _ => None,
            },
            Some(Ty::Iter(e)) => Some((**e).clone()),
            _ => None,
        };
        if elts.is_empty() {
            return Ok(match want_e {
                Some(e) => CE::new(Ty::list(e.clone()), format!("fpy_list_new({}, 4)", e.kind())),
                None => CE::new(Ty::list(Ty::Var("?".into())), "fpy_list_new(FK_OBJ, 4)"),
            });
        }
        let (et, items) = self.elements(elts, want_e, span)?;
        let tmp = self.fresh("l");
        let mut code = format!("({{ FpyList *{} = fpy_list_new({}, {}); ", tmp, et.kind(), elts.len());
        for it in items {
            match it {
                Elem::One(c) => code.push_str(&format!("fpy_list_append({}, {}); ", tmp, et.to_word(&c))),
                Elem::Splat(ce) => code.push_str(&self.splat_into(&tmp, &ce, &et, span, "fpy_list_append")?),
            }
        }
        code.push_str(&format!("{}; }})", tmp));
        Ok(CE::new(Ty::list(et), code))
    }

    fn splat_into(&mut self, target: &str, ce: &CE, et: &Ty, span: Span, add: &str) -> R<String> {
        match &ce.ty {
            Ty::List(e) if **e == *et && add == "fpy_list_append" => Ok(format!("fpy_list_extend({}, {}); ", target, ce.c)),
            t => {
                let el = self.elem_type(t).ok_or_else(|| self.e(span, "cannot unpack non-iterable"))?;
                if el != *et {
                    return Err(self.e(span, format!("cannot unpack {} into a collection of {}", self.tyname(t), self.tyname(et))));
                }
                let ic = match t {
                    Ty::Any => format!("fpy_dyn_iter({})", ce.c),
                    _ => self.coerce(&ce.c, t, &Ty::iter(el)),
                };
                Ok(format!("{{ FpyIter *_si = {}; fpy_word _sw; while (_si->next(_si, &_sw)) {}({}, _sw); }} ", ic, add, target))
            }
        }
    }

    fn elements(&mut self, elts: &[Expr], want_e: Option<Ty>, span: Span) -> R<(Ty, Vec<Elem>)> {
        let mut ces = Vec::new();
        for e in elts {
            match &e.kind {
                ExprKind::Starred(inner) => {
                    let w = want_e.clone().map(Ty::list);
                    let ce = self.expr(inner, w.as_ref())?;
                    ces.push((true, ce, e.span));
                }
                _ => {
                    let ce = self.expr(e, want_e.as_ref())?;
                    ces.push((false, ce, e.span));
                }
            }
        }
        let et = match want_e {
            Some(t) => t,
            None => {
                let mut t: Option<Ty> = None;
                for (star, ce, sp) in &ces {
                    let ct = if *star { self.elem_type(&ce.ty).ok_or_else(|| self.e(*sp, "cannot unpack non-iterable"))? } else { ce.ty.clone() };
                    t = Some(match t {
                        None => ct,
                        Some(prev) => match self.unify(&prev, &ct) {
                            Some(u) => u,
                            // heterogeneous literal: infer a union type (elements need narrowing to use)
                            None => make_union(vec![prev, ct]),
                        },
                    });
                    let _ = sp;
                }
                t.unwrap_or(Ty::Var("?".into()))
            }
        };
        let _ = span;
        let mut out = Vec::new();
        for (star, ce, sp) in ces {
            if star {
                out.push(Elem::Splat(ce));
            } else {
                let c = self.check_coerce(&ce, &et, sp)?;
                out.push(Elem::One(c));
            }
        }
        Ok((et, out))
    }

    fn tuple_lit(&mut self, elts: &[Expr], want: Option<&Ty>, span: Span) -> R<CE> {
        if elts.iter().any(|e| matches!(e.kind, ExprKind::Starred(_))) {
            return Err(self.e(span, "starred elements in tuple literals are not supported"));
        }
        let wants: Vec<Option<Ty>> = match want {
            Some(Ty::Tuple(ts)) if ts.len() == elts.len() => ts.iter().cloned().map(Some).collect(),
            _ => vec![None; elts.len()],
        };
        let mut tys = Vec::new();
        let mut cs = Vec::new();
        for (e, w) in elts.iter().zip(wants.iter()) {
            let ce = self.expr(e, w.as_ref())?;
            let t = match w {
                Some(w) if self.assignable(&ce.ty, w) || has_unknown(&ce.ty) => w.clone(),
                _ => ce.ty.clone(),
            };
            let c = self.check_coerce(&ce, &t, e.span)?;
            tys.push(t);
            cs.push(c);
        }
        let kinds: Vec<u8> = tys.iter().map(|t| t.kind()).collect();
        let karr = self.kinds_array(&kinds);
        let tmp = self.fresh("t");
        let mut code = format!("({{ FpyTuple *{} = fpy_tuple_new({}, {}); ", tmp, elts.len(), karr);
        for (i, (c, t)) in cs.iter().zip(&tys).enumerate() {
            code.push_str(&format!("{}->items[{}] = {}; ", tmp, i, t.to_word(c)));
        }
        code.push_str(&format!("{}; }})", tmp));
        Ok(CE::new(Ty::Tuple(tys), code))
    }

    fn set_lit(&mut self, elts: &[Expr], want: Option<&Ty>, span: Span) -> R<CE> {
        let want_e = match want {
            Some(Ty::Set(e)) => Some((**e).clone()),
            _ => None,
        };
        let (et, items) = self.elements(elts, want_e, span)?;
        let tmp = self.fresh("s");
        let mut code = format!("({{ FpyDict *{} = fpy_set_new({}); ", tmp, et.kind());
        for it in items {
            match it {
                Elem::One(c) => code.push_str(&format!("fpy_set_add({}, {}); ", tmp, et.to_word(&c))),
                Elem::Splat(ce) => code.push_str(&self.splat_into(&tmp, &ce, &et, span, "fpy_set_add")?),
            }
        }
        code.push_str(&format!("{}; }})", tmp));
        Ok(CE::new(Ty::set(et), code))
    }

    fn dict_lit(&mut self, items: &[(Option<Expr>, Expr)], want: Option<&Ty>, span: Span) -> R<CE> {
        let (wk, wv) = match want {
            Some(Ty::Dict(k, v)) => (Some((**k).clone()), Some((**v).clone())),
            Some(Ty::Optional(i)) => match &**i {
                Ty::Dict(k, v) => (Some((**k).clone()), Some((**v).clone())),
                _ => (None, None),
            },
            _ => (None, None),
        };
        if items.is_empty() {
            return Ok(match (wk, wv) {
                (Some(k), Some(v)) => CE::new(Ty::dict(k.clone(), v.clone()), format!("fpy_dict_new({}, {})", k.kind(), v.kind())),
                _ => CE::new(Ty::dict(Ty::Var("?".into()), Ty::Var("?".into())), "fpy_dict_new(FK_OBJ, FK_OBJ)"),
            });
        }
        let mut entries = Vec::new();
        let mut kt = wk.clone();
        let mut vt = wv.clone();
        for (k, v) in items {
            match k {
                Some(k) => {
                    let kc = self.expr(k, kt.as_ref())?;
                    let vc = self.expr(v, vt.as_ref())?;
                    kt = Some(match kt {
                        None => kc.ty.clone(),
                        Some(t) => {
                            if wk.is_some() {
                                t
                            } else {
                                self.unify(&t, &kc.ty).ok_or_else(|| self.e(k.span, "dict keys have mixed types; annotate the variable"))?
                            }
                        }
                    });
                    vt = Some(match vt {
                        None => vc.ty.clone(),
                        Some(t) => {
                            if wv.is_some() {
                                t
                            } else {
                                self.unify(&t, &vc.ty).unwrap_or_else(|| make_union(vec![t.clone(), vc.ty.clone()]))
                            }
                        }
                    });
                    entries.push((Some((kc, k.span)), vc, v.span));
                }
                None => {
                    let w = match (&kt, &vt) {
                        (Some(k), Some(v)) => Some(Ty::dict(k.clone(), v.clone())),
                        _ => None,
                    };
                    let dc = self.expr(v, w.as_ref())?;
                    if let Ty::Dict(k2, v2) = &dc.ty {
                        if kt.is_none() {
                            kt = Some((**k2).clone());
                            vt = Some((**v2).clone());
                        }
                    }
                    entries.push((None, dc, v.span));
                }
            }
        }
        let kt = kt.unwrap();
        let vt = vt.unwrap();
        let tmp = self.fresh("d");
        let mut code = format!("({{ FpyDict *{} = fpy_dict_new({}, {}); ", tmp, kt.kind(), vt.kind());
        let dty = Ty::dict(kt.clone(), vt.clone());
        for (k, v, vs) in entries {
            match k {
                Some((kc, ks)) => {
                    let kcc = self.check_coerce(&kc, &kt, ks)?;
                    let vcc = self.check_coerce(&v, &vt, vs)?;
                    code.push_str(&format!("fpy_dict_set({}, {}, {}); ", tmp, kt.to_word(&kcc), vt.to_word(&vcc)));
                }
                None => {
                    let dc = self.check_coerce(&v, &dty, vs)?;
                    code.push_str(&format!("fpy_dict_update({}, {}); ", tmp, dc));
                }
            }
        }
        let _ = span;
        code.push_str(&format!("{}; }})", tmp));
        Ok(CE::new(dty, code))
    }

    // ------------------------------------------------------------------ comprehensions

    pub fn comprehension(&mut self, kind: CompKind, elt: &Expr, value: Option<&Expr>, gens: &[Comprehension], want: Option<&Ty>, span: Span) -> R<CE> {
        // Collect comprehension-scoped variable names.
        let mut names = BTreeSet::new();
        for g in gens {
            scope::collect_target_names(&g.target, &mut names);
        }
        let (want_e, want_v) = match (kind, want) {
            (CompKind::List, Some(Ty::List(e))) | (CompKind::Set, Some(Ty::Set(e))) => (Some((**e).clone()), None),
            (CompKind::Dict, Some(Ty::Dict(k, v))) => (Some((**k).clone()), Some((**v).clone())),
            _ => (None, None),
        };
        let saved_body = std::mem::take(&mut self.fx().body);
        let saved_indent = self.fxr().indent;
        self.fx().indent = 1;
        self.fx().comp.push(HashMap::new());
        let res_var = self.fresh("cr");
        let mut result_ty: Option<(Ty, Option<Ty>)> = None;
        let names_v: Vec<String> = names.into_iter().collect();
        let r = self.comp_level(gens, 0, &names_v, &mut |c: &mut Compiler| {
            let ke = c.expr(elt, want_e.as_ref())?;
            let ve = match value {
                Some(v) => Some(c.expr(v, want_v.as_ref())?),
                None => None,
            };
            let kt = match &want_e {
                Some(w) => w.clone(),
                None => c.infer_var_type(&ke.ty, elt.span, "<comprehension element>")?,
            };
            let vt = match (&ve, &want_v) {
                (Some(_), Some(w)) => Some(w.clone()),
                (Some(v), None) => Some(c.infer_var_type(&v.ty, span, "<comprehension value>")?),
                _ => None,
            };
            if let Some((pk, pv)) = &result_ty {
                if *pk != kt || *pv != vt {
                    return Err(c.e(span, "inconsistent comprehension element types"));
                }
            }
            result_ty = Some((kt.clone(), vt.clone()));
            let kc = c.check_coerce(&ke, &kt, elt.span)?;
            match kind {
                CompKind::List => c.emit(&format!("fpy_list_append({}, {});", res_var, kt.to_word(&kc))),
                CompKind::Set => c.emit(&format!("fpy_set_add({}, {});", res_var, kt.to_word(&kc))),
                CompKind::Dict => {
                    let v = ve.unwrap();
                    let vt = vt.unwrap();
                    let vc = c.check_coerce(&v, &vt, span)?;
                    c.emit(&format!("fpy_dict_set({}, {}, {});", res_var, kt.to_word(&kc), vt.to_word(&vc)));
                }
            }
            Ok(())
        });
        let scope_vars = self.fx().comp.pop().unwrap();
        let body = std::mem::replace(&mut self.fx().body, saved_body);
        self.fx().indent = saved_indent;
        r?;
        let (kt, vt) = result_ty.ok_or_else(|| self.e(span, "empty comprehension"))?;
        let mut decls = String::new();
        for (_, l) in scope_vars.iter() {
            if l.cname.starts_with("cv_") {
                decls.push_str(&format!("{} {} = {}; ", self.ctype(&l.ty), l.cname, l.ty.zero()));
            }
        }
        let (ty, init) = match kind {
            CompKind::List => (Ty::list(kt.clone()), format!("FpyList *{} = fpy_list_new({}, 8);", res_var, kt.kind())),
            CompKind::Set => (Ty::set(kt.clone()), format!("FpyDict *{} = fpy_set_new({});", res_var, kt.kind())),
            CompKind::Dict => {
                let vt = vt.unwrap();
                (Ty::dict(kt.clone(), vt.clone()), format!("FpyDict *{} = fpy_dict_new({}, {});", res_var, kt.kind(), vt.kind()))
            }
        };
        Ok(CE::new(ty, format!("({{ {} {}\n{} {}; }})", init, decls, body, res_var)))
    }

    /// Emit nested loops for comprehension generators; comprehension variables are block-scoped.
    fn comp_level(&mut self, gens: &[Comprehension], i: usize, names: &[String], inner: &mut dyn FnMut(&mut Compiler) -> R<()>) -> R<()> {
        if i == gens.len() {
            return inner(self);
        }
        let g = &gens[i];
        // Declare target names lazily: store_name on a comp-scoped name needs a Local.
        let mut tnames = BTreeSet::new();
        scope::collect_target_names(&g.target, &mut tnames);
        let it_saved = std::mem::take(&mut self.fx().comp.last_mut().unwrap().clone());
        let _ = it_saved;
        // Determine element type to declare targets.
        let iter_ty = {
            let saved = self.fxr().dry_run;
            self.fx().dry_run = true;
            let r = self.expr(&g.iter, None);
            self.fx().dry_run = saved;
            r?.ty
        };
        let et = self.loop_elem_type(&g.iter, &iter_ty).ok_or_else(|| self.e(g.iter.span, format!("{} is not iterable", self.tyname(&iter_ty))))?;
        self.declare_comp_targets(&g.target, &et)?;
        let ifs = g.ifs.clone();
        let rest = &gens[i + 1..];
        let names_v = names.to_vec();
        let gens_all = gens.to_vec();
        self.for_loop(&g.target, &g.iter, None, &mut |c: &mut Compiler| {
            let mut opened = 0;
            for cond in &ifs {
                let (cc, pos) = c.cond(cond)?;
                c.emit(&format!("if ({}) {{", cc));
                c.fx().indent += 1;
                c.fx().narrow.extend(pos);
                opened += 1;
            }
            let _ = rest;
            c.comp_level(&gens_all, i + 1, &names_v, inner)?;
            for _ in 0..opened {
                c.fx().indent -= 1;
                c.emit("}");
            }
            Ok(())
        })
    }

    fn loop_elem_type(&mut self, iter: &Expr, ty: &Ty) -> Option<Ty> {
        if let ExprKind::Call { func, args, .. } = &iter.kind {
            if let ExprKind::Name(n) = &func.kind {
                if self.is_builtin_name(n) {
                    match n.as_str() {
                        "range" => return Some(Ty::Int),
                        "enumerate" if !args.is_empty() => {
                            let saved = self.fxr().dry_run;
                            self.fx().dry_run = true;
                            let r = self.expr(&args[0], None);
                            self.fx().dry_run = saved;
                            let inner = self.elem_type(&r.ok()?.ty)?;
                            return Some(Ty::Tuple(vec![Ty::Int, inner]));
                        }
                        "zip" => {
                            let mut ts = Vec::new();
                            for a in args {
                                let saved = self.fxr().dry_run;
                                self.fx().dry_run = true;
                                let r = self.expr(a, None);
                                self.fx().dry_run = saved;
                                ts.push(self.elem_type(&r.ok()?.ty)?);
                            }
                            return Some(Ty::Tuple(ts));
                        }
                        _ => {}
                    }
                }
            }
            if let ExprKind::Attribute { attr, .. } = &func.kind {
                if attr == "items" {
                    if let Ty::List(e) = ty {
                        return Some((**e).clone());
                    }
                }
            }
        }
        self.elem_type(ty)
    }

    fn declare_comp_targets(&mut self, t: &Expr, et: &Ty) -> R<()> {
        match &t.kind {
            ExprKind::Name(n) => {
                let cname = format!("cv_{}_{}", c_ident(n), self.tmp);
                self.tmp += 1;
                self.fx().comp.last_mut().unwrap().insert(n.clone(), Local { ty: et.clone(), cname, cell: false });
                Ok(())
            }
            ExprKind::Tuple { elts, .. } | ExprKind::List(elts) => {
                let ets: Vec<Ty> = match et {
                    Ty::Tuple(ts) if ts.len() == elts.len() => ts.clone(),
                    Ty::List(e) => vec![(**e).clone(); elts.len()],
                    Ty::Any => vec![Ty::Any; elts.len()],
                    _ => return Err(self.e(t.span, format!("cannot unpack {}", self.tyname(et)))),
                };
                for (e, t2) in elts.iter().zip(ets) {
                    self.declare_comp_targets(e, &t2)?;
                }
                Ok(())
            }
            _ => Err(self.e(t.span, "unsupported comprehension target")),
        }
    }

    /// Generator expression -> anonymous generator closure.
    pub fn genexp(&mut self, elt: &Expr, gens: &[Comprehension], span: Span) -> R<CE> {
        let first = self.expr(&gens[0].iter, None)?;
        let pname = "\u{1}it0".to_string();
        // Build: def <genexpr>(it0): for t in it0: if c: ... yield elt
        let mut body = Stmt { kind: StmtKind::Expr(Expr::new(ExprKind::Yield(Some(Box::new(elt.clone()))), span)), span, end_line: span.line, header_end: span.line };
        for (i, g) in gens.iter().enumerate().rev() {
            for c in g.ifs.iter().rev() {
                body = Stmt { kind: StmtKind::If { test: c.clone(), body: vec![body], orelse: vec![] }, span, end_line: span.line, header_end: span.line };
            }
            let iter = if i == 0 { Expr::new(ExprKind::Name(pname.clone()), span) } else { g.iter.clone() };
            body = Stmt { kind: StmtKind::For { target: g.target.clone(), iter, body: vec![body], orelse: vec![], is_async: false }, span, end_line: span.line, header_end: span.line };
        }
        let params = Params { args: vec![Arg { name: pname.clone(), ann: None, default: None, span }], ..Default::default() };
        let def = FuncDef { name: "<genexpr>".into(), type_params: vec![], params, returns: None, body: vec![body], decorators: vec![], is_async: false, header_lines: (0, 0), close_paren_pos: 0, colon_pos: 0 };
        let sig = Sig { params: vec![Param { name: pname, ty: first.ty.clone(), default: None, kwonly: false }], vararg: None, kwarg: None, ret: Ty::iter(Ty::Var("?".into())) };
        let (cname, ty) = self.compile_closure(&def, &sig, true, span)?;
        let env = cname.1;
        Ok(CE::new(ty, format!("{}({}, {})", cname.0, env, first.c)))
    }

    /// Compile a synthesized closure (lambda/genexp). Returns ((cname, env expr), function or iterator type).
    pub fn compile_closure(&mut self, def: &FuncDef, sig: &Sig, is_gen: bool, span: Span) -> R<((String, String), Ty)> {
        let m = self.cur_mod();
        let scope = Rc::new(scope::analyze_function(&def.params, &def.body));
        let caps = self.captures_for(&scope, span)?;
        self.lambda_count += 1;
        let cname = format!("L_{}_{}", if is_gen { "genexpr" } else { "lambda" }, self.lambda_count);
        let mut fx = func::Fx::new(m, self.fxr().class);
        fx.scope = scope.clone();
        fx.ret = sig.ret.clone();
        fx.subst = self.fxr().subst.clone();
        fx.fname = def.name.clone();
        fx.dry_run = self.fxr().dry_run;
        self.fx_stack.push(fx);
        let res = self.compile_function_body(&cname, def, sig, is_gen, Some(&caps));
        let fx = self.fx_stack.pop().unwrap();
        let (proto, code) = res?;
        if !self.fxr().dry_run {
            self.out.protos.push_str(&proto);
            self.out.funcs.push_str(&code);
        }
        let env = self.build_env(&caps)?;
        let ty = if is_gen {
            Ty::iter(fx.gen.map(|g| g.1).unwrap_or(Ty::Any))
        } else {
            Ty::Func(sig.params.iter().map(|p| p.ty.clone()).collect(), Box::new(fx.ret.clone()))
        };
        if !is_gen && !self.fxr().dry_run {
            self.closure_dyn_wrapper(&cname, &ty);
        }
        Ok(((cname, env), ty))
    }

    /// Dynamic entry (boxed args) for a closure so Python and `Any` calls can invoke it.
    pub fn closure_dyn_wrapper(&mut self, cname: &str, ty: &Ty) {
        let Ty::Func(ps, ret) = ty else { return };
        let mut args = vec!["env".to_string()];
        for (i, p) in ps.iter().enumerate() {
            args.push(self.unbox(&format!("a[{}]", i), p));
        }
        let call = format!("{}({})", cname, args.join(", "));
        let body = if **ret == Ty::NoneT || **ret == Ty::Never { format!("{}; return NULL;", call) } else { format!("return {};", self.coerce(&call, ret, &Ty::Any)) };
        self.out.protos.push_str(&format!("static FpyObj *DW_{}(void *env, FpyObj **a, int64_t n);\n", cname));
        self.out.funcs.push_str(&format!(
            "static FpyObj *DW_{c}(void *env, FpyObj **a, int64_t n) {{\n    if (n != {n}) fpy_raise_fmt(FPY_EXC_TYPE, \"lambda takes %d arguments (%lld given)\", {n}, (long long)n);\n    {b}\n}}\n\n",
            c = cname,
            n = ps.len(),
            b = body
        ));
    }

    fn lambda(&mut self, params: &Params, body: &Expr, want: Option<&Ty>, span: Span) -> R<CE> {
        let (pts, ret) = match want {
            Some(Ty::Func(ps, r)) => (ps.clone(), (**r).clone()),
            _ => return Err(self.e(span, "cannot infer lambda parameter types here; assign it to an annotated variable (Callable[[...], ...])")),
        };
        let names: Vec<&Arg> = params.all_positional().collect();
        if names.len() != pts.len() {
            return Err(self.e(span, format!("lambda takes {} parameters but {} are expected", names.len(), pts.len())));
        }
        let def = FuncDef {
            name: "<lambda>".into(),
            type_params: vec![],
            params: params.clone(),
            returns: None,
            body: vec![Stmt { kind: StmtKind::Return(Some(body.clone())), span, end_line: span.line, header_end: span.line }],
            decorators: vec![],
            is_async: false,
            header_lines: (0, 0),
            close_paren_pos: 0,
            colon_pos: 0,
        };
        let sig = Sig {
            params: names.iter().zip(&pts).map(|(a, t)| Param { name: a.name.clone(), ty: t.clone(), default: None, kwonly: false }).collect(),
            vararg: None,
            kwarg: None,
            ret: ret.clone(),
        };
        let ((cname, env), ty) = self.compile_closure(&def, &sig, false, span)?;
        let dw = if self.fxr().dry_run { "NULL".to_string() } else { format!("DW_{}", cname) };
        Ok(CE::new(ty, format!("fpy_func_new((void*){}, {}, {}, \"<lambda>\")", cname, env, dw)))
    }

    // ------------------------------------------------------------------ strings

    pub fn to_str(&mut self, v: &CE) -> String {
        match &v.ty {
            Ty::Str => v.c.clone(),
            Ty::Int => format!("fpy_int_to_str({})", v.c),
            Ty::Float => format!("fpy_float_to_str({})", v.c),
            Ty::Bool => format!("fpy_bool_to_str({})", v.c),
            Ty::Class(c) => {
                if let Some(f) = self.find_method(*c, "__str__") {
                    if !self.is_overridden(*c, "__str__") {
                        let n = self.method_cname(f);
                        let oc = self.classes[self.funcs[f].owner.unwrap()].cname.clone();
                        return format!("{}((struct {}*)({}))", n, oc, v.c);
                    }
                }
                format!("fpy_obj_str((FpyObj*)({}))", v.c)
            }
            t if t.is_boxed() => format!("fpy_obj_str({})", v.c),
            _ => format!("fpy_obj_str((FpyObj*)({}))", v.c),
        }
    }

    pub fn to_repr(&mut self, v: &CE) -> String {
        match &v.ty {
            Ty::Str => format!("fpy_str_repr({})", v.c),
            Ty::Int => format!("fpy_int_to_str({})", v.c),
            Ty::Float => format!("fpy_float_to_str({})", v.c),
            Ty::Bool => format!("fpy_bool_to_str({})", v.c),
            _ => format!("fpy_obj_repr((FpyObj*)({}))", v.c),
        }
    }

    fn fstring(&mut self, parts: &[FStrPart]) -> R<CE> {
        // Single literal: just a string.
        if parts.len() == 1 {
            if let FStrPart::Lit(s) = &parts[0] {
                return Ok(CE::new(Ty::Str, self.string_lit(s)));
            }
        }
        let sb = self.fresh("sb");
        let mut code = format!("({{ FpySB {}; fpy_sb_init(&{}); ", sb, sb);
        for p in parts {
            match p {
                FStrPart::Lit(s) => {
                    code.push_str(&format!("fpy_sb_add(&{}, {}, {}); ", sb, c_string(s.as_bytes()), s.len()));
                }
                FStrPart::Expr { expr, conv, spec, debug_text } => {
                    if let Some(d) = debug_text {
                        code.push_str(&format!("fpy_sb_add(&{}, {}, {}); ", sb, c_string(d.as_bytes()), d.len()));
                    }
                    let v = self.expr(expr, None)?;
                    let s = if !spec.is_empty() {
                        let spec_ce = self.fstring(spec)?;
                        let base = match conv {
                            Some('r') => CE::new(Ty::Str, self.to_repr(&v)),
                            Some('s') => CE::new(Ty::Str, self.to_str(&v)),
                            Some('a') => CE::new(Ty::Str, format!("fpy_ascii_escape({})", self.to_repr(&v))),
                            _ => v.clone(),
                        };
                        match &base.ty {
                            Ty::Int | Ty::Bool => format!("fpy_format_int({}, {})", self.coerce(&base.c, &base.ty, &Ty::Int), spec_ce.c),
                            Ty::Float => format!("fpy_format_float({}, {})", base.c, spec_ce.c),
                            Ty::Str => format!("fpy_format_str({}, {})", base.c, spec_ce.c),
                            Ty::Class(c) if self.find_method(*c, "__format__").is_some() => {
                                let f = self.find_method(*c, "__format__").unwrap();
                                self.call_method_with_ces(base.clone(), f, vec![spec_ce], Span::default())?.c
                            }
                            _ => {
                                let st = self.to_str(&base);
                                format!("fpy_format_str({}, {})", st, spec_ce.c)
                            }
                        }
                    } else {
                        match (conv, &v.ty) {
                            (Some('r'), _) => self.to_repr(&v),
                            (Some('a'), _) => format!("fpy_ascii_escape({})", self.to_repr(&v)),
                            _ if debug_text.is_some() && conv.is_none() => self.to_repr(&v),
                            // f"{x}" is format(x, ""): a class's own __format__ wins over __str__
                            (None, Ty::Class(c)) if self.find_method(*c, "__format__").is_some() => {
                                let f = self.find_method(*c, "__format__").unwrap();
                                let empty = CE::new(Ty::Str, self.string_lit(""));
                                self.call_method_with_ces(v.clone(), f, vec![empty], Span::default())?.c
                            }
                            _ => self.to_str(&v),
                        }
                    };
                    code.push_str(&format!("fpy_sb_add_str(&{}, {}); ", sb, s));
                }
            }
        }
        code.push_str(&format!("fpy_sb_finish(&{}); }})", sb));
        Ok(CE::new(Ty::Str, code))
    }
}

pub enum Elem {
    One(String),
    Splat(CE),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CompKind {
    List,
    Set,
    Dict,
}

pub fn float_lit(v: f64) -> String {
    if v.is_infinite() {
        return if v > 0.0 { "INFINITY".into() } else { "(-INFINITY)".into() };
    }
    if v.is_nan() {
        return "NAN".into();
    }
    let s = format!("{:?}", v);
    if s.contains('.') || s.contains('e') || s.contains("inf") {
        s
    } else {
        format!("{}.0", s)
    }
}
