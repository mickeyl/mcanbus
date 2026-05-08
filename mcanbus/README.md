# mcanbus

High-performance, low-overhead [SocketCAN](https://www.kernel.org/doc/html/latest/networking/can.html) bindings for Linux.

- **Pure libc, no async runtime.** No tokio, no smol, no `unsafe` hidden behind a thick wrapper.
- **CAN 2.0 and CAN-FD** in one frame type, with hardware timestamps when the driver supports them.
- **Batched syscalls** — `recvmmsg` / `sendmmsg` for high-throughput pipelines.
- **Netlink helpers** — bring interfaces up/down, query `CAN_STATE_*`, recover from BUS-OFF without `ip link` shell-outs.
- **ISO 15765-2 (ISO-TP)** — synchronous request/response with automatic SF/FF/CF segmentation and Flow Control in both directions. Used for UDS, KWP2000, OBD-II reads larger than 7 bytes.
- **Optional fan-out reader** — one RX thread, many lock-free subscribers via `crossbeam-channel`. Zero-loss when subscribers keep up; per-subscriber drop counters when they don't.

## Status

Early. The API will move before 1.0.

## Why another SocketCAN crate?

The existing [`socketcan`](https://crates.io/crates/socketcan) crate is great but pulls in optional async runtimes and abstracts further from the kernel than some users want. `mcanbus` stays close to `linux/can.h`, exposes raw constants for power users, and is built for tools that move millions of frames per second without dropping any.

It is extracted from the production code of [mcandump](https://crates.io/crates/mcandump) and [mcangen](https://crates.io/crates/mcangen).

## License

MIT.
