//! ISO 15765-2 (ISO-TP) — segmented transport over classic CAN.
//!
//! [`request`] is a synchronous request/response: it sends a payload of any
//! length up to 4095 bytes (12-bit FF length cap), then waits for the
//! response, handling Single Frame, First Frame + Consecutive Frames, and
//! Flow Control automatically in both directions.
//!
//! Only classic 8-byte CAN frames are supported in v0.2. CAN-FD ISO-TP
//! ("ISO 15765-2:2024 escape sequence") is on the roadmap.
//!
//! # Example
//!
//! ```no_run
//! use std::time::Duration;
//! use mcanbus::{CanId, OpenOpts, Socket};
//! use mcanbus::isotp;
//!
//! let socket = Socket::open("can0", &OpenOpts::default())?;
//! // KWP2000 ReadEcuIdentification, LID 0x90 (VIN), to ISO-15765 extended
//! // address 0x00 from tester 0xF9.
//! let response = isotp::request(
//!     &socket,
//!     CanId::extended(0x18DA00F9),
//!     CanId::extended(0x18DAF900),
//!     &[0x1A, 0x90],
//!     &isotp::Options::default(),
//! )?;
//! // First two response bytes are 0x5A (positive response) + 0x90 (LID echo);
//! // the remaining 17 are the VIN.
//! assert_eq!(response[0], 0x5A);
//! # Ok::<_, std::io::Error>(())
//! ```

use std::io;
use std::time::{Duration, Instant};

use crate::frame::{CanId, Frame};
use crate::socket::Socket;

// ── Wire-format constants ─────────────────────────────────────────────────

/// Single Frame.
const PCI_SF: u8 = 0x00;
/// First Frame.
const PCI_FF: u8 = 0x10;
/// Consecutive Frame.
const PCI_CF: u8 = 0x20;
/// Flow Control.
const PCI_FC: u8 = 0x30;
/// Mask isolating the PCI type nibble.
const PCI_TYPE_MASK: u8 = 0xF0;

const FC_CTS: u8 = 0;
const FC_WAIT: u8 = 1;
const FC_OVERFLOW: u8 = 2;

const SF_MAX_LEN: usize = 7;
const FF_FIRST_PAYLOAD: usize = 6;
const CF_PAYLOAD: usize = 7;
const FRAME_DLC: usize = 8;
/// 12-bit length field cap. 2024 escape format extends this — not yet supported.
const MAX_FF_LENGTH: usize = 0xFFF;

// ── Public API ────────────────────────────────────────────────────────────

/// Options for an ISO-TP request.
#[derive(Clone, Debug)]
pub struct Options {
    /// Padding byte for unused bytes within an 8-byte frame. ECUs commonly
    /// expect `0x00`, `0xAA`, `0xCC`, or `0xFF`. Default `0xCC`.
    pub padding: u8,
    /// Block Size we advertise in our outgoing Flow Control. `0` = "send
    /// every Consecutive Frame, no further FC". Default `0`.
    pub block_size: u8,
    /// SeparationTime minimum we request from the ECU between consecutive
    /// frames it sends to us, in milliseconds. Default `0`.
    pub st_min_ms: u8,
    /// Total timeout for the entire request/response cycle. Default 1 s.
    pub timeout: Duration,
    /// How many `Wait` flow-control frames we tolerate from the ECU before
    /// giving up. Default `8`.
    pub max_waits: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            padding: 0xCC,
            block_size: 0,
            st_min_ms: 0,
            timeout: Duration::from_millis(1000),
            max_waits: 8,
        }
    }
}

/// Send `payload` to `tx_id` and wait for a complete ISO-TP response on `rx_id`.
///
/// Frames whose ID doesn't match `rx_id` are silently dropped, so this works
/// over a socket with broad or no kernel-level filter (at the cost of some
/// userspace CPU on a busy bus).
///
/// # Errors
///
/// - [`io::ErrorKind::TimedOut`] if the total `opts.timeout` elapses before
///   the response is complete, or if too many `Wait` flow controls are
///   received.
/// - [`io::ErrorKind::InvalidData`] for malformed frames from the ECU
///   (unexpected PCI types, sequence-number mismatch, length overflow).
/// - [`io::ErrorKind::Unsupported`] if the ECU's Flow Control requests a
///   `BlockSize > 0` (not yet implemented).
/// - [`io::ErrorKind::Other`] if the ECU answers Flow Control `Overflow`.
pub fn request(
    socket: &Socket,
    tx_id: CanId,
    rx_id: CanId,
    payload: &[u8],
    opts: &Options,
) -> io::Result<Vec<u8>> {
    if payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload must not be empty",
        ));
    }
    if payload.len() > MAX_FF_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "payload {} bytes exceeds 12-bit FF length cap (4095)",
                payload.len()
            ),
        ));
    }

    let deadline = Instant::now() + opts.timeout;

    // ── Phase 1: send request ──
    if payload.len() <= SF_MAX_LEN {
        send_sf(socket, tx_id, payload, opts)?;
    } else {
        send_ff(socket, tx_id, payload, opts)?;
        let (bs, st_min_ms) = wait_for_fc(socket, rx_id, deadline, opts)?;
        send_consecutive_frames(
            socket,
            tx_id,
            &payload[FF_FIRST_PAYLOAD..],
            bs,
            st_min_ms,
            opts,
        )?;
    }

    // ── Phase 2: receive response ──
    receive_response(socket, tx_id, rx_id, deadline, opts)
}

// ── Encoding helpers ──────────────────────────────────────────────────────

fn encode_sf_frame(payload: &[u8], padding: u8) -> [u8; FRAME_DLC] {
    debug_assert!(payload.len() <= SF_MAX_LEN);
    let mut buf = [padding; FRAME_DLC];
    buf[0] = PCI_SF | (payload.len() as u8);
    buf[1..1 + payload.len()].copy_from_slice(payload);
    buf
}

fn encode_ff_frame(total_len: u16, first_chunk: &[u8], padding: u8) -> [u8; FRAME_DLC] {
    debug_assert!(first_chunk.len() == FF_FIRST_PAYLOAD);
    debug_assert!((total_len as usize) <= MAX_FF_LENGTH);
    let mut buf = [padding; FRAME_DLC];
    buf[0] = PCI_FF | ((total_len >> 8) as u8 & 0x0F);
    buf[1] = total_len as u8;
    buf[2..2 + FF_FIRST_PAYLOAD].copy_from_slice(first_chunk);
    buf
}

fn encode_cf_frame(seq: u8, chunk: &[u8], padding: u8) -> [u8; FRAME_DLC] {
    debug_assert!(chunk.len() <= CF_PAYLOAD);
    let mut buf = [padding; FRAME_DLC];
    buf[0] = PCI_CF | (seq & 0x0F);
    buf[1..1 + chunk.len()].copy_from_slice(chunk);
    buf
}

fn encode_fc_frame(flow_status: u8, block_size: u8, st_min_ms: u8, padding: u8) -> [u8; FRAME_DLC] {
    let mut buf = [padding; FRAME_DLC];
    buf[0] = PCI_FC | (flow_status & 0x0F);
    buf[1] = block_size;
    // STmin: 0x00..=0x7F → 0..=127 ms. Cap at 0x7F.
    buf[2] = st_min_ms.min(0x7F);
    buf
}

/// Decode the SeparationTime byte the ECU sent us into milliseconds (we
/// round microsecond ranges up to 1 ms — we don't have sub-ms scheduling).
fn decode_st_min(byte: u8) -> u8 {
    match byte {
        0x00..=0x7F => byte,
        0xF1..=0xF9 => 1,
        _ => 0x7F, // reserved → conservative
    }
}

// ── Send-side primitives ──────────────────────────────────────────────────

fn send_sf(socket: &Socket, tx_id: CanId, payload: &[u8], opts: &Options) -> io::Result<()> {
    let buf = encode_sf_frame(payload, opts.padding);
    socket.send(&Frame::new_classic(tx_id, &buf)?)
}

fn send_ff(socket: &Socket, tx_id: CanId, payload: &[u8], opts: &Options) -> io::Result<()> {
    let buf = encode_ff_frame(
        payload.len() as u16,
        &payload[..FF_FIRST_PAYLOAD],
        opts.padding,
    );
    socket.send(&Frame::new_classic(tx_id, &buf)?)
}

fn send_consecutive_frames(
    socket: &Socket,
    tx_id: CanId,
    rest: &[u8],
    bs: u8,
    st_min_ms: u8,
    opts: &Options,
) -> io::Result<()> {
    let mut seq: u8 = 1;
    let mut block_count: u8 = 0;
    let mut offset = 0;
    while offset < rest.len() {
        let end = (offset + CF_PAYLOAD).min(rest.len());
        let buf = encode_cf_frame(seq, &rest[offset..end], opts.padding);
        socket.send(&Frame::new_classic(tx_id, &buf)?)?;
        offset = end;
        seq = (seq + 1) & 0x0F;
        block_count = block_count.saturating_add(1);

        if bs != 0 && block_count == bs && offset < rest.len() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ECU's BlockSize > 0 not yet supported on the send path",
            ));
        }
        if st_min_ms > 0 && offset < rest.len() {
            std::thread::sleep(Duration::from_millis(st_min_ms as u64));
        }
    }
    Ok(())
}

fn send_fc(socket: &Socket, tx_id: CanId, opts: &Options) -> io::Result<()> {
    let buf = encode_fc_frame(FC_CTS, opts.block_size, opts.st_min_ms, opts.padding);
    socket.send(&Frame::new_classic(tx_id, &buf)?)
}

// ── Receive-side primitives ───────────────────────────────────────────────

fn recv_with_id(
    socket: &Socket,
    expected_id: CanId,
    deadline: Instant,
) -> io::Result<Option<Frame>> {
    while Instant::now() < deadline {
        match socket.recv()? {
            Some(frame) if frame.id == expected_id => return Ok(Some(frame)),
            Some(_) | None => continue,
        }
    }
    Ok(None)
}

fn wait_for_fc(
    socket: &Socket,
    rx_id: CanId,
    deadline: Instant,
    opts: &Options,
) -> io::Result<(u8, u8)> {
    let mut waits = 0u8;
    loop {
        let Some(frame) = recv_with_id(socket, rx_id, deadline)? else {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for Flow Control",
            ));
        };
        let data = frame.data();
        if data.is_empty() {
            continue;
        }
        if (data[0] & PCI_TYPE_MASK) != PCI_FC {
            return Err(invalid_data(format!(
                "expected Flow Control, got PCI byte 0x{:02X}",
                data[0]
            )));
        }
        match data[0] & 0x0F {
            FC_CTS => {
                let bs = if data.len() >= 2 { data[1] } else { 0 };
                let st = if data.len() >= 3 { data[2] } else { 0 };
                return Ok((bs, decode_st_min(st)));
            }
            FC_WAIT => {
                waits += 1;
                if waits > opts.max_waits {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("ECU sent more than {} Wait flow controls", opts.max_waits),
                    ));
                }
                continue;
            }
            FC_OVERFLOW => {
                return Err(io::Error::other("ECU returned Flow Control Overflow"));
            }
            other => {
                return Err(invalid_data(format!(
                    "unknown Flow Control flag 0x{other:X}"
                )));
            }
        }
    }
}

fn receive_response(
    socket: &Socket,
    tx_id: CanId,
    rx_id: CanId,
    deadline: Instant,
    opts: &Options,
) -> io::Result<Vec<u8>> {
    let Some(first) = recv_with_id(socket, rx_id, deadline)? else {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "no response from ECU",
        ));
    };
    let data = first.data();
    if data.is_empty() {
        return Err(invalid_data("empty response frame"));
    }

    match data[0] & PCI_TYPE_MASK {
        PCI_SF => {
            let len = (data[0] & 0x0F) as usize;
            if len == 0 || len > SF_MAX_LEN || 1 + len > data.len() {
                return Err(invalid_data(format!(
                    "invalid Single Frame length 0x{:X}",
                    data[0]
                )));
            }
            Ok(data[1..1 + len].to_vec())
        }
        PCI_FF => {
            if data.len() < 2 {
                return Err(invalid_data("First Frame too short"));
            }
            let total = (((data[0] & 0x0F) as usize) << 8) | (data[1] as usize);
            if total <= SF_MAX_LEN {
                return Err(invalid_data(format!(
                    "First Frame with length {total} would have fit in SF"
                )));
            }
            // Accumulate first-frame payload (6 bytes after the 2-byte PCI).
            let mut out = Vec::with_capacity(total);
            let first_chunk_len = (data.len() - 2).min(total);
            out.extend_from_slice(&data[2..2 + first_chunk_len]);

            // Hand back FlowControl so the ECU sends Consecutive Frames.
            send_fc(socket, tx_id, opts)?;

            let mut expected_seq: u8 = 1;
            while out.len() < total {
                let Some(frame) = recv_with_id(socket, rx_id, deadline)? else {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out collecting Consecutive Frames",
                    ));
                };
                let data = frame.data();
                if data.is_empty() || (data[0] & PCI_TYPE_MASK) != PCI_CF {
                    return Err(invalid_data(format!(
                        "expected Consecutive Frame, got PCI 0x{:02X}",
                        data.first().copied().unwrap_or(0)
                    )));
                }
                let got_seq = data[0] & 0x0F;
                if got_seq != expected_seq {
                    return Err(invalid_data(format!(
                        "CF sequence mismatch: expected {expected_seq:X}, got {got_seq:X}"
                    )));
                }
                let remaining = total - out.len();
                let take = remaining.min(data.len() - 1).min(CF_PAYLOAD);
                out.extend_from_slice(&data[1..1 + take]);
                expected_seq = (expected_seq + 1) & 0x0F;
            }
            Ok(out)
        }
        PCI_CF => Err(invalid_data(
            "received unsolicited Consecutive Frame as response start",
        )),
        PCI_FC => Err(invalid_data(
            "received Flow Control while expecting response data",
        )),
        _ => Err(invalid_data(format!(
            "unknown PCI type in response 0x{:02X}",
            data[0]
        ))),
    }
}

#[inline]
fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sf_encoding() {
        let f = encode_sf_frame(&[0x1A, 0x90], 0xCC);
        assert_eq!(f, [0x02, 0x1A, 0x90, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC]);
    }

    #[test]
    fn sf_encoding_max_len() {
        let f = encode_sf_frame(&[1, 2, 3, 4, 5, 6, 7], 0x00);
        assert_eq!(f, [0x07, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn ff_encoding_19_bytes() {
        // The exact pattern we saw on the Scania bus: 19-byte response total.
        let payload = [0x5A, 0x90, b'Y', b'S', b'2', b'R'];
        let f = encode_ff_frame(0x013, &payload, 0xCC);
        assert_eq!(f, [0x10, 0x13, 0x5A, 0x90, b'Y', b'S', b'2', b'R']);
    }

    #[test]
    fn ff_encoding_max_length() {
        let payload = [0u8; FF_FIRST_PAYLOAD];
        let f = encode_ff_frame(0xFFF, &payload, 0xAA);
        assert_eq!(f[0], 0x1F);
        assert_eq!(f[1], 0xFF);
    }

    #[test]
    fn cf_encoding_seq_wraparound() {
        let f = encode_cf_frame(1, &[1, 2, 3, 4, 5, 6, 7], 0xCC);
        assert_eq!(f, [0x21, 1, 2, 3, 4, 5, 6, 7]);
        let f15 = encode_cf_frame(15, &[0xAA], 0xCC);
        assert_eq!(f15[0], 0x2F);
        let f16 = encode_cf_frame(16, &[0xAA], 0xCC); // wraps to 0
        assert_eq!(f16[0], 0x20);
    }

    #[test]
    fn cf_short_chunk_padded() {
        let f = encode_cf_frame(2, &[0x34, 0x31, 0x32, 0x37, 0x33, 0x35], 0xCC);
        assert_eq!(f, [0x22, 0x34, 0x31, 0x32, 0x37, 0x33, 0x35, 0xCC]);
    }

    #[test]
    fn fc_encoding_cts() {
        let f = encode_fc_frame(FC_CTS, 0, 0, 0xCC);
        assert_eq!(f, [0x30, 0x00, 0x00, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC]);
    }

    #[test]
    fn fc_encoding_with_block_size_and_stmin() {
        let f = encode_fc_frame(FC_CTS, 8, 10, 0x00);
        assert_eq!(f[0], 0x30);
        assert_eq!(f[1], 8);
        assert_eq!(f[2], 10);
    }

    #[test]
    fn fc_st_min_caps_at_127ms() {
        let f = encode_fc_frame(FC_CTS, 0, 200, 0x00);
        assert_eq!(f[2], 0x7F);
    }

    #[test]
    fn st_min_decode_ms_range() {
        assert_eq!(decode_st_min(0x00), 0);
        assert_eq!(decode_st_min(0x05), 5);
        assert_eq!(decode_st_min(0x7F), 127);
    }

    #[test]
    fn st_min_decode_microsecond_range() {
        // 0xF1..=0xF9 = 100..=900 µs — round to 1 ms.
        for b in 0xF1..=0xF9 {
            assert_eq!(decode_st_min(b), 1);
        }
    }

    #[test]
    fn st_min_decode_reserved_is_conservative() {
        assert_eq!(decode_st_min(0x80), 0x7F);
        assert_eq!(decode_st_min(0xFA), 0x7F);
    }
}
