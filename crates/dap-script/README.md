# dap-script

Run a script against a live DAP session and exit.

dap-script is a simple language to talk to DAP compatible debuggers and tools.
It plays two roles. As an observer it can join a session over a shared mux, and
see the program where everyone left it. As a launching client it starts the
program itself and sets breakpoints, the role an editor usually fills. Because
it speaks plain DAP, the connect target can be a mux or a bare adapter. Pointed
at a mux the launched session is shared. Pointed at an adapter dap-script drives
it solo, with no editor and no mux, which is what makes it useful for automated
debugging.

Use cases:
- Record a set of debugging steps to repeat later. Possibly as a regression check
or to pass those steps on to someone else.
- Replay a set of steps to quickly get to a known state.
- Debug a program and dumps its state to a json file to be used to create a fixture
for use in testing.

## Running

    dap-script script.daps
    dap-script script.daps --address 5680
    dap-script script.daps --address debughost:5679

The script file is the one required argument. `--address` overrides the address
in the script's `connect`, so a script can be pointed at a different DAP adapter
without editing it. With no address anywhere, dap-script connects to `127.0.0.1:5679`.

The process exits zero when the script runs to its end. It exits non-zero on a
parse error, a failed connection, or an `expect` timeout. Diagnostics go to
stderr. Only `print` and `dump` write to stdout, so piping stdout to another
tool gives you just the data.

## The language

A script is a sequence of statements. Whitespace is insignificant and `#` begins
a line comment.

### Connecting and synchronizing

`connect` opens the session. With no argument it uses the default address. With a
bare port or a `host:port` it uses that instead.

    connect
    connect 5680
    connect "debughost:5679"

The connect target can be a mux or a bare debug adapter such as `debugpy.adapter`,
`dlv dap`, or `lldb-dap`. dap-script speaks the same DAP either way.

`expect stopped` is the synchronization point. The mux replays the last stop to a
client when it joins, so a session that is already parked satisfies it at once. A
running session waits for the next stop, bounded by a timeout so a script never
wedges. A session that has ended fails the statement.

    connect
    expect stopped

### Launching a session

`launch` makes dap-script the launching client. It carries an adapter launch
configuration as JSON, forwarded to the adapter unchanged, so dap-script needs to
know nothing about the adapter. `break <file>:<line>` registers a breakpoint. A
relative break path resolves against the script file's directory. Two
placeholders expand in the launch configuration so a script carries no absolute
paths: `${dir}` becomes the script file's directory, and `${env:NAME}` becomes
the value of an environment variable. A reference to an unset variable fails the
launch rather than passing a broken value to the adapter. `${env:NAME}` reaches a
machine-specific path the script cannot know, such as a toolchain sysroot whose
location varies per machine.

    connect
    launch {
      "request": "launch",
      "type": "python",
      "program": "${dir}/sieve.py",
      "console": "internalConsole"
    }
    break sieve.py:48
    expect stopped
    dump locals as json

`launch` and `break` are setup. They record intent and may appear in either
order. The handshake itself runs at the first `expect stopped`: dap-script sends
`launch`, waits for the adapter to initialize, sets the breakpoints, sends
`configurationDone`, and the program runs to the first breakpoint. A launch the
adapter rejects fails with its error.

A script that never uses `launch` stays a pure observer. dap-script never sends
`terminate`, and never advertises `supportsRunInTerminalRequest`, so a launch
configuration should use an internal-console mode where the adapter would
otherwise need a reverse `runInTerminal`.

### Reading the current stop

`frame.line`, `frame.name`, and `frame.source` read the current top frame.
`eval "expr"` evaluates an expression in the current frame and yields its result.
These appear on the right of `let`, inside conditions, and inside `print`
interpolations.

    let start = frame.line
    let count = eval "len(items)"

### Bindings, conditions, and control flow

`let` binds a value for use later. `if`/`else` branches on a comparison.
`repeat N` runs a block a fixed number of times.

    let n = eval "n"
    if frame.line == 88 {
      print "reached the spot"
    } else {
      next
    }
    repeat 3 { next }

`step until <cond>` single-steps, checking the condition before each step, until
it holds. It steps into calls, so it suits a data condition you are waiting on
rather than reaching a line. To reach a line, use `continue until line <n>`,
which resumes the program and checks after each stop, so it relies on a
breakpoint to stop at the line. Both stop early if the session ends.

    step until eval "ready" == "True"
    continue until line 200

Conditions compare two expressions with `==`, `!=`, `<`, `<=`, `>`, or `>=`. A
comparison is numeric when both sides parse as integers and lexicographic
otherwise.

### Navigation

`step` and `stepIn` step into a call. `next` steps over it. `stepOut` runs to the
caller. `continue` resumes the program. Each one drives the shared session, so a
step here moves the program for every connected client.

### Output

`print` writes a line to stdout. A template interpolates `{...}` expressions into
prose. `print eval "expr"` emits whatever the expression returns.

    print "n={eval \"n\"} at line {frame.line}"
    print eval "json.dumps(obj, default=str)"

`dump <query> as json` writes the queried state to stdout as JSON for piping.
`locals` and `eval "expr"` produce a variable tree where each node carries name,
value, type, and children. `stack` and `frame` produce the frame-shaped state of
the current stop.

    dump locals as json
    dump eval "request" as json
    dump stack as json

## Depth and the breadth limitation

A variable tree can be deep or cyclic, so `dump` applies a default depth cap of
three child levels. `dump locals as json depth 5` overrides it. At the cap, a
node that still has children is marked `"truncated": true` rather than expanded,
so a consumer can tell the cut from a real leaf.

Breadth is not capped. The engine fetches all of a node's children in one request
and does not honor the protocol's paging hints, so a dump pointed at a very large
collection fetches the whole collection. The depth cap bounds tree depth, not
collection width.

## Serializing rich values with eval

The variable tree shows the display strings the adapter provides, which are
useless for rich objects. Rather than fight that, let the debuggee serialize
itself. `print eval "json.dumps(obj, default=str)"` returns a clean JSON string
produced by the program's own runtime.

`print eval "..."` emits the value the expression returns. `dump ... as json`
walks the structured node tree. When a value expands cleanly in the tree, prefer
`dump eval "x" as json`, which emits bare JSON. Reach for the `json.dumps` form
for values the tree cannot render faithfully, where the runtime's own serialization
is the only good rendering.

One adapter wrinkle to know about. An adapter returns the rendered result of an
evaluation, and some render a string result as the language's own repr. debugpy
does this, so `print eval "json.dumps(obj)"` comes back wrapped in single quotes,
valid JSON inside a Python string literal rather than bare JSON. Strip the outer
quotes before piping to jq, or prefer `dump eval ... as json` when the value
expands in the tree.

Evaluation occurs in the live debugger. It can have whatever side effects possible
just like when you are typing into the debugger directly. Be mindful.

## Example

    # Walk a fresh session to the interesting iteration and dump it.
    connect
    expect stopped
    continue until line 142
    repeat 5 { next }
    print "arrived at line {frame.line} in {frame.name}"
    dump locals as json

For complete, runnable examples against some simple targets, see the
`examples` directory.
