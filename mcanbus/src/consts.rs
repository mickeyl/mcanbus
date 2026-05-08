//! Raw constants from `linux/can.h`, `linux/can/raw.h`, and `linux/can/netlink.h`.
//!
//! These are exposed for users who need to talk to the kernel directly — for
//! example, building a custom filter set or reading a non-standard socket
//! option. Most users will not need this module.

#![allow(missing_docs)]

use libc::{c_int, Ioctl};

// ── Address family / protocol ─────────────────────────────────────────────

pub const AF_CAN: c_int = 29;
pub const PF_CAN: c_int = AF_CAN;
pub const CAN_RAW: c_int = 1;

// `libc::Ioctl` is `c_ulong` on glibc and `c_int` on musl — use the alias.
pub const SIOCGIFINDEX: Ioctl = 0x8933 as Ioctl;

// ── Setsockopt levels & names ─────────────────────────────────────────────

pub const SOL_CAN_BASE: c_int = 100;
pub const SOL_CAN_RAW: c_int = SOL_CAN_BASE + CAN_RAW;

pub const CAN_RAW_FILTER: c_int = 1;
pub const CAN_RAW_ERR_FILTER: c_int = 2;
pub const CAN_RAW_LOOPBACK: c_int = 3;
pub const CAN_RAW_RECV_OWN_MSGS: c_int = 4;
pub const CAN_RAW_FD_FRAMES: c_int = 5;
pub const CAN_RAW_JOIN_FILTERS: c_int = 6;

// ── can_id flags (bits in `can_id` itself) ────────────────────────────────

/// Extended frame format (29-bit identifier).
pub const CAN_EFF_FLAG: u32 = 0x8000_0000;
/// Remote transmission request.
pub const CAN_RTR_FLAG: u32 = 0x4000_0000;
/// Error message frame.
pub const CAN_ERR_FLAG: u32 = 0x2000_0000;

/// Mask for extracting a 29-bit extended identifier.
pub const CAN_EFF_MASK: u32 = 0x1FFF_FFFF;
/// Mask for extracting an 11-bit standard identifier.
pub const CAN_SFF_MASK: u32 = 0x0000_07FF;
/// Mask for the full error class set.
pub const CAN_ERR_MASK: u32 = 0x1FFF_FFFF;

// ── canfd_frame.flags ─────────────────────────────────────────────────────

/// Bit Rate Switch (CAN-FD).
pub const CANFD_BRS: u8 = 0x01;
/// Error State Indicator (CAN-FD).
pub const CANFD_ESI: u8 = 0x02;
/// FD Format flag (set on TX to mark as FD; not always required).
pub const CANFD_FDF: u8 = 0x04;

// ── Frame sizes on the wire ───────────────────────────────────────────────

/// `sizeof(struct can_frame)`.
pub const CAN_FRAME_SIZE: usize = 16;
/// `sizeof(struct canfd_frame)`.
pub const CANFD_FRAME_SIZE: usize = 72;

/// Maximum classic CAN data length.
pub const CAN_MAX_DLEN: usize = 8;
/// Maximum CAN-FD data length.
pub const CANFD_MAX_DLEN: usize = 64;

// ── SO_TIMESTAMPING flag bits (`linux/net_tstamp.h`) ──────────────────────

pub const SOF_TIMESTAMPING_TX_HARDWARE: u32 = 1 << 0;
pub const SOF_TIMESTAMPING_TX_SOFTWARE: u32 = 1 << 1;
pub const SOF_TIMESTAMPING_RX_HARDWARE: u32 = 1 << 2;
pub const SOF_TIMESTAMPING_RX_SOFTWARE: u32 = 1 << 3;
pub const SOF_TIMESTAMPING_SOFTWARE: u32 = 1 << 4;
pub const SOF_TIMESTAMPING_SYS_HARDWARE: u32 = 1 << 5;
pub const SOF_TIMESTAMPING_RAW_HARDWARE: u32 = 1 << 6;

// ── Error class bits (when CAN_ERR_FLAG is set in can_id) ─────────────────

pub const CAN_ERR_TX_TIMEOUT: u32 = 0x0000_0001;
pub const CAN_ERR_LOSTARB: u32 = 0x0000_0002;
pub const CAN_ERR_CRTL: u32 = 0x0000_0004;
pub const CAN_ERR_PROT: u32 = 0x0000_0008;
pub const CAN_ERR_TRX: u32 = 0x0000_0010;
pub const CAN_ERR_ACK: u32 = 0x0000_0020;
pub const CAN_ERR_BUSOFF: u32 = 0x0000_0040;
pub const CAN_ERR_BUSERROR: u32 = 0x0000_0080;
pub const CAN_ERR_RESTARTED: u32 = 0x0000_0100;

// ── CAN controller state (`enum can_state`, `linux/can/netlink.h`) ────────

pub const CAN_STATE_ERROR_ACTIVE: u32 = 0;
pub const CAN_STATE_ERROR_WARNING: u32 = 1;
pub const CAN_STATE_ERROR_PASSIVE: u32 = 2;
pub const CAN_STATE_BUS_OFF: u32 = 3;
pub const CAN_STATE_STOPPED: u32 = 4;
pub const CAN_STATE_SLEEPING: u32 = 5;
