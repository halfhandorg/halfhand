# ADR-0001: Threads + channels for the recorder (not tokio)

- **Status:** Accepted
- **Date:** 2026-07-04
- **Deciders:** Halfhand engineering
- **SRS context:** §6 lists `tokio (or threads + channels — implementer's
  choice, justify in ADR)` for `hh-record`.

## Context

`hh-record` must concurrently drive three independent I/O sources during a
recording:

1. a PTY (read agent stdout, forward stdin, handle resize/signals),
2. a recursive filesystem watcher (`notify`) for file changes,
3. (optionally) a stdio MCP proxy forwarding JSON-RPC both ways,
4. and feed all of them into the single-writer SQLite task.

We need to choose a concurrency model for the recorder crate. The two
realistic options are:

- **A. `tokio`** — an async runtime; each source becomes an `async` task
  multiplexed on a thread pool, with `tokio::sync::mpsc` feeding the writer.
- **B. OS threads + `std::sync::mpsc`** — one thread per source, plus the
  writer thread, communicating through standard channels.

## Decision

We choose **B: OS threads + `std::sync::mpsc`** for v0.1.

## Justification

1. **Concurrency shape is small and fixed.** A recording has a bounded,
   known set of long-lived workers (PTY reader, PTY writer, FS watcher, MCP
   proxy sides, SQLite writer). There is no fan-out to thousands of tasks. The
   cost/benefit of an async runtime does not pay off here: we would be paying
   the runtime's complexity to multiplex a handful of blocking I/O streams.
2. **All real I/O here is blocking.** `portable-pty` is a blocking PTY API;
   `notify` delivers events through a blocking channel; SQLite writes are
   blocking. Wrapping blocking I/O in `spawn_blocking` defeats the purpose of
   async and adds thread-pool juggling. Direct threads match the I/O model
   exactly.
3. **The single-writer rule maps directly to a thread + channel.** CLAUDE.md
   mandates "single-writer for SQLite (one writer task fed by an mpsc channel)
   … Never share a `Connection` across threads." That is literally one thread
   draining one `std::sync::mpsc::Receiver`. Introducing `tokio` would require
   a runtime, a `tokio::sync::mpsc`, and `spawn`ing the writer — more moving
   parts for the same guarantee, and a runtime handle that must be threaded
   across the FFI/PTY boundary.
4. **Smaller, more auditable binary — relevant to NFR-2.** No async runtime
   means fewer transitive crates and a smaller attack surface for a tool whose
   core promise is "never talks to the network." `tokio` pulls a large dependency
   graph with features (time, net, process, fs) we do not need.
5. **MCP proxy latency (NFR-1, < 5 ms p50) is fine with threads.** Two threads
   (one per stdio direction) forwarding bytes add negligible overhead; there is
   no scheduler poll, no task wakeup latency.
6. **Cancellation is straightforward.** Agent exit or SIGTERM tears down the
   recorder by joining the workers; no runtime to shut down, no task abort
   machinery.

## Consequences

- The recorder will use `std::thread` + `std::sync::mpsc` (or `crossbeam`
  channels if a multi-producer detail calls for it). `signal-hook` for
  SIGTERM/SIGINT.
- We forgo structured concurrency primitives (`tokio::select!`, cancellation
  tokens). We will emulate what we need with channel close + `JoinHandle`,
  which is adequate for this fixed topology.
- **Reversibility:** if v0.2+ adds many concurrent MCP servers, streaming
  adapters, or a network-facing feature (none currently in scope — SRS §8
  excludes cloud/network), we would revisit this and likely adopt `tokio` at
  that boundary only, keeping `hh-core` runtime-free.

## SRS deviation flagged

The SRS leaves this open ("implementer's choice"). This ADR is the required
justification. No SRS requirement is violated by either choice.