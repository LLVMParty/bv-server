# Architecture

This repository has three layers:

1. **Client libraries** build typed QF_BV/Bool formulas without linking to a solver.
2. **`bv-wire`** serializes those formulas into a compact, validated binary protocol.
3. **`bv-server`** owns solver/simplifier backends and returns models, cores, simplified terms, or optimum values.

The server is intentionally stateless at the protocol boundary. Every request contains the complete expression DAG, assertions, assumptions, command, flags, and budget. That makes requests cacheable, replayable, and easy to compare across backends.

## Repository components

| Path | Role |
|---|---|
| `crates/bv-wire` | Rust protocol/client crate. It owns wire constants, expression validation, request/response codecs, high-level Rust `Context`/term APIs, and a TCP client. |
| `crates/bv-server` | TCP server, binary/text dispatch, response cache, backend trait, backend racing, and integrations for Z3, binbit, qfbvsmtrs, and Rumba. |
| `crates/bv-smtlib` | Shared solver-agnostic QF_BV/Bool SMT-LIB frontend. It parses with `yaspar` and lowers through a sink trait. |
| `crates/qfbvsmtrs` | Standalone pure-Rust QF_BV solver. It can be used directly, through its SMT-LIB frontend, or as a server backend through the `bv-wire` bridge. |
| `python` | Dependency-free Python client package (`bv_wire.py`) plus examples/tests. Install from this repo with pip's `#subdirectory=python` support. |
| `cpp` | Header-only C++17 client package exposing the `bv_wire::bv_wire` CMake `INTERFACE` target plus examples/tests. |

## Request flow

### Binary clients

```text
Python / C++ / Rust Context
        |
        v
bv-wire expression buffer + request envelope
        |
        v
length-prefixed TCP frame
        |
        v
bv-server binary decoder + validator
        |
        v
optional response cache
        |
        v
command router / backend(s)
        |
        v
binary response envelope + payload
```

The binary path is the primary API. Clients construct a typed DAG and send one length-prefixed frame. The server validates the frame before any backend sees it.

### SMT-LIB text

```text
one complete SMT-LIB script
        |
        v
shared QF_BV frontend (`bv-smtlib`)
        |
        v
wire request or qfbvsmtrs fallback query
        |
        v
backend(s)
        |
        v
SMT-LIB text response
```

Text requests are for compatibility with external tools and tests. They use the same transport framing as binary requests. The server tries to lower supported QF_BV scripts into a wire request; when the selected backend supports it, scripts outside the server text subset can fall back to qfbvsmtrs' standalone SMT-LIB path with a bounded default budget.

## Default server backend graph

The shipped binary in `crates/bv-server/src/main.rs` builds this backend stack:

```text
CommandRouterBackend
├── SIMPLIFY  -> RumbaBackend
└── SOLVE / MINIMIZE / MAXIMIZE
    -> RacingBackend(default budget: 30s)
       ├── Z3Backend
       ├── BinbitBackend
       └── QfbvsmtrsBackend
```

Backend responsibilities:

- `Z3Backend` translates validated wire IR to the Rust `z3` crate and supports solve, model extraction, named unsat cores, and bit-hunt optimization.
- `BinbitBackend` translates validated wire IR to `binbit` and supports solve, model extraction, named unsat cores, and optimization helpers.
- `QfbvsmtrsBackend` lowers wire requests into the standalone qfbvsmtrs IR and uses the pure-Rust bit-blast/SAT pipeline.
- `RumbaBackend` handles `SIMPLIFY` for supported 64-bit-or-smaller MBA expression islands. Unsupported simplifications return the original target expression rather than a wrong rewrite.

`RacingBackend` returns the first conclusive answer and logs later disagreements for investigation. `UNKNOWN` is safe and means no backend produced a conclusive answer within the applicable budget.

## Service safeguards

The server has explicit bounds for hostile or accidental large inputs:

- maximum request frame size;
- maximum response frame size;
- maximum active connections;
- bounded response cache entries/key sizes/response sizes;
- request/response validation before cache insertion or client-side decoding;
- optional read/write timeouts;
- per-request backend budgets.

The server also records handled binary request/response pairs into a SQLite database at `~/.bv-server/recordings.db` by default. `BV_SERVER_RECORD_DB` overrides the path; setting it to an empty value disables recording. Each row is keyed by the BLAKE3 hash of the canonical request (`INSERT OR IGNORE` dedupes, leaving existing rows unchanged), and stores the canonical request and response blobs plus the name of the backend that produced the response (`QueryResult::backend`, filled in by wrapper backends such as the racing and command-router backends via `QueryResult::attributed_to`, so it names the leaf that actually answered). SMT-LIB text requests are recorded after lowering to binary wire requests. The recorder zeroes request/response IDs before hashing or storage.

Recording is synchronous: the server attempts the SQLite insert before returning the response from the dispatcher. The database opens in WAL mode with `synchronous=NORMAL` and a busy timeout, so multiple server instances on the same host can write to the same file (writes serialize; readers don't block). WAL relies on local filesystem locking and is not safe over network mounts. Stored rows always use the current magics: legacy `SMTQ` requests are upgraded before they reach the cache or the recorder, and a database written by smt-server (`PRAGMA user_version` 0) is rewritten and re-keyed once when opened. If the default database does not exist, the server first copies `~/.smt-server/recordings.db` into place (read-only, via `VACUUM INTO`). The one-time importer reads the legacy `~/.smt-server/requests` file tree: run `bv-server migrate-recordings [SOURCE_DIR] [DB_PATH]` to fold an existing tree into the database.

The client libraries also enforce sort/width/context checks during construction so malformed requests are normally caught before serialization.

## Scope and non-goals

Supported logic is quantifier-free bit-vectors plus Booleans. Arrays, floating point, quantifiers, and uninterpreted functions are not part of the wire protocol or the default server contract.

The protocol is not an incremental solver session API. `push`/`pop` in binary clients is client-side bookkeeping over assertions; the server receives one self-contained query at a time.
