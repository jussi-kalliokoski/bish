<p align="center">
  <img src="bish.svg" width="120" height="120" alt="bish logo">
</p>

<h1 align="center">bish</h1>

<p align="center"><em>the batteries-included shell</em></p>

bish is a shell written from scratch in Rust, with zero external
dependencies. For now, it's a simple bash-compatible shell: pipelines,
redirects, control flow, functions, arrays, arithmetic, globbing,
command/process substitution, and the usual builtins all work as
drop-in replacements for their bash counterparts.

That's just the starting point. The plan is to keep growing bish's
batteries — closing remaining bash compatibility gaps and, over time,
adding conveniences bash doesn't have out of the box.

## Building

```sh
cargo build --release
```

## Running

```sh
./target/release/bish              # interactive REPL
./target/release/bish --promoted   # interactive REPL, starting in windowed/tabbed mode
./target/release/bish script.sh    # run a script
./target/release/bish -c 'echo hi' # run a one-liner
```

## Status

Early days. Expect rough edges — issues and PRs welcome.
