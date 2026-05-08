# mcanbus workspace

This repository hosts two crates that share a common SocketCAN core:

| Crate                                        | What it is                                                                            |
| -------------------------------------------- | ------------------------------------------------------------------------------------- |
| [`mcanbus`](mcanbus/)                        | Library: high-performance SocketCAN bindings for Linux (CAN 2.0 + CAN-FD, batched I/O, netlink). |
| [`socketcan-mcp`](socketcan-mcp/)            | Binary: [Model Context Protocol](https://modelcontextprotocol.io) server that exposes SocketCAN to AI agents. |

Both crates are MIT-licensed and meant to be lifted into other projects. They are extracted from the production code of [mcandump](https://github.com/mickeyl/mcandump) and [mcangen](https://github.com/mickeyl/mcangen), and built by the makers of CANsole — a forthcoming desktop CAN debugger for working engineers.

## What an agent does with this

`socketcan-mcp` turns SocketCAN into a typed RPC surface — agents talk to a CAN bus through structured tool calls, not by parsing `candump` output. A short transcript from a real session against a Scania S8 truck:

> **Me:** I plugged a Scania S8 onto can0. Can you find its VIN?
>
> **Agent:** Standard KWP2000 read — service `0x1A`, local identifier `0x90`, ISO-15765 extended addressing. Tester `0xF9` to target ECU `0x00`.
>
> Tool call:
> ```
> isotp_request(iface="can0", tx_id="18DA00F9", rx_id="18DAF900",
>               extended=true, payload="1A90")
> ```
>
> Result:
> ```
> { duration_ms: 1,
>   response: { len: 19,
>     hex:   "5A905953325236583430303035343132373335",
>     ascii: "Z.YS2R6X40005412735" } }
> ```
>
> The leading `5A 90` is the KWP positive-response header (echo of service `1A` + LID echo). The remaining 17 bytes are the VIN: **`YS2R6X40005412735`**. `YS2` is Scania's manufacturer prefix; the rest decodes to an R-series cab with 6×4 drive configuration.

That same interaction took four MCP tool calls before `isotp_request` existed (one capture, one Single Frame, one Flow Control, one to read the Consecutive Frames). The library now handles segmentation and Flow Control; the agent handles the meaning.

For contrast, the shell-driven version is `isotpsend` and `isotprecv` running in two background processes, with the agent parsing line-by-line text output and racing stdout buffering for timing — every CAN frame becomes a string, every reply becomes a regex, every multi-frame transfer becomes process orchestration. Most of the agent's context window ends up on plumbing instead of on the diagnostic question. The MCP path is a single typed call returning a typed result.

See [`socketcan-mcp/`](socketcan-mcp/) for the full tool list and a Claude Desktop / Claude Code config snippet.

## Building

```sh
cargo build --release
```

`cargo build` will produce both the library and the MCP server binary.

## Testing

A virtual CAN interface is required for most tests:

```sh
sudo ip link add dev vcan0 type vcan && sudo ip link set up vcan0
cargo test
```

## License

MIT for everything in this workspace.
