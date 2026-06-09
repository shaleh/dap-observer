//! Runtime values for `let` bindings and the operands of conditions.
//!
//! The language has no data structures of its own. A value is a number or a
//! string. An `eval` result arrives as the adapter's rendered display string, so
//! comparisons coerce to a number when both sides look numeric and fall back to
//! string comparison otherwise.

use crate::ast::ComparisonOp;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Str(String),
}

impl Value {
    /// The text used when interpolating this value into a `print`.
    pub fn as_string(&self) -> String {
        match self {
            Value::Int(n) => n.to_string(),
            Value::Str(s) => s.clone(),
        }
    }

    fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            Value::Str(s) => s.trim().parse().ok(),
        }
    }
}

/// Compare two values under an operator. Numeric when both sides parse as
/// integers, lexicographic otherwise.
pub fn compare(op: ComparisonOp, left: &Value, right: &Value) -> bool {
    let ordering = match (left.as_int(), right.as_int()) {
        (Some(l), Some(r)) => l.cmp(&r),
        _ => left.as_string().cmp(&right.as_string()),
    };
    match op {
        ComparisonOp::Eq => ordering == std::cmp::Ordering::Equal,
        ComparisonOp::Ne => ordering != std::cmp::Ordering::Equal,
        ComparisonOp::Lt => ordering == std::cmp::Ordering::Less,
        ComparisonOp::Le => ordering != std::cmp::Ordering::Greater,
        ComparisonOp::Gt => ordering == std::cmp::Ordering::Greater,
        ComparisonOp::Ge => ordering != std::cmp::Ordering::Less,
    }
}
