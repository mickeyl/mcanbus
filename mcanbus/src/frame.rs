//! CAN frame types and wire-format conversion.
//!
//! Frames are stored with a fixed-size 64-byte data buffer and a length tag.
//! That gives us [`Copy`] semantics and avoids any allocation on the hot path,
//! at the cost of 56 wasted bytes for classic 8-byte frames. For tools that
//! shovel millions of frames per second this is the right trade.

use core::fmt;

use crate::consts::*;

/// Maximum payload of a classic CAN 2.0 frame.
pub const MAX_DATA_LEN: usize = CAN_MAX_DLEN;
/// Maximum payload of a CAN-FD frame.
pub const MAX_FD_DATA_LEN: usize = CANFD_MAX_DLEN;

// ── CanId ─────────────────────────────────────────────────────────────────

/// A CAN identifier — either an 11-bit standard or a 29-bit extended ID.
///
/// Construction is total: out-of-range bits are silently masked off.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CanId {
    /// Standard 11-bit identifier (CAN 2.0A).
    Standard(u16),
    /// Extended 29-bit identifier (CAN 2.0B).
    Extended(u32),
}

impl CanId {
    /// Construct a [`CanId::Standard`], masking to 11 bits.
    #[inline]
    pub const fn standard(id: u16) -> Self {
        Self::Standard(id & CAN_SFF_MASK as u16)
    }

    /// Construct a [`CanId::Extended`], masking to 29 bits.
    #[inline]
    pub const fn extended(id: u32) -> Self {
        Self::Extended(id & CAN_EFF_MASK)
    }

    /// Numeric value of the identifier (without flag bits).
    #[inline]
    pub const fn raw(self) -> u32 {
        match self {
            Self::Standard(id) => id as u32,
            Self::Extended(id) => id,
        }
    }

    /// Whether this is an extended (29-bit) identifier.
    #[inline]
    pub const fn is_extended(self) -> bool {
        matches!(self, Self::Extended(_))
    }

    /// Pack into the kernel `can_id` field, setting [`CAN_EFF_FLAG`] for
    /// extended identifiers.
    #[inline]
    pub const fn to_wire(self) -> u32 {
        match self {
            Self::Standard(id) => id as u32 & CAN_SFF_MASK,
            Self::Extended(id) => (id & CAN_EFF_MASK) | CAN_EFF_FLAG,
        }
    }

    /// Parse a kernel `can_id` value. The [`CAN_RTR_FLAG`] and
    /// [`CAN_ERR_FLAG`] bits are ignored here; check them separately on the
    /// raw `u32` if you care.
    #[inline]
    pub const fn from_wire(wire: u32) -> Self {
        if wire & CAN_EFF_FLAG != 0 {
            Self::Extended(wire & CAN_EFF_MASK)
        } else {
            Self::Standard((wire & CAN_SFF_MASK) as u16)
        }
    }
}

impl fmt::Display for CanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Standard(id) => write!(f, "{:03X}", id),
            Self::Extended(id) => write!(f, "{:08X}", id),
        }
    }
}

// ── FdFlags / FrameKind ───────────────────────────────────────────────────

/// CAN-FD-specific flags carried in `canfd_frame.flags`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdFlags {
    /// Bit Rate Switch — payload transmitted at the higher data-phase rate.
    pub brs: bool,
    /// Error State Indicator — set by the transmitter when error-passive.
    pub esi: bool,
}

impl FdFlags {
    /// Encode into the kernel `canfd_frame.flags` byte.
    #[inline]
    pub const fn to_wire(self) -> u8 {
        let mut f = 0u8;
        if self.brs {
            f |= CANFD_BRS;
        }
        if self.esi {
            f |= CANFD_ESI;
        }
        f
    }

    /// Parse from the kernel `canfd_frame.flags` byte.
    #[inline]
    pub const fn from_wire(wire: u8) -> Self {
        Self {
            brs: wire & CANFD_BRS != 0,
            esi: wire & CANFD_ESI != 0,
        }
    }
}

/// Whether a frame is classic CAN 2.0 or CAN-FD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    /// Classic CAN 2.0 frame, payload ≤ 8 bytes.
    Classic,
    /// CAN-FD frame, payload ≤ 64 bytes.
    Fd(FdFlags),
}

impl FrameKind {
    /// Whether this is a CAN-FD frame.
    #[inline]
    pub const fn is_fd(self) -> bool {
        matches!(self, Self::Fd(_))
    }

    /// CAN-FD flags, or `FdFlags::default()` for classic frames.
    #[inline]
    pub const fn fd_flags(self) -> FdFlags {
        match self {
            Self::Fd(f) => f,
            Self::Classic => FdFlags { brs: false, esi: false },
        }
    }
}

// ── Frame ─────────────────────────────────────────────────────────────────

/// Errors that can occur when constructing a [`Frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Payload exceeded the maximum for the requested frame kind
    /// (8 bytes classic, 64 bytes FD).
    DataTooLong {
        /// Length that was attempted.
        len: usize,
        /// Maximum permitted for this kind.
        max: usize,
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataTooLong { len, max } => {
                write!(f, "payload {} bytes exceeds maximum {} bytes", len, max)
            }
        }
    }
}

impl std::error::Error for FrameError {}

impl From<FrameError> for std::io::Error {
    fn from(e: FrameError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
    }
}

/// A CAN or CAN-FD frame.
///
/// Payload is held inline in a 64-byte buffer; `len` is the meaningful prefix.
/// [`Frame::data`] is the convenient borrow.
///
/// `timestamp_ns` is populated by [`crate::Socket::recv`] when the kernel
/// provides one (`SO_TIMESTAMPING` hardware → software → `SO_TIMESTAMPNS`
/// fallback). It is in nanoseconds since the UNIX epoch. For TX paths it is
/// always [`None`].
#[derive(Clone, Copy)]
pub struct Frame {
    /// Identifier.
    pub id: CanId,
    /// Frame kind (classic / FD).
    pub kind: FrameKind,
    /// Length of the meaningful prefix in [`Self::data_buf`].
    pub len: u8,
    /// Inline data buffer; only `data_buf[..len]` is valid.
    pub data_buf: [u8; MAX_FD_DATA_LEN],
    /// RX timestamp in nanoseconds since the UNIX epoch, when available.
    pub timestamp_ns: Option<u64>,
}

impl Frame {
    /// Construct a classic CAN 2.0 frame.
    pub fn new_classic(id: CanId, data: &[u8]) -> Result<Self, FrameError> {
        if data.len() > MAX_DATA_LEN {
            return Err(FrameError::DataTooLong {
                len: data.len(),
                max: MAX_DATA_LEN,
            });
        }
        let mut buf = [0u8; MAX_FD_DATA_LEN];
        buf[..data.len()].copy_from_slice(data);
        Ok(Self {
            id,
            kind: FrameKind::Classic,
            len: data.len() as u8,
            data_buf: buf,
            timestamp_ns: None,
        })
    }

    /// Construct a CAN-FD frame with the given flags.
    pub fn new_fd(id: CanId, data: &[u8], flags: FdFlags) -> Result<Self, FrameError> {
        if data.len() > MAX_FD_DATA_LEN {
            return Err(FrameError::DataTooLong {
                len: data.len(),
                max: MAX_FD_DATA_LEN,
            });
        }
        let mut buf = [0u8; MAX_FD_DATA_LEN];
        buf[..data.len()].copy_from_slice(data);
        Ok(Self {
            id,
            kind: FrameKind::Fd(flags),
            len: data.len() as u8,
            data_buf: buf,
            timestamp_ns: None,
        })
    }

    /// The valid prefix of the data buffer.
    #[inline]
    pub fn data(&self) -> &[u8] {
        &self.data_buf[..self.len as usize]
    }

    /// Whether this frame is CAN-FD.
    #[inline]
    pub const fn is_fd(&self) -> bool {
        self.kind.is_fd()
    }
}

impl fmt::Debug for Frame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Frame")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("data", &self.data())
            .field("timestamp_ns", &self.timestamp_ns)
            .finish()
    }
}

impl fmt::Display for Frame {
    /// `candump`-style: `123#DEADBEEF` (classic) or `123##0DEADBEEF` (FD with
    /// FD flags byte after the double `##`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)?;
        match self.kind {
            FrameKind::Classic => write!(f, "#")?,
            FrameKind::Fd(flags) => write!(f, "##{:X}", flags.to_wire())?,
        }
        for b in self.data() {
            write!(f, "{:02X}", b)?;
        }
        Ok(())
    }
}

// ── Wire-format conversion (kernel struct ↔ Frame) ────────────────────────

/// Kernel `struct canfd_frame` layout. Public so power users can interop
/// with `read`/`write` directly, but most callers should use [`Socket`]
/// methods which handle this transparently.
///
/// [`Socket`]: crate::Socket
#[repr(C)]
#[derive(Clone, Copy)]
pub struct WireFrame {
    /// `can_id` — packed identifier with EFF/RTR/ERR flag bits.
    pub can_id: u32,
    /// Payload length in bytes (0..=8 classic, 0..=64 FD).
    pub len: u8,
    /// FD flags byte (BRS/ESI). Zero for classic frames.
    pub flags: u8,
    /// Reserved.
    pub __res0: u8,
    /// Reserved (classic frames: `len_8_dlc`).
    pub __res1: u8,
    /// Payload (only `len` bytes are meaningful).
    pub data: [u8; MAX_FD_DATA_LEN],
}

impl WireFrame {
    /// Zero-initialised wire frame.
    #[inline]
    pub const fn zeroed() -> Self {
        Self {
            can_id: 0,
            len: 0,
            flags: 0,
            __res0: 0,
            __res1: 0,
            data: [0; MAX_FD_DATA_LEN],
        }
    }

    /// Build from a [`Frame`]. The result is suitable for `write`/`send` to a
    /// `CAN_RAW` socket, using either [`CAN_FRAME_SIZE`] (classic) or
    /// [`CANFD_FRAME_SIZE`] (FD) bytes.
    pub fn from_frame(frame: &Frame) -> Self {
        let mut w = Self::zeroed();
        w.can_id = frame.id.to_wire();
        w.len = frame.len;
        if let FrameKind::Fd(flags) = frame.kind {
            w.flags = flags.to_wire();
        }
        w.data[..frame.len as usize].copy_from_slice(&frame.data_buf[..frame.len as usize]);
        w
    }

    /// Number of bytes to write/read on the wire for this frame kind.
    #[inline]
    pub const fn wire_size(is_fd: bool) -> usize {
        if is_fd {
            CANFD_FRAME_SIZE
        } else {
            CAN_FRAME_SIZE
        }
    }
}

/// Outcome of decoding a wire-format frame.
#[derive(Debug, Clone, Copy)]
pub enum DecodedFrame {
    /// A normal data frame.
    Data(Frame),
    /// A remote transmission request (CAN_RTR_FLAG set). Length and ID kept;
    /// no payload is delivered by the kernel.
    Rtr {
        /// Identifier of the requested frame.
        id: CanId,
        /// Requested DLC.
        len: u8,
    },
    /// An error frame (CAN_ERR_FLAG set). The error class is in the low bits
    /// of the raw `can_id`; the payload (`data`, up to 8 bytes) carries
    /// driver-specific detail per `linux/can/error.h`.
    Error {
        /// Error class bits (subset of [`crate::consts::CAN_ERR_TX_TIMEOUT`] etc.).
        class: u32,
        /// Up to 8 bytes of error-specific data.
        data: [u8; 8],
        /// Length of the meaningful prefix in `data`.
        len: u8,
    },
}

/// Decode a kernel-format buffer (16 bytes for classic, 72 for FD) into a
/// [`DecodedFrame`].
///
/// Returns `None` if the buffer is shorter than [`CAN_FRAME_SIZE`].
///
/// `timestamp_ns` is attached to the resulting [`Frame`] (data variant only).
pub fn decode_wire(buf: &[u8], timestamp_ns: Option<u64>) -> Option<DecodedFrame> {
    if buf.len() < CAN_FRAME_SIZE {
        return None;
    }
    // Both `can_frame` and `canfd_frame` start with: can_id(4) len(1) flags(1) res(2).
    // SAFETY: we just verified `buf.len() >= 16`, and we only read 8 bytes of header
    // plus up to `data_len` bytes of payload, all bounded by `buf.len()`.
    let can_id = u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let payload_len = buf[4];
    let fd_flags = buf[5];

    let is_rtr = can_id & CAN_RTR_FLAG != 0;
    let is_err = can_id & CAN_ERR_FLAG != 0;
    let is_extended = can_id & CAN_EFF_FLAG != 0;

    if is_err {
        // Error frame: payload is fixed 8-byte struct can_frame.
        let mut data = [0u8; 8];
        let n = (payload_len as usize).min(8).min(buf.len().saturating_sub(8));
        data[..n].copy_from_slice(&buf[8..8 + n]);
        return Some(DecodedFrame::Error {
            class: can_id & CAN_ERR_MASK,
            data,
            len: n as u8,
        });
    }

    let id = if is_extended {
        CanId::Extended(can_id & CAN_EFF_MASK)
    } else {
        CanId::Standard((can_id & CAN_SFF_MASK) as u16)
    };

    if is_rtr {
        return Some(DecodedFrame::Rtr {
            id,
            len: payload_len,
        });
    }

    // FD frames are exactly 72 bytes; classic frames are 16. The kernel always
    // gives us one of those two sizes.
    let is_fd = buf.len() == CANFD_FRAME_SIZE;
    let max_payload = if is_fd { MAX_FD_DATA_LEN } else { MAX_DATA_LEN };
    let len = (payload_len as usize).min(max_payload);

    let mut data_buf = [0u8; MAX_FD_DATA_LEN];
    // Header is 8 bytes; payload follows immediately.
    let header = 8;
    let take = len.min(buf.len().saturating_sub(header));
    data_buf[..take].copy_from_slice(&buf[header..header + take]);

    let kind = if is_fd {
        FrameKind::Fd(FdFlags::from_wire(fd_flags))
    } else {
        FrameKind::Classic
    };

    Some(DecodedFrame::Data(Frame {
        id,
        kind,
        len: take as u8,
        data_buf,
        timestamp_ns,
    }))
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canid_standard_round_trip() {
        let id = CanId::standard(0x123);
        assert_eq!(id.raw(), 0x123);
        assert!(!id.is_extended());
        assert_eq!(id.to_wire(), 0x123);
        assert_eq!(CanId::from_wire(0x123), id);
    }

    #[test]
    fn canid_extended_round_trip() {
        let id = CanId::extended(0x18DA_F101);
        assert_eq!(id.raw(), 0x18DA_F101);
        assert!(id.is_extended());
        assert_eq!(id.to_wire(), 0x18DA_F101 | CAN_EFF_FLAG);
        assert_eq!(CanId::from_wire(0x18DA_F101 | CAN_EFF_FLAG), id);
    }

    #[test]
    fn canid_masks_overflow() {
        // Out-of-range bits are silently masked, by design.
        assert_eq!(CanId::standard(0xFFFF).raw(), 0x7FF);
        assert_eq!(CanId::extended(0xFFFF_FFFF).raw(), CAN_EFF_MASK);
    }

    #[test]
    fn fd_flags_round_trip() {
        let f = FdFlags { brs: true, esi: false };
        assert_eq!(f.to_wire(), CANFD_BRS);
        assert_eq!(FdFlags::from_wire(CANFD_BRS | CANFD_ESI), FdFlags { brs: true, esi: true });
    }

    #[test]
    fn classic_frame_too_long() {
        let too_long = [0u8; 9];
        assert!(matches!(
            Frame::new_classic(CanId::standard(0x100), &too_long),
            Err(FrameError::DataTooLong { len: 9, max: 8 })
        ));
    }

    #[test]
    fn fd_frame_max_len() {
        let max = [0xAAu8; 64];
        let f = Frame::new_fd(CanId::standard(0x100), &max, FdFlags::default()).unwrap();
        assert_eq!(f.data(), &max[..]);
        assert!(f.is_fd());
    }

    #[test]
    fn classic_round_trip_through_wire() {
        let original = Frame::new_classic(
            CanId::standard(0x123),
            &[0xDE, 0xAD, 0xBE, 0xEF],
        )
        .unwrap();
        let wire = WireFrame::from_frame(&original);
        // Reinterpret the WireFrame as a 16-byte classic frame buffer.
        let bytes: &[u8] =
            unsafe { core::slice::from_raw_parts((&wire as *const WireFrame) as *const u8, CAN_FRAME_SIZE) };
        let decoded = decode_wire(bytes, None).unwrap();
        match decoded {
            DecodedFrame::Data(f) => {
                assert_eq!(f.id, original.id);
                assert!(matches!(f.kind, FrameKind::Classic));
                assert_eq!(f.data(), original.data());
            }
            other => panic!("expected data frame, got {other:?}"),
        }
    }

    #[test]
    fn extended_with_eff_flag() {
        let f = Frame::new_classic(CanId::extended(0x1ABC_DEF0), &[]).unwrap();
        let w = WireFrame::from_frame(&f);
        assert_eq!(w.can_id & CAN_EFF_FLAG, CAN_EFF_FLAG);
        assert_eq!(w.can_id & CAN_EFF_MASK, 0x1ABC_DEF0);
    }

    #[test]
    fn fd_round_trip_through_wire() {
        let payload: Vec<u8> = (0..32).collect();
        let original = Frame::new_fd(
            CanId::extended(0x1FFF_FFFF),
            &payload,
            FdFlags { brs: true, esi: true },
        )
        .unwrap();
        let wire = WireFrame::from_frame(&original);
        let bytes: &[u8] =
            unsafe { core::slice::from_raw_parts((&wire as *const WireFrame) as *const u8, CANFD_FRAME_SIZE) };
        let decoded = decode_wire(bytes, Some(123_456_789)).unwrap();
        match decoded {
            DecodedFrame::Data(f) => {
                assert_eq!(f.id, original.id);
                assert!(matches!(f.kind, FrameKind::Fd(FdFlags { brs: true, esi: true })));
                assert_eq!(f.data(), &payload[..]);
                assert_eq!(f.timestamp_ns, Some(123_456_789));
            }
            other => panic!("expected data frame, got {other:?}"),
        }
    }

    #[test]
    fn rtr_decoded_separately() {
        let mut buf = [0u8; CAN_FRAME_SIZE];
        let id = 0x456 | CAN_RTR_FLAG;
        buf[0..4].copy_from_slice(&id.to_ne_bytes());
        buf[4] = 4;
        match decode_wire(&buf, None).unwrap() {
            DecodedFrame::Rtr { id, len } => {
                assert_eq!(id, CanId::Standard(0x456));
                assert_eq!(len, 4);
            }
            other => panic!("expected RTR, got {other:?}"),
        }
    }

    #[test]
    fn error_frame_decoded_separately() {
        let mut buf = [0u8; CAN_FRAME_SIZE];
        let id = CAN_ERR_FLAG | CAN_ERR_BUSOFF;
        buf[0..4].copy_from_slice(&id.to_ne_bytes());
        buf[4] = 8;
        buf[8..16].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        match decode_wire(&buf, None).unwrap() {
            DecodedFrame::Error { class, data, len } => {
                assert_eq!(class, CAN_ERR_BUSOFF);
                assert_eq!(len, 8);
                assert_eq!(&data, &[1, 2, 3, 4, 5, 6, 7, 8]);
            }
            other => panic!("expected error frame, got {other:?}"),
        }
    }

    #[test]
    fn display_classic() {
        let f = Frame::new_classic(CanId::standard(0x123), &[0xDE, 0xAD]).unwrap();
        assert_eq!(format!("{f}"), "123#DEAD");
    }

    #[test]
    fn display_fd_with_brs() {
        let f = Frame::new_fd(
            CanId::standard(0x123),
            &[0xDE, 0xAD],
            FdFlags { brs: true, esi: false },
        )
        .unwrap();
        assert_eq!(format!("{f}"), "123##1DEAD");
    }
}
