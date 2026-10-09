//! fpython's static type model and its mapping onto C storage.

use std::fmt;

pub type ClassId = usize;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    NoneT,
    List(Box<Ty>),
    Dict(Box<Ty>, Box<Ty>),
    Set(Box<Ty>),
    Tuple(Vec<Ty>),
    /// `tuple[T, ...]`: a tuple of any length whose items all have type T
    VarTuple(Box<Ty>),
    /// `T | None`
    Optional(Box<Ty>),
    /// General union of non-None members (boxed representation).
    Union(Vec<Ty>),
    Class(ClassId),
    /// Callable[[params], ret]
    Func(Vec<Ty>, Box<Ty>),
    /// Iterator[T] / Iterable[T] / Generator[T, ...]
    Iter(Box<Ty>),
    /// type[C] — a class object.
    Type(ClassId),
    Any,
    /// The type of expressions that never produce a value (raise, sys.exit).
    Never,
    /// Unresolved type variable (only inside generic signatures during inference).
    Var(String),
}

pub const FK_INT: u8 = 0;
pub const FK_FLOAT: u8 = 1;
pub const FK_BOOL: u8 = 2;
pub const FK_OBJ: u8 = 3;

impl Ty {
    pub fn list(t: Ty) -> Ty {
        Ty::List(Box::new(t))
    }
    pub fn dict(k: Ty, v: Ty) -> Ty {
        Ty::Dict(Box::new(k), Box::new(v))
    }
    pub fn set(t: Ty) -> Ty {
        Ty::Set(Box::new(t))
    }
    pub fn iter(t: Ty) -> Ty {
        Ty::Iter(Box::new(t))
    }
    pub fn optional(t: Ty) -> Ty {
        match t {
            Ty::Optional(_) | Ty::Any | Ty::NoneT => t,
            t => Ty::Optional(Box::new(t)),
        }
    }

    /// Storage kind used inside containers.
    pub fn kind(&self) -> u8 {
        match self {
            Ty::Int => FK_INT,
            Ty::Float => FK_FLOAT,
            Ty::Bool => FK_BOOL,
            _ => FK_OBJ,
        }
    }

    pub fn is_scalar(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float | Ty::Bool)
    }

    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float | Ty::Bool)
    }

    /// Values represented as boxed `FpyObj*` (Any, unions, Optional of scalars).
    pub fn is_boxed(&self) -> bool {
        match self {
            Ty::Any | Ty::Union(_) => true,
            Ty::Optional(t) => t.is_scalar() || t.is_boxed(),
            _ => false,
        }
    }

    pub fn is_nullable(&self) -> bool {
        matches!(self, Ty::Optional(_) | Ty::Any | Ty::NoneT)
    }

    pub fn ctype(&self, class_cname: &dyn Fn(ClassId) -> String) -> String {
        match self {
            Ty::Int => "fpy_int".into(),
            Ty::Float => "double".into(),
            Ty::Bool => "bool".into(),
            Ty::Str => "FpyStr*".into(),
            Ty::Bytes => "FpyBytes*".into(),
            Ty::NoneT | Ty::Any | Ty::Union(_) | Ty::Never | Ty::Type(_) | Ty::Var(_) => "FpyObj*".into(),
            Ty::List(_) => "FpyList*".into(),
            Ty::Dict(..) | Ty::Set(_) => "FpyDict*".into(),
            Ty::Tuple(_) | Ty::VarTuple(_) => "FpyTuple*".into(),
            Ty::Optional(t) => {
                if t.is_scalar() || t.is_boxed() {
                    "FpyObj*".into()
                } else {
                    t.ctype(class_cname)
                }
            }
            Ty::Class(id) => format!("struct {}*", class_cname(*id)),
            Ty::Func(..) => "FpyFunc*".into(),
            Ty::Iter(_) => "FpyIter*".into(),
        }
    }

    pub fn zero(&self) -> &'static str {
        match self {
            Ty::Int => "0",
            Ty::Float => "0.0",
            Ty::Bool => "false",
            _ => "NULL",
        }
    }

    /// Convert a C expression of this type to an `fpy_word`.
    pub fn to_word(&self, code: &str) -> String {
        match self {
            Ty::Int => format!("((fpy_word)({}))", code),
            Ty::Float => format!("fpy_wf({})", code),
            Ty::Bool => format!("((fpy_word)({}))", code),
            _ => format!("fpy_wp({})", code),
        }
    }

    /// Convert an `fpy_word` C expression to this type.
    pub fn from_word(&self, code: &str, ctype: &str) -> String {
        match self {
            Ty::Int => format!("((fpy_int)({}))", code),
            Ty::Float => format!("fpy_fw({})", code),
            Ty::Bool => format!("(({}) != 0)", code),
            _ => format!("(({})fpy_pw({}))", ctype, code),
        }
    }

    pub fn has_var(&self) -> bool {
        match self {
            Ty::Var(_) => true,
            Ty::List(t) | Ty::Set(t) | Ty::Optional(t) | Ty::Iter(t) | Ty::VarTuple(t) => t.has_var(),
            Ty::Dict(k, v) => k.has_var() || v.has_var(),
            Ty::Tuple(ts) | Ty::Union(ts) => ts.iter().any(|t| t.has_var()),
            Ty::Func(ps, r) => ps.iter().any(|t| t.has_var()) || r.has_var(),
            _ => false,
        }
    }

    pub fn subst(&self, m: &std::collections::HashMap<String, Ty>) -> Ty {
        match self {
            Ty::Var(n) => m.get(n).cloned().unwrap_or_else(|| self.clone()),
            Ty::List(t) => Ty::List(Box::new(t.subst(m))),
            Ty::Set(t) => Ty::Set(Box::new(t.subst(m))),
            Ty::Optional(t) => Ty::optional(t.subst(m)),
            Ty::Iter(t) => Ty::Iter(Box::new(t.subst(m))),
            Ty::Dict(k, v) => Ty::Dict(Box::new(k.subst(m)), Box::new(v.subst(m))),
            Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| t.subst(m)).collect()),
            Ty::VarTuple(t) => Ty::VarTuple(Box::new(t.subst(m))),
            Ty::Union(ts) => Ty::Union(ts.iter().map(|t| t.subst(m)).collect()),
            Ty::Func(ps, r) => Ty::Func(ps.iter().map(|t| t.subst(m)).collect(), Box::new(r.subst(m))),
            _ => self.clone(),
        }
    }

}

/// Display with class names resolved through a callback.
pub struct TyDisplay<'a> {
    pub ty: &'a Ty,
    pub names: &'a dyn Fn(ClassId) -> String,
}

impl fmt::Display for TyDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = |t: &Ty| TyDisplay { ty: t, names: self.names }.to_string();
        match self.ty {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::Bytes => write!(f, "bytes"),
            Ty::NoneT => write!(f, "None"),
            Ty::List(t) => write!(f, "list[{}]", d(t)),
            Ty::Dict(k, v) => write!(f, "dict[{}, {}]", d(k), d(v)),
            Ty::Set(t) => write!(f, "set[{}]", d(t)),
            Ty::Tuple(ts) => {
                if ts.is_empty() {
                    write!(f, "tuple[()]")
                } else {
                    write!(f, "tuple[{}]", ts.iter().map(d).collect::<Vec<_>>().join(", "))
                }
            }
            Ty::VarTuple(t) => write!(f, "tuple[{}, ...]", d(t)),
            Ty::Optional(t) => write!(f, "{} | None", d(t)),
            Ty::Union(ts) => write!(f, "{}", ts.iter().map(d).collect::<Vec<_>>().join(" | ")),
            Ty::Class(id) => write!(f, "{}", (self.names)(*id)),
            Ty::Type(id) => write!(f, "type[{}]", (self.names)(*id)),
            Ty::Func(ps, r) => write!(f, "Callable[[{}], {}]", ps.iter().map(d).collect::<Vec<_>>().join(", "), d(r)),
            Ty::Iter(t) => write!(f, "Iterator[{}]", d(t)),
            Ty::Any => write!(f, "Any"),
            Ty::Never => write!(f, "Never"),
            Ty::Var(n) => write!(f, "{}", n),
        }
    }
}
