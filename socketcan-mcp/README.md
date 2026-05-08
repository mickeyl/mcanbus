# socketcan-mcp

Model Context Protocol server that exposes Linux [SocketCAN](https://www.kernel.org/doc/html/latest/networking/can.html) interfaces to AI agents — Claude Code, Claude Desktop, the MCP Inspector, anything that speaks MCP.

Built on top of [`mcanbus`](../mcanbus/) (high-performance SocketCAN bindings) by the makers of CANsole, a forthcoming desktop CAN debugger for working engineers.

## Why?

Without an MCP server, an agent that wants to test a CAN device has to orchestrate `candump` and `cansend` through `Bash`, parse text output, and manage background processes. With `socketcan-mcp` the same workflow is one structured tool call:

```jsonc
// "send 0x7DF#02 0100 (OBD-II PID query) and capture replies on can0 for 200 ms"
{
  "name": "send_and_capture",
  "arguments": {
    "iface": "can0",
    "frame": { "id": "7DF", "data": "020100" },
    "timeout_ms": 200,
    "filter": { "entries": [{ "id": "7E8", "mask": "7F8" }] }
  }
}
```

The response is a structured JSON list of frames with hardware timestamps — no `awk`, no race conditions, no torn output.

## Tools

| Tool                  | Description                                                                                       | Gated by   |
| --------------------- | ------------------------------------------------------------------------------------------------- | ---------- |
| `list_interfaces`     | Enumerate every CAN-class interface with state and bitrate.                                       | —          |
| `iface_state`         | Detailed status (up/down, controller state, bitrate) for one interface.                           | —          |
| `capture`             | Listen for up to `timeout_ms` ms and return up to `max_frames` matching frames.                   | allowlist  |
| `send_frame`          | Transmit a single frame.                                                                          | allowlist + writable |
| `send_and_capture`    | Transmit and immediately capture replies in the same call (raw frames, no segmentation).         | allowlist + writable |
| `isotp_request`       | ISO 15765-2 request/response with automatic SF/FF/CF segmentation and Flow Control. UDS/KWP/OBD. | allowlist + writable |

All write tools refuse when `SOCKETCAN_MCP_READONLY=1`.

## Configuration

Configuration is via environment variables — no config file, no surprises.

| Variable                     | Effect                                                                                                |
| ---------------------------- | ----------------------------------------------------------------------------------------------------- |
| `SOCKETCAN_MCP_INTERFACES`   | Comma-separated allowlist (e.g. `vcan0,can0`). **If unset, every send/capture call fails.**           |
| `SOCKETCAN_MCP_READONLY`     | When `1`/`true`, send-side tools refuse with a permission error.                                      |
| `RUST_LOG`                   | Standard `tracing` filter; defaults to `info` on stderr.                                              |

## Installation

```sh
cargo install socketcan-mcp
```

Or build from source:

```sh
git clone https://github.com/mickeyl/mcanbus
cd mcanbus
cargo build --release -p socketcan-mcp
# binary at target/release/socketcan-mcp
```

The binary needs `CAP_NET_RAW` (or root) to open `CAN_RAW` sockets — the standard SocketCAN requirement. For most desktop setups this means running it as root or granting the capability:

```sh
sudo setcap cap_net_raw+ep /path/to/socketcan-mcp
```

## Wiring it up

### Claude Desktop

```json
{
  "mcpServers": {
    "socketcan": {
      "command": "/usr/local/bin/socketcan-mcp",
      "env": {
        "SOCKETCAN_MCP_INTERFACES": "vcan0,can0",
        "RUST_LOG": "info"
      }
    }
  }
}
```

### Claude Code

Add to `~/.config/claude-code/mcp.json` (or your project's `.mcp.json`):

```json
{
  "mcpServers": {
    "socketcan": {
      "command": "socketcan-mcp",
      "env": {
        "SOCKETCAN_MCP_INTERFACES": "vcan0"
      }
    }
  }
}
```

### MCP Inspector (for debugging)

```sh
npx @modelcontextprotocol/inspector socketcan-mcp
```

then set `SOCKETCAN_MCP_INTERFACES` in the inspector's environment panel.

## Safety model

- **Allowlist**: no interface is touched without an explicit opt-in via `SOCKETCAN_MCP_INTERFACES`. Even `iface_state` only shows information — it doesn't grant capability.
- **Readonly mode**: `SOCKETCAN_MCP_READONLY=1` reduces the surface area to capture/inspect tools. Pair with the allowlist for a tight sandbox.
- **No interface configuration**: this server cannot bring interfaces up/down, change bitrate, or alter controller state. That's a deliberate scope choice — netlink admin operations are too consequential to expose by default. See the `mcanbus::iface` crate if you need them in custom tooling.

## Worked example: read a VIN

The Scania S8 truck (and many other commercial vehicles) speak KWP2000 over CAN
with ISO-15765 extended addressing. A VIN read is `service 0x1A` + `LID 0x90`,
which the ECU answers with 19 bytes (positive-response header + 17-character
VIN). That fits in a First Frame + 2 Consecutive Frames; without ISO-TP the
agent would have to orchestrate the Flow Control by hand.

With `isotp_request` it's one tool call:

```jsonc
{
  "name": "isotp_request",
  "arguments": {
    "iface": "can0",
    "tx_id": "18DA00F9",
    "rx_id": "18DAF900",
    "extended": true,
    "payload": "1A90"
  }
}
```

Response:

```jsonc
{
  "duration_ms": 1,
  "response": {
    "len": 19,
    "hex":   "5A905953325236583430303035343132373335",
    "ascii": "Z.YS2R6X40005412735"
  }
}
```

The ASCII column shows the bytes verbatim — the leading `Z.` is the KWP
positive-response header (`0x5A` `0x90`), the rest is the VIN.

## Roadmap

### Transport & protocol

- Long-running capture sessions (`capture_start`/`capture_read`/`capture_stop`) backed by the [`mcanbus::reader`](../mcanbus/src/reader.rs) fan-out, so the agent can correlate request/response timing without polling.
- CAN-FD ISO-TP (ISO 15765-2:2024 escape sequence) — extended `isotp_request` for FD payloads beyond 4095 bytes.
- ECU-side `BlockSize > 0` in `isotp_request` (currently errors with `Unsupported`).
- DBC decoding so frames come back as named signals alongside raw bytes.

### Diagnostic-domain tools

These are convenience wrappers over `isotp_request` and `capture` that turn common workflows into single typed calls:

- **`uds_session`** — replay a UDS choreography (DSC → SecAccess → RoutineControl → RequestDownload → TransferData → TransferExit → ECUReset) and validate every step against the ISO 14229 state machine. Reports P2/P2* timing, NRC sequences, and `0x78` (ResponsePending) escalations. Aimed at flasher-app validation: catches wrong P2* timeouts, padding mismatches, and BlockSize handling bugs before they cost a 30-minute flash.
- **`obd_pid_query`** — OBD-II Mode 0x01 PID reads with structured decoding (engine RPM, coolant temperature, etc.). Customer reports of the form "PID 0x05 looks weird" become reproducible in one call.
- **`dtc_read`** — read all DTCs (`19 02 FF`), parse the 3-byte ID + 1-byte status structure, decode where a database is available.
- **`dtc_diff`** — `dtc_read` snapshot before and after a flash or routine, return the delta (cleared / new / status-changed). Catches flash regressions that a screenshot-and-compare workflow misses.
- **`ecu_discover`** — broadcast DSC on the functional address (`0x7DF` / `0x18DB33F1`), capture all responders, return a map of `{tester_address → first_response_bytes}`. Bus-topology exploration in one call.
- **`uds_diff`** — run the same UDS payload against two interfaces (e.g. `can0` real ECU vs. `can1` mock / [ECUmulator](https://github.com/Automotive-Swift/Swift-CANyonero)) and report response-by-response divergences. Useful for validating mocks against silicon.
- **`isotp_fuzz_lite`** — bounded-set malformed ISO-TP requests (oversized SF, wrong CF sequence, illegal sub-function bytes, out-of-range DIDs) to verify the ECU answers with proper NRCs (`0x12` / `0x13` / `0x22` / `0x33`) instead of timing out. Robustness regression test.

### Observability

- Pre-flight summary tool (`bus_health`) — combine `iface_state`, an N-second `capture`, and an error-frame count into a single "is this bus ready for a 30-minute flash" verdict.

## License

MIT.
