# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Cargo workspace hosting two crates that share a common SocketCAN core, both MIT-licensed and meant to be published to crates.io:

- **`mcanbus`** — high-performance SocketCAN bindings for Linux. Pure libc, no async runtime. CAN 2.0 + CAN-FD, batched `recvmmsg`/`sendmmsg` I/O, netlink helpers (`set_up`/`set_down`/`state`/`cycle`), ISO 15765-2 (ISO-TP) request/response, optional multi-consumer fan-out reader. Extracted from the production code of [mcandump](https://github.com/mickeyl/mcandump) and [mcangen](https://github.com/mickeyl/mcangen).

- **`socketcan-mcp`** — Model Context Protocol server (rmcp 0.8) exposing SocketCAN to AI agents over stdio. Built on `mcanbus`. Tools: `list_interfaces`, `iface_state`, `capture`, `send_frame`, `send_and_capture`, `isotp_request`. Sandbox via `SOCKETCAN_MCP_INTERFACES` allowlist + `SOCKETCAN_MCP_READONLY=1`.

Pinned to Rust 1.94.1 via `rust-toolchain.toml`.

## Build and test

```bash
cargo build --release          # release build (LTO, stripped) for both crates
cargo build                    # debug build
cargo clippy --all-targets -- -D warnings
cargo fmt
cargo test -p mcanbus --lib    # unit tests only — no SocketCAN required
```

Library unit tests need no kernel support. Integration tests in `mcanbus/tests/integration_vcan.rs` are gated on environment variables and silently skip otherwise:

```bash
# Two adapters on the same wire (real hardware, the production setup):
MCANBUS_TEST_IFACE=can0 MCANBUS_TEST_IFACE_RX=can1 \
    cargo test --test integration_vcan -- --test-threads=1

# Pure software loopback:
sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
MCANBUS_TEST_IFACE=vcan0 cargo test --test integration_vcan -- --test-threads=1
```

The FD-TX test additionally requires `MCANBUS_TEST_FD=1` and a controller that actually supports CAN-FD transmission (gs_usb does *not* — it accepts the setsockopt but rejects FD frames at TX with `EINVAL`).

For ad-hoc testing the `examples/` directory has working `candump`, `cangen`, `multireader`, and `list_interfaces` binaries:

```bash
cargo run --example candump -- can0 --filter 7F0:7F0
cargo run --example cangen  -- can0 --rate 5000 --batch 32 --id 0x7F1 --data CAFEBABE
cargo run --example multireader -- can1 --workers 8
cargo run --example list_interfaces
```

The MCP server can be smoke-tested by piping JSON-RPC over stdio:

```bash
cargo build -p socketcan-mcp
SOCKETCAN_MCP_INTERFACES=can0,can1 \
    target/debug/socketcan-mcp < some-mcp-requests.jsonl
```

Both binaries need `CAP_NET_RAW` to open `CAN_RAW` sockets, and netlink helpers (`set_up`/`cycle`) additionally need `CAP_NET_ADMIN`.

## Architecture

```
mcanbus/                         workspace root
├── Cargo.toml                   workspace manifest
├── rust-toolchain.toml          pinned 1.94.1
├── examples/                    shared examples consumed by both crates
├── mcanbus/                     library crate
│   ├── src/
│   │   ├── lib.rs               module wiring + doctest
│   │   ├── consts.rs            re-exported linux/can.h constants for power users
│   │   ├── frame.rs             CanId, Frame, FdFlags, WireFrame, decode_wire
│   │   ├── socket.rs            Socket, OpenOpts, CanFilter, Timestamping
│   │   ├── iface.rs             Interface, CanState, list_can_interfaces, netlink
│   │   ├── isotp.rs             ISO 15765-2 request() with automatic SF/FF/CF + Flow Control
│   │   └── reader.rs            Reader, Subscriber (feature `reader`, default on)
│   └── tests/integration_vcan.rs  env-gated live tests
└── socketcan-mcp/               binary crate (MCP server)
    └── src/main.rs              5 tools, env-driven config
```

### `mcanbus::frame`

`Frame` is `Copy`, with a fixed 64-byte data buffer plus a length tag. Wastes 56 bytes for classic 8-byte frames; the trade is no allocation on the hot path. `WireFrame` is `repr(C)` mirroring `struct canfd_frame` for direct kernel interop. `decode_wire()` discriminates RTR / error / data via `CAN_RTR_FLAG` / `CAN_ERR_FLAG`. The `Display` impl produces candump-style output (`123#DEAD`, `123##1DEAD` for FD with BRS).

### `mcanbus::socket`

`Socket::open(iface, &OpenOpts)` resolves ifindex via `SIOCGIFINDEX`, configures `CAN_RAW_FD_FRAMES`, optional `CAN_RAW_FILTER` / `CAN_RAW_ERR_FILTER`, optional `SO_TIMESTAMPING` (with `SO_TIMESTAMPNS` fallback), `SO_RCVBUF`/`SO_SNDBUF`, `SO_RCVTIMEO`, and binds. `CanFilter` carries the kernel `{id, mask}` semantics including the `CAN_EFF_FLAG` distinction.

`recv()` returns `Option<Frame>` (None on timeout/EAGAIN/EINTR). `recv_decoded()` surfaces RTR and error frames separately. `recv_batch()` uses `recvmmsg` with `MSG_WAITFORONE`. `send()` writes a 16- or 72-byte buffer; `send_batch()` uses `sendmmsg`. EINTR is treated as a no-op (matches Rust `Read`/`Write` conventions and lets caller stop-flags break the loop).

The socket is `Send + Sync`. `try_clone()` does `dup`. `AsRawFd`/`IntoRawFd`/`FromRawFd` are implemented.

### `mcanbus::iface`

Pure netlink for state/up/down/cycle. `list_can_interfaces()` walks `/sys/class/net` and matches `type == 280` (ARPHRD_CAN), so it returns both real CAN devices and `vcan` shims. `bitrate()` shells out to `ip -details -json` rather than reproducing the nested `IFLA_LINKINFO`/`IFLA_INFO_DATA`/`IFLA_CAN_BITTIMING` parsing — the JSON parser is hand-rolled with a single `find("\"bitrate\":")` to avoid pulling in serde for what would be one read-mostly call.

`set_up`/`set_down`/`cycle` build raw netlink `RTM_NEWLINK` requests and parse the kernel's `NLMSG_ERROR` ack. `cycle` is the gs_usb-class recipe for recovering from BUS-OFF (down, sleep 150 ms, up).

### `mcanbus::isotp`

Synchronous ISO 15765-2 request/response: `request(socket, tx_id, rx_id, payload, opts) -> io::Result<Vec<u8>>`. Encodes Single Frame for ≤ 7 bytes, otherwise First Frame + Consecutive Frames; receives Flow Control from the ECU and respects its BS=0 / STmin (BS > 0 not yet implemented). On the response side, sends our own Flow Control after a First Frame and reassembles Consecutive Frames into a `Vec<u8>`. Frames whose ID doesn't match `rx_id` are silently dropped, so the function works on a socket with a broad or no kernel-level filter.

Padding default `0xCC` (Scania-typical). Total timeout default 1 s. Errors map cleanly: `TimedOut` for missed deadlines or excessive Wait flow controls, `InvalidData` for malformed PCI / sequence mismatches, `Unsupported` for the BS > 0 case, `Other` for ECU Overflow.

Validated against a real Scania S8 truck: a single `request` reads the full 17-character VIN in ~1 ms (vs. four manual frames worth of orchestration without it).

### `mcanbus::reader`

Optional (default-enabled) feature `reader` adds a multi-consumer fan-out built on `crossbeam_channel`. One dedicated thread reads from the socket via `recv_batch`, then snapshots the subscriber list under a `Mutex`, then fans out to per-subscriber `Sender` outside the lock. Each `Subscriber` has its own queue (unbounded by default; `subscribe_bounded(cap)` for backpressure) and atomic `received` / `dropped` counters. Dead subscribers (receiver dropped → `TrySendError::Disconnected`) are pruned in the same iteration. Shutdown latency is bounded by the socket's `recv_timeout` (default 500 ms).

Validated zero-loss against real hardware: 8 concurrent subscribers consuming 5000 fps for 3 seconds = each saw the identical ~15 000 frames, zero drops.

### `socketcan-mcp`

`rmcp 0.8` server with `#[tool_router]` / `#[tool_handler]`. Tokio runtime; the synchronous `Socket::recv` / `send` calls are wrapped in `tokio::task::spawn_blocking` so they don't pin a runtime worker. Configuration is purely environmental — no config file:

| Variable                     | Effect                                                                                  |
| ---------------------------- | --------------------------------------------------------------------------------------- |
| `SOCKETCAN_MCP_INTERFACES`   | Comma-separated allowlist. **Empty = no interface allowed.**                            |
| `SOCKETCAN_MCP_READONLY`     | When `1`/`true`, send-side tools refuse with a permission error.                        |
| `RUST_LOG`                   | `tracing` filter; logs go to stderr.                                                    |

The 5 tools all take typed `Parameters<...>` structs that derive `Deserialize + Serialize + JsonSchema`. The output is always JSON inside a `Content::text` cell — agents get structured data, not parseable text. Frames returned to agents include `timestamp_ns` so request/response timing is observable end-to-end.

## Key design choices

- **No tokio in the library.** The library exposes a synchronous API. The MCP server uses tokio because it has to (rmcp), but it keeps the boundary clean: `spawn_blocking` for everything that touches a socket.
- **`Frame` is `Copy`.** Fixed 64-byte buffer. The waste is real for classic frames; the simplicity it buys (no lifetimes, trivial fan-out, batch arrays) is worth more.
- **EINTR is hidden, EAGAIN is hidden.** Both surface as `Ok(None)` from `recv` so a caller's stop-flag pattern works. Other errors propagate.
- **Reader fan-out is lock-snapshot.** Lock the subscriber list, clone the `Arc`s, drop the lock, then call `try_send` outside. Subscribe/unsubscribe never blocks the reader thread.
- **MCP server has no global mutable state.** Each tool call opens its own sockets and tears them down. Long-running capture sessions (which would need state) are deliberately deferred.
- **Allowlist is the safety primitive.** Without an allowlist entry the server is inert. Empty default; agents see clear error messages explaining the env var.
- **Bitrate via `ip -j` shell-out** rather than netlink reimplementation — pragmatism. Read-mostly, called rarely, output format is stable enough.

## Dependencies

`mcanbus` library: `libc` (mandatory), `crossbeam-channel` (optional, default-on via `reader` feature). That's it.

`socketcan-mcp`: `mcanbus`, `libc`, `rmcp` (server + macros + transport-io + schemars), `schemars`, `serde`, `serde_json`, `tokio` (rt-multi-thread + io-std + sync + time + macros), `anyhow`, `tracing`, `tracing-subscriber`.

The release profile enables LTO and single codegen unit for both crates.

## Conventions

- Test IDs in integration tests use a private range (`0x7F0..=0x7FF` standard, `0x1FFF_FFE0..=0x1FFF_FFFF` extended) to avoid colliding with real ECU traffic. Each test owns a unique sub-ID; tests drain their RX socket before sending so cross-test pollution is impossible even on a busy live bus.
- `examples/` lives at the workspace root, not inside `mcanbus/examples/`, so both crates can pick them up; each example is wired in `mcanbus/Cargo.toml` via `[[example]] path = "../examples/..."`.
- Doctests use `?` and rely on the `From<FrameError> for io::Error` impl in `frame.rs`.
- `CLAUDE.md` is the canonical agent doc; `AGENTS.md` is a symlink.
