//! dap-script provides a DSL for interacting with DAP adapters.

mod ast;
mod dump;
mod interpreter;
mod parser;
mod session;
mod value;

use std::path::PathBuf;

use clap::Parser;

/// Run a dap-script file against a DAP session.
#[derive(Parser)]
#[command(name = "dap-script", version, about, long_about = None)]
struct Args {
    /// Script file to run.
    #[arg(value_name = "script.daps")]
    script: PathBuf,

    /// The Adapter address as host:port, or a bare port that assumes 127.0.0.1. When
    /// given, it overrides the address in the script's `connect`.
    #[arg(short, long, value_name = "host:port | port")]
    address: Option<String>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let source = match std::fs::read_to_string(&args.script) {
        Ok(source) => source,
        Err(e) => {
            eprintln!("could not read {}: {e}", args.script.display());
            std::process::exit(2);
        }
    };

    let program = match parser::parse_program(&source) {
        Ok(program) => program,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // Breakpoint paths and the `${dir}` placeholder resolve against the script's
    // own directory. The adapter needs absolute paths, so anchor to the absolute
    // location of the script file rather than however it was named on the CLI.
    let script_path = std::fs::canonicalize(&args.script).unwrap_or_else(|_| args.script.clone());
    let script_dir = script_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let mut interpreter = interpreter::Interpreter::new(args.address, script_dir);
    if let Err(e) = interpreter.run(&program).await {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
