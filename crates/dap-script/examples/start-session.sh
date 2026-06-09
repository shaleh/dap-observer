#!/bin/sh
# Start a debug session for a DAP adapter with a dap-mux
# attached in front of it.
#
#   ./start-session.sh python     # debugpy.adapter + mux
#   ./start-session.sh go         # dlv dap + mux
#   ./start-session.sh rust       # lldb-dap + mux  (build first: rust/build.sh)
#
# The mux defaults to dmux, the Python dap-mux. Select a different one with MUX,
# which is how you validate another implementation against the same flow:
#   MUX=dap-mux ./start-session.sh rust
#
# Then, in another terminal, launch a target with dap-script itself:
#   dap-script python/launch.daps              # launches and parks the sieve
#   dap-script python/probe-locals.daps | jq   # observer joins the parked session
# or drive from your editor with a debug remote call to 127.0.0.1:5679 and performing a launch.
#
# For the solo case you do not need this script at all. Start just an adapter
# and point a launch runbook straight at it, see python/launch.daps.
#
# Prerequisites:
#   - a DAP mux on PATH, dmux by default or whatever MUX names
#   - python target: debugpy importable by $PYTHON
#   - go target:     dlv on PATH or in $(go env GOPATH)/bin
#   - rust target:   lldb-dap on PATH, and the binary built with rust/build.sh.
#                    The launch script also needs RUST_SYSROOT exported in its
#                    own terminal: export RUST_SYSROOT="$(rustc --print sysroot)"
cd "$(dirname "$0")" || exit 1

TARGET="$1"
if [ "$TARGET" != "python" ] && [ "$TARGET" != "go" ] && [ "$TARGET" != "rust" ]; then
    echo "usage: $0 python|go|rust" >&2
    exit 2
fi

# Resolve tools without depending on the launching shell's PATH, which may be
# bare when started from an editor.
MUX_BIN="${MUX:-dmux}"
MUX_PATH="$(command -v "$MUX_BIN" || true)"
if [ -z "$MUX_PATH" ]; then
    echo "error: mux '$MUX_BIN' not found on PATH." >&2
    if [ "$MUX_BIN" = dmux ]; then
        echo "install it with: uv tool install dap-mux --with debugpy" >&2
    fi
    echo "or select another with MUX=<name>, e.g. MUX=dap-mux." >&2
    exit 1
fi

ADAPTER=""
MUX_PID=""
cleanup() {
    [ -n "$MUX_PID" ] && kill "$MUX_PID" 2>/dev/null
    [ -n "$ADAPTER" ] && kill "$ADAPTER" 2>/dev/null
    exit 0
}
trap 'printf "\nstopping...\n"; cleanup' INT TERM

case "$TARGET" in
python)
    PYTHON="${PYTHON:-python3}"
    echo "debugpy.adapter  -> 127.0.0.1:5678"
    "$PYTHON" -m debugpy.adapter --host 127.0.0.1 --port 5678 > /tmp/adapter.log 2>&1 &
    ADAPTER=$!
    sleep 2
    ;;
go)
    DLV="$(command -v dlv || true)"
    if [ -z "$DLV" ]; then
        echo "error: dlv not found. install it with:" >&2
        echo "  go install github.com/go-delve/delve/cmd/dlv@latest" >&2
        exit 1
    fi
    echo "dlv dap          -> 127.0.0.1:5678"
    "$DLV" dap --listen=127.0.0.1:5678 > /tmp/adapter.log 2>&1 &
    ADAPTER=$!
    sleep 1
    ;;
rust)
    LLDB_DAP="$(command -v lldb-dap || true)"
    if [ -z "$LLDB_DAP" ]; then
        echo "error: lldb-dap not found. it ships with LLVM and Xcode." >&2
        exit 1
    fi
    echo "lldb-dap         -> 127.0.0.1:5678"
    "$LLDB_DAP" --connection listen://127.0.0.1:5678 > /tmp/adapter.log 2>&1 &
    ADAPTER=$!
    sleep 1
    ;;
esac

# dmux starts an IPython REPL unless told not to. dap-mux is headless already
# and serves a TUI only with --ui, so it has no such flag.
case "$(basename "$MUX_BIN")" in
dmux) HEADLESS="--headless" ;;
*)    HEADLESS="" ;;
esac

echo "$MUX_BIN (attach) -> 127.0.0.1:5679   (Ctrl-C to stop)"
"$MUX_PATH" --attach 5678 $HEADLESS -p 5679 &
MUX_PID=$!

wait "$MUX_PID"
cleanup
