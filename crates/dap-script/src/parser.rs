//! The dap-script grammar.

use chumsky::prelude::*;
use serde_json::{Number, Value};

use crate::ast::{ComparisonOp, Cond, ExecutionControl, Expr, Print, Query, Stmt, StrPart, Until};

/// Parse a whole script into its statements.
pub fn parse_program(src: &str) -> Result<Vec<Stmt>, String> {
    program_parser()
        .parse(src)
        .into_result()
        .map_err(|errors| format_errors(src, &errors))
}

/// Whitespace and `#` line comments, the gaps allowed between tokens.
fn padding<'src>() -> impl Parser<'src, &'src str, (), extra::Err<Rich<'src, char>>> + Clone {
    let comment = just('#')
        .ignore_then(any().filter(|c: &char| *c != '\n').repeated())
        .ignored();
    let whitespace = any().filter(|c: &char| c.is_whitespace()).ignored();
    choice((whitespace, comment)).repeated().ignored()
}

/// Surround a token parser with the inter-token padding.
fn padded<'src, O, P>(
    parser: P,
) -> impl Parser<'src, &'src str, O, extra::Err<Rich<'src, char>>> + Clone
where
    P: Parser<'src, &'src str, O, extra::Err<Rich<'src, char>>> + Clone,
{
    parser.padded_by(padding())
}

/// A reserved word, padded.
fn keyword<'src>(
    word: &'static str,
) -> impl Parser<'src, &'src str, (), extra::Err<Rich<'src, char>>> + Clone {
    padded(text::keyword(word).ignored())
}

/// A literal symbol such as `=` or `==`, padded.
fn sym<'src>(
    s: &'static str,
) -> impl Parser<'src, &'src str, (), extra::Err<Rich<'src, char>>> + Clone {
    padded(just(s).ignored())
}

fn int_i64<'src>() -> impl Parser<'src, &'src str, i64, extra::Err<Rich<'src, char>>> + Clone {
    padded(text::int(10).try_map(|s: &str, span| {
        s.parse::<i64>()
            .map_err(|e| Rich::custom(span, format!("invalid integer: {e}")))
    }))
}

fn int_u64<'src>() -> impl Parser<'src, &'src str, u64, extra::Err<Rich<'src, char>>> + Clone {
    padded(text::int(10).try_map(|s: &str, span| {
        s.parse::<u64>()
            .map_err(|e| Rich::custom(span, format!("invalid count: {e}")))
    }))
}

/// A double-quoted string with the usual escapes, padded.
fn string<'src>() -> impl Parser<'src, &'src str, String, extra::Err<Rich<'src, char>>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('t').to('\t'),
        just('r').to('\r'),
    )));
    let normal = any().filter(|c: &char| *c != '"' && *c != '\\');
    padded(
        choice((escape, normal))
            .repeated()
            .collect::<String>()
            .delimited_by(just('"'), just('"')),
    )
}

/// A value expression. Reused both inside the grammar and standalone for the
/// pieces of a `print` template.
fn expr_parser<'src>() -> impl Parser<'src, &'src str, Expr, extra::Err<Rich<'src, char>>> + Clone {
    let eval = keyword("eval").ignore_then(string()).map(Expr::Eval);
    // An identifier, optionally with a `.field` access. Parsing the whole
    // `name.field` in one branch means a bad field is a clear error rather than
    // silently falling back to the bare identifier `frame` plus leftover.
    let ident_or_frame = padded(
        text::ident()
            .then(just('.').ignore_then(text::ident()).or_not())
            .try_map(|(name, field): (&str, Option<&str>), span| match field {
                None => Ok(Expr::Ident(name.to_string())),
                Some("line") if name == "frame" => Ok(Expr::FrameLine),
                Some("name") if name == "frame" => Ok(Expr::FrameName),
                Some("source") if name == "frame" => Ok(Expr::FrameSource),
                Some(field) if name == "frame" => Err(Rich::custom(
                    span,
                    format!("unknown frame field `{field}`; expected line, name, or source"),
                )),
                Some(field) => Err(Rich::custom(
                    span,
                    format!("unknown member access `{name}.{field}`"),
                )),
            }),
    );
    choice((
        eval,
        ident_or_frame,
        int_i64().map(Expr::Int),
        string().map(Expr::Str),
    ))
}

fn comparison_op<'src>()
-> impl Parser<'src, &'src str, ComparisonOp, extra::Err<Rich<'src, char>>> + Clone {
    choice((
        sym("==").to(ComparisonOp::Eq),
        sym("!=").to(ComparisonOp::Ne),
        sym("<=").to(ComparisonOp::Le),
        sym(">=").to(ComparisonOp::Ge),
        sym("<").to(ComparisonOp::Lt),
        sym(">").to(ComparisonOp::Gt),
    ))
}

fn cond_parser<'src>() -> impl Parser<'src, &'src str, Cond, extra::Err<Rich<'src, char>>> + Clone {
    expr_parser()
        .then(comparison_op())
        .then(expr_parser())
        .map(|((left, op), right)| Cond { left, op, right })
}

/// A JSON value, for a `launch` configuration. dap-script forwards this verbatim
/// to the adapter as plain JSON.
fn json_value<'src>() -> impl Parser<'src, &'src str, Value, extra::Err<Rich<'src, char>>> + Clone {
    recursive(|value| {
        // Capture a run of number characters and let serde_json validate it, so
        // integers, floats, and exponents all parse and an invalid number such
        // as a leading-zero gets a clear error rather than a stray-token one.
        let number = padded(
            any()
                .filter(|c: &char| c.is_ascii_digit() || matches!(*c, '-' | '+' | '.' | 'e' | 'E'))
                .repeated()
                .at_least(1)
                .to_slice()
                .try_map(|s: &str, span| {
                    serde_json::from_str::<Number>(s)
                        .map(Value::Number)
                        .map_err(|_| Rich::custom(span, "not a valid JSON number".to_string()))
                }),
        );
        let literal = choice((
            keyword("true").to(Value::Bool(true)),
            keyword("false").to(Value::Bool(false)),
            keyword("null").to(Value::Null),
        ));
        let array = value
            .clone()
            .separated_by(sym(","))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(sym("["), sym("]"))
            .map(Value::Array);
        let member = string().then_ignore(sym(":")).then(value);
        let object = member
            .separated_by(sym(","))
            .allow_trailing()
            .collect::<Vec<(String, Value)>>()
            .delimited_by(sym("{"), sym("}"))
            .map(|members| Value::Object(members.into_iter().collect()));
        choice((object, array, string().map(Value::String), number, literal))
    })
}

fn program_parser<'src>() -> impl Parser<'src, &'src str, Vec<Stmt>, extra::Err<Rich<'src, char>>> {
    let stmt = recursive(|stmt| {
        let block = stmt
            .repeated()
            .collect::<Vec<_>>()
            .delimited_by(sym("{"), sym("}"));

        let connect = keyword("connect")
            .ignore_then(choice((string(), int_i64().map(|n| n.to_string()))).or_not())
            .map(Stmt::Connect);

        let expect = keyword("expect")
            .ignore_then(keyword("stopped"))
            .to(Stmt::ExpectStopped);

        let let_stmt = keyword("let")
            .ignore_then(padded(text::ident()).map(|s: &str| s.to_string()))
            .then_ignore(sym("="))
            .then(expr_parser())
            .map(|(name, value)| Stmt::Let { name, value });

        let if_stmt = keyword("if")
            .ignore_then(cond_parser())
            .then(block.clone())
            .then(keyword("else").ignore_then(block.clone()).or_not())
            .map(|((cond, then_block), else_block)| Stmt::If {
                cond,
                then_block,
                else_block,
            });

        let repeat = keyword("repeat")
            .ignore_then(int_u64())
            .then(block.clone())
            .map(|(count, body)| Stmt::Repeat { count, body });

        // `step until` takes a condition only. A line target invites stepping
        // into callees while checking a line that means something else there, so
        // it is left out until real use shows whether it is wanted.
        let step_stmt = keyword("step").ignore_then(
            keyword("until")
                .ignore_then(cond_parser())
                .map(|cond| Stmt::StepUntil(Until::Cond(cond)))
                .or(empty().to(Stmt::ExecutionControl(ExecutionControl::Step))),
        );

        let continue_target = keyword("line")
            .ignore_then(int_i64())
            .map(Until::Line)
            .or(cond_parser().map(Until::Cond));
        let continue_stmt = keyword("continue").ignore_then(
            keyword("until")
                .ignore_then(continue_target)
                .map(Stmt::ContinueUntil)
                .or(empty().to(Stmt::ExecutionControl(ExecutionControl::Continue))),
        );

        let next = keyword("next").to(Stmt::ExecutionControl(ExecutionControl::Next));
        let step_in = keyword("stepIn").to(Stmt::ExecutionControl(ExecutionControl::StepIn));
        let step_out = keyword("stepOut").to(Stmt::ExecutionControl(ExecutionControl::StepOut));

        let print = keyword("print")
            .ignore_then(
                keyword("eval")
                    .ignore_then(string())
                    .map(Print::Eval)
                    .or(string()
                        .try_map(|s, span| parse_template(&s).map_err(|m| Rich::custom(span, m)))
                        .map(Print::Template)),
            )
            .map(Stmt::Print);

        let launch = keyword("launch")
            .ignore_then(json_value())
            .map(Stmt::Launch);

        let break_stmt = keyword("break")
            .ignore_then(padded(
                any()
                    .filter(|c: &char| !c.is_whitespace() && *c != ':' && *c != '{' && *c != '}')
                    .repeated()
                    .at_least(1)
                    .collect::<String>()
                    .then_ignore(just(':'))
                    .then(text::int(10).try_map(|s: &str, span| {
                        s.parse::<i64>()
                            .map_err(|e| Rich::custom(span, format!("invalid line: {e}")))
                    })),
            ))
            .map(|(file, line)| Stmt::Break { file, line });

        let query = choice((
            keyword("locals").to(Query::Locals),
            keyword("stack").to(Query::Stack),
            keyword("frame").to(Query::Frame),
            keyword("eval").ignore_then(string()).map(Query::Eval),
        ));
        let dump = keyword("dump")
            .ignore_then(query)
            .then_ignore(keyword("as"))
            .then_ignore(keyword("json"))
            .then(keyword("depth").ignore_then(int_u64()).or_not())
            .map(|(query, depth)| Stmt::Dump {
                query,
                depth: depth.map(|d| d as usize),
            });

        choice((
            connect,
            expect,
            let_stmt,
            if_stmt,
            repeat,
            step_stmt,
            continue_stmt,
            next,
            step_in,
            step_out,
            print,
            dump,
            launch,
            break_stmt,
        ))
    });

    padding()
        .ignore_then(stmt.repeated().collect::<Vec<_>>())
        .then_ignore(end())
}

/// Split a `print` template into runs of literals and `{expr}` interpolations.
///
/// The outer string was already unescaped, so braces and any quotes inside an
/// interpolation are literal here. `{{` and `}}` emit a literal brace. A bare
/// `}` only closes an interpolation when it is outside a quoted string, so
/// `{eval "a}b"}` keeps its brace.
fn parse_template(s: &str) -> Result<Vec<StrPart>, String> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Double brace emits a brace.
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                literal.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                literal.push('}');
            }
            '{' => {
                if !literal.is_empty() {
                    parts.push(StrPart::Literal(std::mem::take(&mut literal)));
                }

                let mut inner = String::new();
                let mut in_string = false;
                let mut closed = false;

                while let Some(ic) = chars.next() {
                    match ic {
                        '"' => {
                            in_string = !in_string;
                            inner.push(ic);
                        }
                        '\\' if in_string => {
                            inner.push(ic);
                            if let Some(next) = chars.next() {
                                inner.push(next);
                            }
                        }
                        '}' if !in_string => {
                            closed = true;
                            break;
                        }
                        _ => inner.push(ic),
                    }
                }
                if !closed {
                    return Err(
                        "unterminated `{` in print template; use `{{` for a literal brace"
                            .to_string(),
                    );
                }
                if inner.trim().is_empty() {
                    return Err("empty interpolation `{}` in print template".to_string());
                }
                parts.push(StrPart::Interpolated(parse_expr(inner.trim())?));
            }
            '}' => {
                return Err(
                    "unexpected `}` in print template; use `}}` for a literal brace".to_string(),
                );
            }
            _ => literal.push(c),
        }
    }
    if !literal.is_empty() {
        parts.push(StrPart::Literal(literal));
    }
    Ok(parts)
}

/// Parse a single expression, used for the contents of a `{...}` interpolation.
fn parse_expr(s: &str) -> Result<Expr, String> {
    expr_parser()
        .then_ignore(end())
        .parse(s)
        .into_result()
        .map_err(|errors| {
            errors
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        })
}

/// Turn chumsky's spanned errors into a located, caret-pointed message.
fn format_errors(src: &str, errors: &[Rich<char>]) -> String {
    errors
        .iter()
        .map(|error| {
            let offset = error.span().start;
            let (line_no, column, line) = locate(src, offset);
            let caret = format!("{}^", " ".repeat(column - 1));
            format!("parse error at line {line_no}, column {column}:\n  {line}\n  {caret}\n{error}")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Map a byte offset to a one-based line and column plus that source line.
fn locate(src: &str, offset: usize) -> (usize, usize, &str) {
    let mut line_start = 0;
    let mut line_no = 1;

    for (i, c) in src.char_indices() {
        if i >= offset {
            break;
        }
        if c == '\n' {
            line_no += 1;
            line_start = i + 1;
        }
    }

    let line_end = src[line_start..]
        .find('\n')
        .map(|p| line_start + p)
        .unwrap_or(src.len());

    (line_no, offset - line_start + 1, &src[line_start..line_end])
}
