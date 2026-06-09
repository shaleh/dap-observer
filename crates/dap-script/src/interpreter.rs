//! Executing a parsed script against a live session.
//!
//! Flow is sequential and top to bottom. A statement reads or dumps the current
//! stop, binds a value, branches, loops, or drives execution. Diagnostics go to
//! stderr. Only `print` and `dump` write to stdout, so their output can be piped
//! to another tool cleanly.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, Result, bail};

use crate::{
    ast::{Cond, Expr, Stmt, Until},
    session::Session,
    value::{self, Value},
};

/// State tracking data.
pub struct Interpreter {
    /// An override from the command line, applied to every `connect` so a
    /// script can be pointed at a different adapter without editing it.
    address_override: Option<String>,
    /// The directory of the script file. Breakpoint paths and the `${dir}`
    /// launch-config placeholder resolve against it, so a script carries no
    /// absolute paths.
    script_dir: PathBuf,
    session: Option<Session>,
    bindings: HashMap<String, Value>,
    /// A `launch` configuration recorded but not yet sent. The handshake runs
    /// lazily when the script first synchronizes on a stop.
    pending_launch: Option<serde_json::Value>,
    /// Breakpoints registered by `break`, grouped by source file in order.
    breakpoints: Vec<(String, i64)>,
}

impl Interpreter {
    pub fn new(address_override: Option<String>, script_dir: PathBuf) -> Interpreter {
        Interpreter {
            address_override,
            script_dir,
            session: None,
            bindings: HashMap::new(),
            pending_launch: None,
            breakpoints: Vec::new(),
        }
    }

    /// Run a whole script, then disconnect without ending the shared session.
    pub async fn run(&mut self, program: &[Stmt]) -> Result<()> {
        let mut result = self.exec_block(program).await;
        // A launch that was set up but never synchronized never ran. Surface
        // that rather than exiting zero having done nothing.
        if result.is_ok() && self.pending_launch.is_some() {
            result = self.require_synchronized();
        }
        if let Some(session) = &self.session {
            session.disconnect().await;
        }
        result
    }

    fn exec_block<'a>(
        &'a mut self,
        statements: &'a [Stmt],
    ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            for statement in statements {
                self.exec_stmt(statement).await?;
            }
            Ok(())
        })
    }

    async fn exec_stmt(&mut self, statement: &Stmt) -> Result<()> {
        match statement {
            Stmt::Connect(address) => eval::connect(self, &address.as_deref()).await,
            Stmt::ExpectStopped => eval::stopped(self).await,
            Stmt::Let { name, value } => eval::let_binding(self, name, value).await,
            Stmt::If {
                cond,
                then_block,
                else_block,
            } => eval::if_block(self, cond, then_block, else_block).await,
            Stmt::Repeat { count, body } => eval::repeat(self, *count, body).await,
            Stmt::StepUntil(until) => eval::drive_until(self, "stepIn", until).await,
            Stmt::ContinueUntil(until) => eval::drive_until(self, "continue", until).await,
            Stmt::ExecutionControl(command) => eval::drive(self, command.as_str()).await,
            Stmt::Print(print) => eval::print(self, print).await,
            Stmt::Dump { query, depth } => eval::dump(self, query, *depth).await,
            Stmt::Launch(config) => eval::set_launch(self, config).await,
            Stmt::Break { file, line } => eval::set_breakpoint(self, file, *line).await,
        }
    }

    /// Resolve a script-relative path against the script's directory, leaving an
    /// absolute path unchanged.
    fn resolve_path(&self, file: &str) -> String {
        let path = Path::new(file);
        if path.is_absolute() {
            file.to_string()
        } else {
            self.script_dir.join(path).to_string_lossy().into_owned()
        }
    }

    /// Perform the launch handshake the first time the script needs the program
    /// running or stopped. A no-op once launched, or when no launch was declared.
    async fn ensure_launched(&mut self) -> Result<()> {
        let Some(config) = self.pending_launch.take() else {
            return Ok(());
        };
        let breakpoints = std::mem::take(&mut self.breakpoints);
        self.session_mut()?.launch(config, breakpoints).await
    }

    async fn until_holds(&mut self, until: &Until) -> Result<bool> {
        match until {
            Until::Line(line) => Ok(self.resolved_session().await?.frame_line()? == *line),
            Until::Cond(cond) => self.eval_cond(cond).await,
        }
    }

    async fn eval_cond(&mut self, cond: &Cond) -> Result<bool> {
        let left = self.eval_expr(&cond.left).await?;
        let right = self.eval_expr(&cond.right).await?;
        Ok(value::compare(cond.op, &left, &right))
    }

    async fn eval_expr(&mut self, expr: &Expr) -> Result<Value> {
        match expr {
            Expr::Int(n) => Ok(Value::Int(*n)),
            Expr::Str(s) => Ok(Value::Str(s.clone())),
            Expr::Ident(name) => self
                .bindings
                .get(name)
                .cloned()
                .with_context(|| format!("unknown binding `{name}`")),
            Expr::Eval(expression) => Ok(Value::Str(
                self.resolved_session().await?.eval(expression).await?.value,
            )),
            Expr::FrameLine => Ok(Value::Int(self.resolved_session().await?.frame_line()?)),
            Expr::FrameName => Ok(Value::Str(self.resolved_session().await?.frame_name()?)),
            Expr::FrameSource => Ok(Value::Str(self.resolved_session().await?.frame_source()?)),
        }
    }

    fn session_mut(&mut self) -> Result<&mut Session> {
        self.session
            .as_mut()
            .context("the script used the session before `connect`")
    }

    /// The session, with the current stop resolved so frame reads and evaluation
    /// have something to resolve against.
    async fn resolved_session(&mut self) -> Result<&mut Session> {
        self.require_synchronized()?;
        let session = self.session_mut()?;
        session.ensure_resolved().await?;
        Ok(session)
    }

    /// Guard the paths that read or step against a launch that was set up but
    /// never run. `expect stopped` is the one place that fires the launch and
    /// waits for the first stop, so anything else needs it to have happened.
    fn require_synchronized(&self) -> Result<()> {
        if self.pending_launch.is_some() {
            bail!(
                "`launch` is set up but the program has not stopped yet. \
                 Add `expect stopped` after the breakpoints to run to the first one."
            );
        }
        Ok(())
    }
}

/// Expand placeholders in every string of a launch configuration, recursing
/// through arrays and objects. `${dir}` becomes the script's directory.
/// `${env:NAME}` becomes the value of that environment variable, which lets a
/// checked-in script reach a machine-specific path such as a toolchain sysroot
/// without hardcoding it. A reference to an unset variable is an error rather
/// than a silently broken path handed to the adapter.
fn expand_placeholders(value: &mut serde_json::Value, dir: &Path) -> Result<()> {
    match value {
        serde_json::Value::String(s) if s.contains("${") => *s = expand_string(s, dir)?,
        serde_json::Value::Array(items) => {
            for item in items {
                expand_placeholders(item, dir)?;
            }
        }
        serde_json::Value::Object(map) => {
            for v in map.values_mut() {
                expand_placeholders(v, dir)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn expand_string(input: &str, dir: &Path) -> Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unclosed placeholder is not one. Leave the rest verbatim.
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let token = &after[..end];
        if token == "dir" {
            out.push_str(&dir.to_string_lossy());
        } else if let Some(name) = token.strip_prefix("env:") {
            let value = std::env::var(name).with_context(|| {
                format!("launch config references ${{env:{name}}} but {name} is not set")
            })?;
            out.push_str(&value);
        } else {
            // An unknown placeholder stays verbatim so it surfaces in whatever
            // error the adapter raises, rather than vanishing.
            out.push_str("${");
            out.push_str(token);
            out.push('}');
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

mod eval {
    use anyhow::Result;
    use dap_client::dap;

    use crate::{
        ast::{Cond, Expr, Print, Query, Stmt, StrPart, Until},
        dump::{self},
        interpreter::{Interpreter, expand_placeholders},
        session::{Session, Settled},
    };

    pub async fn connect(interpreter: &mut Interpreter, address: &Option<&str>) -> Result<()> {
        let target = interpreter
            .address_override
            .clone()
            .or_else(|| address.map(str::to_string));
        let address = dap::resolve_addr(target.as_deref());
        let session = Session::connect(&address).await?;
        eprintln!("connected to {}", session.address());
        interpreter.session = Some(session);
        Ok(())
    }

    pub async fn set_launch(
        interpreter: &mut Interpreter,
        config: &serde_json::Value,
    ) -> Result<()> {
        let mut config = config.clone();
        expand_placeholders(&mut config, &interpreter.script_dir)?;
        interpreter.pending_launch = Some(config);
        Ok(())
    }

    /// Synchronize on a stop, launching first if a launch is pending.
    pub async fn stopped(interpreter: &mut Interpreter) -> Result<()> {
        interpreter.ensure_launched().await?;
        interpreter.session_mut()?.expect_stopped().await
    }

    pub async fn let_binding(
        interpreter: &mut Interpreter,
        name: &str,
        value: &Expr,
    ) -> Result<()> {
        let bound = interpreter.eval_expr(value).await?;
        interpreter.bindings.insert(name.to_string(), bound);
        Ok(())
    }

    pub async fn if_block(
        interpreter: &mut Interpreter,
        cond: &Cond,
        then_block: &[Stmt],
        else_block: &Option<Vec<Stmt>>,
    ) -> Result<()> {
        if interpreter.eval_cond(cond).await? {
            interpreter.exec_block(then_block).await
        } else if let Some(else_block) = else_block {
            interpreter.exec_block(else_block).await
        } else {
            Ok(())
        }
    }

    pub async fn repeat(interpreter: &mut Interpreter, count: u64, block: &[Stmt]) -> Result<()> {
        for _ in 0..count {
            interpreter.exec_block(block).await?;
        }
        Ok(())
    }

    /// Single-step or continue until the target holds, checking before each
    /// move so an already-satisfied target does nothing. A session that ends
    /// while looping stops the loop rather than failing, since there is nothing
    /// left to drive.
    pub async fn drive_until(
        interpreter: &mut Interpreter,
        command: &str,
        until: &Until,
    ) -> Result<()> {
        // Synchronize on a stop before reading the frame or driving, so a loop
        // written without a preceding `expect stopped` waits for the session
        // rather than failing with a bare "not stopped".
        stopped(interpreter).await?;
        loop {
            if interpreter.until_holds(until).await? {
                return Ok(());
            }
            if interpreter.session_mut()?.drive(command).await? == Settled::Ended {
                return Ok(());
            }
        }
    }

    pub async fn drive(interpreter: &mut Interpreter, command: &str) -> Result<()> {
        interpreter.require_synchronized()?;
        interpreter.session_mut()?.drive(command).await?;
        Ok(())
    }

    pub async fn print(interpreter: &mut Interpreter, print: &Print) -> Result<()> {
        let line = match print {
            Print::Eval(expression) => {
                interpreter
                    .resolved_session()
                    .await?
                    .eval(expression)
                    .await?
                    .value
            }
            Print::Template(parts) => {
                let mut out = String::new();

                for part in parts {
                    match part {
                        StrPart::Literal(text) => out.push_str(text),
                        StrPart::Interpolated(expr) => {
                            out.push_str(&interpreter.eval_expr(expr).await?.as_string())
                        }
                    }
                }
                out
            }
        };
        println!("{line}");
        Ok(())
    }

    pub async fn dump(
        interpreter: &mut Interpreter,
        query: &Query,
        depth: Option<usize>,
    ) -> Result<()> {
        let depth = depth.unwrap_or(dump::DEFAULT_DEPTH);
        let session = interpreter.resolved_session().await?;
        let client = session.client();
        let json = match query {
            Query::Locals => {
                let scope = session.locals_scope()?;
                serde_json::Value::Array(dump::children_to_json(client, scope, depth).await)
            }
            Query::Eval(expression) => {
                let node = session.eval(expression).await?;
                dump::node_to_json(client, &node, depth).await
            }
            Query::Stack => dump::stack_to_json(session.stack()?),
            Query::Frame => dump::frame_to_json(session.top_frame()?),
        };
        println!("{}", serde_json::to_string_pretty(&json)?);
        Ok(())
    }

    pub async fn set_breakpoint(
        interpreter: &mut Interpreter,
        file: &str,
        line: i64,
    ) -> Result<()> {
        interpreter
            .breakpoints
            .push((interpreter.resolve_path(file), line));
        Ok(())
    }
}
