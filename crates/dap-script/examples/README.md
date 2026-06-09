# dap-script examples

Runnable example scripts for dap-script, against three small debug targets that
are built to be watched: a Sieve of Eratosthenes in Python, Go, and Rust. Each
keeps deliberately rich, mutating state — a list of booleans, a nested map of
lists, and loop scalars — so there is plenty for a debugger to inspect.

Everything here runs against a live session you start locally.

- the Python target needs debugpy importable by a Python in your PATH
- the Go target needs delve (`dlv`) on PATH or in `$(go env GOPATH)/bin`
- the Rust target needs `lldb-dap`, which ships with LLVM and Xcode, the binary built first with `rust/build.sh`, and `RUST_SYSROOT` exported with `export RUST_SYSROOT="$(rustc --print sysroot)"` so the launch can load the toolchain's data formatters

You can either run directly against the debugger or against a mux.
This means either dap-mux or the original Python dmux.

## Two ways to run

The examples come in two kinds. A launching script drives a session from
scratch. The observer scripts join a session that is already parked.

### Launch it yourself, solo, no mux

dap-script can be the launching client. Start only an adapter, then point a
launch script straight at it. This is the editor-free, automation-friendly
path. From this directory:

    # Python
    python3 -m debugpy.adapter --host 127.0.0.1 --port 5678 &
    dap-script python/launch.daps --address 5678 | jq

    # Go
    dlv dap --listen=127.0.0.1:5678 &
    dap-script go/launch.daps --address 5678 | jq

    # Rust
    rust/build.sh
    lldb-dap --connection listen://127.0.0.1:5678 &
    # The Rust path is needed to load the pretty printers into lldb.
    export RUST_SYSROOT="$(rustc --print sysroot)"
    dap-script rust/launch.daps --address 5678 | jq

### Through a mux, shared

Put a mux in front so other clients can watch the same session. The launch
script leaves the session parked when it exits, so other tools can join.

    ./start-session.sh python              # terminal 1: adapter + mux
    dap-script python/launch.daps          # terminal 2: launches and parks
    dap-script python/probe-locals.daps | jq   # terminal 3: observer joins

Swap `python` for `go` or `rust`. For `rust`, export `RUST_SYSROOT` in the
terminal that runs the launch script, as in the solo case above. If you debug
from an editor instead, skip the launch script, do a remote-attach to
`127.0.0.1:5679` and launch with a breakpoint at the line each example names,
then run the observer scripts.

`dap-script` above is the built binary. If you have not installed it, run it
from the workspace with `cargo run -p dap-script --` in place of `dap-script`.

## The examples

Each target parks at the line where it crosses off a multiple: `sieve.py:48`,
`sieve.go:27`, `sieve.rs:26`.

- `launch.daps` — dap-script as the launching client. It launches the target,
  sets the breakpoint, parks, and dumps locals. Works solo or through a mux.
- `probe-locals.daps` — the observer role. Join a parked session and
  `dump locals as json`, piped through `jq`.
- `where.daps` — a prose summary of the current frame from `print` interpolation.
- `step-and-observe.daps` — a single `next` that moves the shared program. Attach
  a `dap-tui 5679` alongside and watch it follow, then see the session stay alive
  after this script disconnects.
- `walk-to-prime.daps` — the replay shape. Join a parked session and advance
  with `continue until eval "p" == "3"`, driving the shared program to a target
  state, then dump the nested `crossed_off` / `crossedOff` map as JSON.
- `serialize.daps` (Python) — the eval serialization escape hatch.
  `print eval "json.dumps(...)"` lets the runtime serialize a rich value.

## Adapting between targets

The dap-script language and the session contract are identical across all three.
What differs is the debuggee's own vocabulary.

Each language's debugger takes its own `launch` configuration.
debugpy takes `program` and `console`, delve takes `mode`, `program`, and `dlvCwd`, lldb takes `program`.

The scripts are otherwise the same across all three targets. The same
`eval`-driven conditions, frame reads, and structured dumps work against every
adapter. This is because dap-script asks each one to evaluate in the protocol's watch
context, which returns a clean value rather than a console echo.
