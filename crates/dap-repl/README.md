# dap-repl

A small interactive console for poking at a running program that's paused at a breakpoint.
Type an expression, see its value:

```
(dap) len(primes)
=> 7 : int
(dap) self.count * 2
=> 24 : int
```

It connects to a [dap-mux](https://github.com/dap-mux/dap-mux) session your editor is
already debugging — you don't launch the program or set breakpoints from here, you just
inspect (and drive) wherever it's stopped.

```sh
dap-repl                # connect to 127.0.0.1:5679 (the default)
dap-repl 5680           # a different port
dap-repl host:port      # a different host and port
```

## Heads up: this can change your program

The expressions you type run for real inside the paused program. Most of the time you're
just reading a value — but nothing stops you from typing `x = 5` or calling a function
that has side effects, and that will actually happen. Since the debugger is shared, those
changes show up for your editor too.

So `dap-repl` is a hands-on tool, not a safe read-only window — if you want to look without
any chance of touching anything, use `dap-observer` instead.
