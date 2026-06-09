//! The dap-script language, as a tree.
//!
//! One [`Stmt`] per source statement. The interpreter walks this directly. The
//! grammar that produces it lives in the parser module.

/// A value-producing expression. These appear on the right of `let`, inside
/// condition operands, and inside `print` interpolations.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Int(i64),
    Str(String),
    /// A `let` binding referenced by name.
    Ident(String),
    /// `eval "expr"`: evaluate in the current frame and take the result.
    Eval(String),
    /// `frame.line`, the source line of the current top frame.
    FrameLine,
    /// `frame.name`, the name of the current top frame.
    FrameName,
    /// `frame.source`, the source path of the current top frame.
    FrameSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A comparison between two expressions, used by `if` and the loops.
#[derive(Debug, Clone, PartialEq)]
pub struct Cond {
    pub left: Expr,
    pub op: ComparisonOp,
    pub right: Expr,
}

/// The stopping target of a loop. `continue until` accepts a line or a
/// condition. `step until` accepts only a condition.
#[derive(Debug, Clone, PartialEq)]
pub enum Until {
    Line(i64),
    Cond(Cond),
}

/// An execution-control verb. `step` is the synonym for `stepIn`,
/// following the gdb and lldb convention where the bare verb steps into a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionControl {
    Continue,
    Next,
    Step,
    StepIn,
    StepOut,
}

impl ExecutionControl {
    /// The DAP request this verb issues.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecutionControl::Continue => "continue",
            ExecutionControl::Next => "next",
            ExecutionControl::Step | ExecutionControl::StepIn => "stepIn",
            ExecutionControl::StepOut => "stepOut",
        }
    }
}

/// String part of a `print` argument.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Literal(String),
    Interpolated(Expr),
}

/// The argument to `print`. `eval "expr"` emits whatever the expression returns.
/// A template emits interpolated prose.
#[derive(Debug, Clone, PartialEq)]
pub enum Print {
    Eval(String),
    Template(Vec<StrPart>),
}

/// What `dump` walks into JSON. `locals` and `eval` produce variable trees.
/// `stack` and `frame` produce the frame-shaped state of the current stop.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Locals,
    Stack,
    Frame,
    Eval(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `connect [address]`. The address, when present, is the raw text of a port
    /// or `host:port`, resolved against the default at run time.
    Connect(Option<String>),
    ExpectStopped,
    Let {
        name: String,
        value: Expr,
    },
    If {
        cond: Cond,
        then_block: Vec<Stmt>,
        else_block: Option<Vec<Stmt>>,
    },
    Repeat {
        count: u64,
        body: Vec<Stmt>,
    },
    StepUntil(Until),
    ContinueUntil(Until),
    ExecutionControl(ExecutionControl),
    Print(Print),
    Dump {
        query: Query,
        depth: Option<usize>,
    },
    /// `launch <json>`. The value is an adapter launch configuration, forwarded
    /// verbatim as the `launch` request arguments. dap-script does not interpret
    /// the keys.
    Launch(serde_json::Value),
    /// `break <file>:<line>`. A breakpoint to set during the launch handshake.
    Break {
        file: String,
        line: i64,
    },
}
