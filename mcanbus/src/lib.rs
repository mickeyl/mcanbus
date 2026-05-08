//! High-performance, low-overhead [SocketCAN](https://www.kernel.org/doc/html/latest/networking/can.html) bindings for Linux.
//!
//! `mcanbus` is intentionally close to the kernel ABI. It exposes the raw
//! constants from `linux/can.h` for power users, but wraps the common path
//! ([`Socket`], [`Frame`], [`Interface`]) in safe, ergonomic Rust.
//!
//! # Quick start
//!
//! ```no_run
//! use mcanbus::{CanId, Frame, OpenOpts, Socket};
//!
//! let sock = Socket::open("vcan0", &OpenOpts::default())?;
//! sock.send(&Frame::new_classic(CanId::Standard(0x123), &[0xDE, 0xAD, 0xBE, 0xEF])?)?;
//! while let Some(frame) = sock.recv()? {
//!     println!("{}", frame);
//! }
//! # Ok::<_, std::io::Error>(())
//! ```
//!
//! # Modules
//!
//! - [`frame`] — frame types, IDs, wire-format conversion.
//! - [`socket`] — `CAN_RAW` socket: open, send, recv, batched I/O.
//! - [`iface`] — interface enumeration and netlink control (up/down, state, bitrate).
//! - [`isotp`] — ISO 15765-2 segmented transport (request/response).
//! - [`consts`] — re-exported `linux/can.h` constants for power users.
//! - [`reader`] *(feature `reader`)* — multi-consumer fan-out reader.

#![cfg(target_os = "linux")]
#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod consts;
pub mod frame;
pub mod iface;
pub mod isotp;
pub mod socket;

#[cfg(feature = "reader")]
pub mod reader;

pub use frame::{CanId, FdFlags, Frame, FrameError, FrameKind, MAX_DATA_LEN, MAX_FD_DATA_LEN};
pub use iface::{CanState, Interface};
pub use socket::{CanFilter, OpenOpts, Socket, Timestamping};
