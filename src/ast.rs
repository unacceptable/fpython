//! Abstract syntax tree for fpython. Mirrors Python 3.14's grammar closely so that
//! pure-Python modules can be parsed by the same front end.

use crate::diag::Span;

pub type P<T> = Box<T>;

#[derive(Debug, Clone)]
pub struct Module {
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
    /// Last source line covered by the statement (for comment attachment in the formatter).
    pub end_line: usize,
    /// For compound statements: line of the ':' ending the header (else equals end_line).
    pub header_end: usize,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: String,
    pub ann: Option<Expr>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct Params {
    pub posonly: Vec<Arg>,
    pub args: Vec<Arg>,
    pub vararg: Option<Arg>,
    pub kwonly: Vec<Arg>,
    pub kwarg: Option<Arg>,
}

impl Params {
    pub fn all_positional(&self) -> impl Iterator<Item = &Arg> {
        self.posonly.iter().chain(self.args.iter())
    }
}

#[derive(Debug, Clone)]
pub struct TypeParam {
    pub name: String,
    pub bound: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct FuncDef {
    pub name: String,
    pub type_params: Vec<TypeParam>,
    pub params: Params,
    pub returns: Option<Expr>,
    pub body: Vec<Stmt>,
    pub decorators: Vec<Expr>,
    pub is_async: bool,
    /// Lines of the `def` keyword and of the ':' ending the header (for the formatter).
    pub header_lines: (usize, usize),
    /// Byte offsets of the ')' closing the parameter list and of the header ':' (for `convert`).
    pub close_paren_pos: usize,
    pub colon_pos: usize,
}

#[derive(Debug, Clone)]
pub struct ClassDef {
    pub name: String,
    pub type_params: Vec<TypeParam>,
    pub bases: Vec<Expr>,
    pub keywords: Vec<Keyword>,
    pub body: Vec<Stmt>,
    pub decorators: Vec<Expr>,
    pub header_lines: (usize, usize),
}

#[derive(Debug, Clone)]
pub struct Alias {
    pub name: String,
    pub asname: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExceptHandler {
    pub typ: Option<Expr>,
    pub name: Option<String>,
    pub body: Vec<Stmt>,
    pub span: Span,
    pub header_end: usize,
}

#[derive(Debug, Clone)]
pub struct WithItem {
    pub ctx: Expr,
    pub var: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct MatchCase {
    /// Line of the `case` keyword.
    pub line: usize,
    pub pattern: Pattern,
    pub guard: Option<Expr>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard,
    Capture(String),
    Value(Expr),
    Or(Vec<Pattern>),
    As(P<Pattern>, String),
    /// `[a, b]` or `(a, b)` / `a, b` (the flag is true for the parenthesized/open form)
    Sequence(Vec<Pattern>, bool),
    Star(Option<String>),
    Class { cls: Expr, args: Vec<Pattern>, kwargs: Vec<(String, Pattern)> },
    Mapping { keys: Vec<Expr>, values: Vec<Pattern>, rest: Option<String> },
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Expr(Expr),
    Assign { targets: Vec<Expr>, value: Expr },
    AnnAssign { target: Expr, ann: Expr, value: Option<Expr> },
    AugAssign { target: Expr, op: BinOp, value: Expr },
    Pass,
    Break,
    Continue,
    Return(Option<Expr>),
    Raise { exc: Option<Expr>, cause: Option<Expr> },
    Global(Vec<String>),
    Nonlocal(Vec<String>),
    Del(Vec<Expr>),
    Assert { test: Expr, msg: Option<Expr> },
    Import(Vec<Alias>),
    ImportFrom { module: String, level: usize, names: Vec<Alias> },
    If { test: Expr, body: Vec<Stmt>, orelse: Vec<Stmt> },
    While { test: Expr, body: Vec<Stmt>, orelse: Vec<Stmt> },
    For { target: Expr, iter: Expr, body: Vec<Stmt>, orelse: Vec<Stmt>, is_async: bool },
    Try { body: Vec<Stmt>, handlers: Vec<ExceptHandler>, orelse: Vec<Stmt>, finalbody: Vec<Stmt>, star: bool },
    With { items: Vec<WithItem>, body: Vec<Stmt>, is_async: bool },
    FuncDef(P<FuncDef>),
    ClassDef(P<ClassDef>),
    TypeAlias { name: String, type_params: Vec<TypeParam>, value: Expr },
    Match { subject: Expr, cases: Vec<MatchCase> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FloorDiv,
    Mod,
    Pow,
    MatMul,
    LShift,
    RShift,
    BitAnd,
    BitOr,
    BitXor,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FloorDiv => "//",
            BinOp::Mod => "%",
            BinOp::Pow => "**",
            BinOp::MatMul => "@",
            BinOp::LShift => "<<",
            BinOp::RShift => ">>",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
        }
    }
    pub fn dunder(self) -> &'static str {
        match self {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "truediv",
            BinOp::FloorDiv => "floordiv",
            BinOp::Mod => "mod",
            BinOp::Pow => "pow",
            BinOp::MatMul => "matmul",
            BinOp::LShift => "lshift",
            BinOp::RShift => "rshift",
            BinOp::BitAnd => "and",
            BinOp::BitOr => "or",
            BinOp::BitXor => "xor",
        }
    }
    pub fn precedence(self) -> u8 {
        match self {
            BinOp::BitOr => 6,
            BinOp::BitXor => 7,
            BinOp::BitAnd => 8,
            BinOp::LShift | BinOp::RShift => 9,
            BinOp::Add | BinOp::Sub => 10,
            BinOp::Mul | BinOp::Div | BinOp::FloorDiv | BinOp::Mod | BinOp::MatMul => 11,
            BinOp::Pow => 13,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Pos,
    Not,
    Invert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtE,
    Gt,
    GtE,
    Is,
    IsNot,
    In,
    NotIn,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::NotEq => "!=",
            CmpOp::Lt => "<",
            CmpOp::LtE => "<=",
            CmpOp::Gt => ">",
            CmpOp::GtE => ">=",
            CmpOp::Is => "is",
            CmpOp::IsNot => "is not",
            CmpOp::In => "in",
            CmpOp::NotIn => "not in",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    And,
    Or,
}

#[derive(Debug, Clone)]
pub struct Keyword {
    /// None for `**kwargs` splat.
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct Comprehension {
    pub target: Expr,
    pub iter: Expr,
    pub ifs: Vec<Expr>,
    pub is_async: bool,
}

#[derive(Debug, Clone)]
pub enum FStrPart {
    Lit(String),
    Expr { expr: P<Expr>, conv: Option<char>, spec: Vec<FStrPart>, debug_text: Option<String> },
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Name(String),
    Int(i64),
    BigInt(String),
    Float(f64),
    Imag(f64),
    /// String literal; `raw` is the original source (all concatenated pieces), used by the formatter.
    Str { value: String, raw: Vec<String> },
    Bytes { value: Vec<u8>, raw: Vec<String> },
    FStr { parts: Vec<FStrPart>, raw: Vec<String> },
    Bool(bool),
    NoneLit,
    Ellipsis,
    BinOp { left: P<Expr>, op: BinOp, right: P<Expr> },
    UnaryOp { op: UnOp, operand: P<Expr> },
    BoolOp { op: BoolOp, values: Vec<Expr> },
    Compare { left: P<Expr>, ops: Vec<CmpOp>, comparators: Vec<Expr> },
    Call { func: P<Expr>, args: Vec<Expr>, keywords: Vec<Keyword> },
    Attribute { value: P<Expr>, attr: String },
    Subscript { value: P<Expr>, index: P<Expr> },
    Slice { lower: Option<P<Expr>>, upper: Option<P<Expr>>, step: Option<P<Expr>> },
    Starred(P<Expr>),
    List(Vec<Expr>),
    Tuple { elts: Vec<Expr>, parens: bool },
    Set(Vec<Expr>),
    /// Dict entries; key None means `**splat`.
    Dict(Vec<(Option<Expr>, Expr)>),
    ListComp { elt: P<Expr>, gens: Vec<Comprehension> },
    SetComp { elt: P<Expr>, gens: Vec<Comprehension> },
    DictComp { key: P<Expr>, value: P<Expr>, gens: Vec<Comprehension> },
    GenExp { elt: P<Expr>, gens: Vec<Comprehension> },
    IfExp { test: P<Expr>, body: P<Expr>, orelse: P<Expr> },
    Lambda { params: P<Params>, body: P<Expr> },
    NamedExpr { target: String, value: P<Expr> },
    Await(P<Expr>),
    Yield(Option<P<Expr>>),
    YieldFrom(P<Expr>),
}

impl Expr {
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binop_tables() {
        use BinOp::*;
        let all = [Add, Sub, Mul, Div, FloorDiv, Mod, Pow, MatMul, LShift, RShift, BitAnd, BitOr, BitXor];
        let syms: Vec<&str> = all.iter().map(|o| o.symbol()).collect();
        assert_eq!(syms, ["+", "-", "*", "/", "//", "%", "**", "@", "<<", ">>", "&", "|", "^"]);
        let dunders: Vec<&str> = all.iter().map(|o| o.dunder()).collect();
        assert_eq!(dunders, ["add", "sub", "mul", "truediv", "floordiv", "mod", "pow", "matmul", "lshift", "rshift", "and", "or", "xor"]);
        assert!(all.iter().all(|o| o.precedence() > 0));
        assert!(Pow.precedence() > Mul.precedence() && Mul.precedence() > Add.precedence());
    }
}
