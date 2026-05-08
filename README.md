# mcanbus workspace

This repository hosts two crates that share a common SocketCAN core:

| Crate                                        | What it is                                                                            |
| -------------------------------------------- | ------------------------------------------------------------------------------------- |
| [`mcanbus`](mcanbus/)                        | Library: high-performance SocketCAN bindings for Linux (CAN 2.0 + CAN-FD, batched I/O, netlink). |
| [`socketcan-mcp`](socketcan-mcp/)            | Binary: [Model Context Protocol](https://modelcontextprotocol.io) server that exposes SocketCAN to AI agents. |

Both crates are MIT-licensed and meant to be lifted into other projects. They are extracted from the production code of [mcandump](https://github.com/mickeyl/mcandump) and [mcangen](https://github.com/mickeyl/mcangen), and built by the makers of [CANsole](https://cansole.app) — the CAN debugger for engineers.

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
