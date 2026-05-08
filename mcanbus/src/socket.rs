//! `CAN_RAW` socket wrapper.
//!
//! [`Socket`] is the workhorse: open it on an interface, send and receive
//! frames, optionally batch with [`Socket::send_batch`] / [`Socket::recv_batch`]
//! for high-throughput pipelines.
//!
//! # Concurrency
//!
//! [`Socket`] is [`Send`] + [`Sync`]. The kernel serialises concurrent
//! access on a `CAN_RAW` socket, so a writer thread and a reader thread can
//! share the same socket safely. For multiple readers, prefer [`crate::reader`]
//! over racing recv()s on the same fd.
//!
//! # Timestamps
//!
//! By default the socket asks the kernel for `SO_TIMESTAMPING` (hardware
//! preferred, software fallback). When that is denied (older kernels, no
//! capabilities), it falls back to `SO_TIMESTAMPNS`. Either way, frames
//! returned by [`Socket::recv`] carry a `timestamp_ns` populated from the
//! best source available.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::consts::*;
use crate::frame::{decode_wire, DecodedFrame, Frame, WireFrame};

// ── CanFilter ─────────────────────────────────────────────────────────────

/// A single `CAN_RAW_FILTER` entry. The kernel delivers a frame when
/// `frame.can_id & mask == id & mask`.
///
/// The high bits of `id` and `mask` carry the same flag conventions as
/// `can_id` itself: set [`CAN_EFF_FLAG`] in `mask` to require that the
/// extended/standard distinction matches.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CanFilter {
    /// The ID pattern (with EFF/RTR/ERR flag bits where appropriate).
    pub id: u32,
    /// The mask applied to both `id` and the incoming `can_id` for comparison.
    pub mask: u32,
}

impl CanFilter {
    /// Match exactly one standard ID.
    pub const fn standard_exact(id: u16) -> Self {
        Self {
            id: (id as u32) & CAN_SFF_MASK,
            // Match the SFF id bits and require EFF=0.
            mask: CAN_SFF_MASK | CAN_EFF_FLAG,
        }
    }

    /// Match exactly one extended ID.
    pub const fn extended_exact(id: u32) -> Self {
        Self {
            id: (id & CAN_EFF_MASK) | CAN_EFF_FLAG,
            mask: CAN_EFF_MASK | CAN_EFF_FLAG,
        }
    }

    /// Match any ID where `id & mask` equals `pattern & mask` (standard frames only).
    pub const fn standard_masked(pattern: u16, mask: u16) -> Self {
        Self {
            id: (pattern as u32) & CAN_SFF_MASK,
            mask: ((mask as u32) & CAN_SFF_MASK) | CAN_EFF_FLAG,
        }
    }
}

// ── Public types ──────────────────────────────────────────────────────────

/// Timestamp source to request from the kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Timestamping {
    /// Best available: `SO_TIMESTAMPING` with hardware preferred, software
    /// fall-back, then `SO_TIMESTAMPNS` if neither was granted.
    #[default]
    BestAvailable,
    /// Force `SO_TIMESTAMPNS` (kernel software, nanosecond resolution).
    Software,
    /// Don't request kernel timestamps; the socket synthesises one from
    /// `SystemTime::now()` at recv time.
    None,
}

/// Configuration for [`Socket::open`].
#[derive(Clone, Debug)]
pub struct OpenOpts {
    /// Enable `CAN_RAW_FD_FRAMES` so CAN-FD traffic is delivered. Non-fatal
    /// if the controller doesn't support FD: the socket falls back to
    /// classic-only delivery.
    pub fd: bool,
    /// `SO_RCVBUF` in bytes. `None` leaves the kernel default.
    pub recv_buf_bytes: Option<usize>,
    /// `SO_SNDBUF` in bytes. `None` leaves the kernel default.
    pub send_buf_bytes: Option<usize>,
    /// Kernel timestamp source.
    pub timestamping: Timestamping,
    /// `SO_RCVTIMEO`. A small value (e.g. 500 ms) lets a read loop poll a
    /// stop flag without blocking forever.
    pub recv_timeout: Option<Duration>,
    /// `CAN_RAW_FILTER` entries. When `Some` (even if empty), the kernel
    /// receive filter is overridden: only frames matching one of these
    /// patterns are delivered. When `None` the kernel default applies (all
    /// frames matching the bound interface).
    ///
    /// An empty `Some(vec![])` is the same as `clear_filter`: no frames at
    /// all (useful for an error-only socket).
    pub filter: Option<Vec<CanFilter>>,
    /// Mask passed to `CAN_RAW_ERR_FILTER`. When set, error frames matching
    /// these classes are delivered alongside data frames as
    /// [`crate::frame::DecodedFrame::Error`] (only visible via
    /// [`Socket::recv_decoded`]).
    pub error_filter: Option<u32>,
    /// Clear the default RX filter so no classic frames are delivered. Useful
    /// for an error-only monitor socket. Equivalent to `filter: Some(vec![])`.
    pub clear_filter: bool,
    /// Set `O_NONBLOCK` on the socket.
    pub nonblocking: bool,
}

impl Default for OpenOpts {
    fn default() -> Self {
        Self {
            fd: true,
            recv_buf_bytes: Some(8 * 1024 * 1024),
            send_buf_bytes: Some(1024 * 1024),
            timestamping: Timestamping::default(),
            recv_timeout: Some(Duration::from_millis(500)),
            filter: None,
            error_filter: None,
            clear_filter: false,
            nonblocking: false,
        }
    }
}

/// A `CAN_RAW` socket bound to a specific interface.
///
/// Dropped sockets close the underlying file descriptor.
pub struct Socket {
    fd: RawFd,
    fd_rx_enabled: bool,
}

// SAFETY: file descriptors are kernel-managed; concurrent recv/send on a
// CAN_RAW socket is safe at the kernel boundary.
unsafe impl Send for Socket {}
unsafe impl Sync for Socket {}

impl AsRawFd for Socket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl IntoRawFd for Socket {
    fn into_raw_fd(self) -> RawFd {
        let fd = self.fd;
        mem::forget(self);
        fd
    }
}

impl FromRawFd for Socket {
    /// Adopt an already-bound `CAN_RAW` socket. The caller is responsible for
    /// having configured FD frames; the resulting [`Socket`] assumes FD is
    /// enabled (so it always reads up to 72 bytes, which is safe — classic
    /// frames will simply be delivered as 16-byte reads).
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self {
            fd,
            fd_rx_enabled: true,
        }
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        // SAFETY: we own the fd; close once.
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ── Open ──────────────────────────────────────────────────────────────────

#[repr(C)]
struct SockaddrCan {
    can_family: libc::sa_family_t,
    can_ifindex: libc::c_int,
    can_addr: [u8; 8],
}

#[repr(C)]
struct Ifreq {
    ifr_name: [u8; libc::IFNAMSIZ],
    ifr_ifindex: libc::c_int,
}

impl Socket {
    /// Open and bind a `CAN_RAW` socket on `iface`.
    pub fn open(iface: &str, opts: &OpenOpts) -> io::Result<Self> {
        let name_bytes = iface.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() >= libc::IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "interface name has invalid length",
            ));
        }

        // SAFETY: we check every libc return value, close the fd on any error
        // before returning, and only treat the descriptor as owned once we've
        // wrapped it in a `Socket` that runs `close` in `Drop`.
        let socket_flags = libc::SOCK_RAW | libc::SOCK_CLOEXEC;
        let fd = unsafe { libc::socket(PF_CAN, socket_flags, CAN_RAW) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Use a guard so any early return closes the fd.
        struct FdGuard(RawFd);
        impl Drop for FdGuard {
            fn drop(&mut self) {
                if self.0 >= 0 {
                    // SAFETY: the guard owns the fd until `defuse` runs.
                    unsafe {
                        libc::close(self.0);
                    }
                }
            }
        }
        impl FdGuard {
            fn defuse(mut self) -> RawFd {
                let fd = self.0;
                self.0 = -1;
                fd
            }
        }
        let guard = FdGuard(fd);

        // Resolve interface index via SIOCGIFINDEX.
        let mut ifr: Ifreq = unsafe { mem::zeroed() };
        ifr.ifr_name[..name_bytes.len()].copy_from_slice(name_bytes);
        // SAFETY: ifr is correctly sized; ioctl writes ifr_ifindex on success.
        if unsafe { libc::ioctl(fd, SIOCGIFINDEX, &mut ifr as *mut Ifreq) } < 0 {
            return Err(io::Error::last_os_error());
        }

        // Configure CAN-FD reception (best-effort: not all controllers support FD).
        // Note: this only enables RX-side FD delivery. Whether the controller
        // can transmit FD frames is a separate question — the answer comes
        // when the first FD send returns EINVAL (or doesn't).
        let mut fd_rx_enabled = false;
        if opts.fd {
            let enable: libc::c_int = 1;
            // SAFETY: setsockopt with valid level/name and a c_int by reference.
            let r = unsafe {
                libc::setsockopt(
                    fd,
                    SOL_CAN_RAW,
                    CAN_RAW_FD_FRAMES,
                    (&enable as *const libc::c_int).cast::<libc::c_void>(),
                    mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            fd_rx_enabled = r == 0;
        }

        // Receive filter. `filter` takes precedence over `clear_filter`.
        let filter_to_apply: Option<&[CanFilter]> = match (&opts.filter, opts.clear_filter) {
            (Some(f), _) => Some(f.as_slice()),
            (None, true) => Some(&[]),
            (None, false) => None,
        };
        if let Some(entries) = filter_to_apply {
            let (ptr, len) = if entries.is_empty() {
                (std::ptr::null(), 0)
            } else {
                (
                    entries.as_ptr().cast::<libc::c_void>(),
                    std::mem::size_of_val(entries) as libc::socklen_t,
                )
            };
            // SAFETY: setsockopt with a CanFilter array (or null+0 for "drop all").
            let r = unsafe { libc::setsockopt(fd, SOL_CAN_RAW, CAN_RAW_FILTER, ptr, len) };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if let Some(mask) = opts.error_filter {
            // SAFETY: setsockopt with valid level/name and a u32 by reference.
            let r = unsafe {
                libc::setsockopt(
                    fd,
                    SOL_CAN_RAW,
                    CAN_RAW_ERR_FILTER,
                    (&mask as *const u32).cast::<libc::c_void>(),
                    mem::size_of::<u32>() as libc::socklen_t,
                )
            };
            if r < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        if let Some(bytes) = opts.recv_buf_bytes {
            let v = bytes as libc::c_int;
            // SAFETY: setsockopt with valid level/name and a c_int by reference.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVBUF,
                    (&v as *const libc::c_int).cast::<libc::c_void>(),
                    mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }
        if let Some(bytes) = opts.send_buf_bytes {
            let v = bytes as libc::c_int;
            // SAFETY: setsockopt with valid level/name and a c_int by reference.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&v as *const libc::c_int).cast::<libc::c_void>(),
                    mem::size_of::<libc::c_int>() as libc::socklen_t,
                );
            }
        }

        // Timestamps. We try SO_TIMESTAMPING first (best resolution); if that
        // fails, fall back to SO_TIMESTAMPNS. Both are best-effort: a denied
        // setsockopt just means recv() will synthesise a timestamp.
        match opts.timestamping {
            Timestamping::BestAvailable => {
                let ts_flags: u32 = SOF_TIMESTAMPING_RX_HARDWARE
                    | SOF_TIMESTAMPING_RX_SOFTWARE
                    | SOF_TIMESTAMPING_SOFTWARE
                    | SOF_TIMESTAMPING_RAW_HARDWARE;
                // SAFETY: setsockopt with a valid u32 reference.
                let r = unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPING,
                        (&ts_flags as *const u32).cast::<libc::c_void>(),
                        mem::size_of::<u32>() as libc::socklen_t,
                    )
                };
                if r != 0 {
                    let enable: libc::c_int = 1;
                    // SAFETY: setsockopt with a valid c_int reference.
                    unsafe {
                        libc::setsockopt(
                            fd,
                            libc::SOL_SOCKET,
                            libc::SO_TIMESTAMPNS,
                            (&enable as *const libc::c_int).cast::<libc::c_void>(),
                            mem::size_of::<libc::c_int>() as libc::socklen_t,
                        );
                    }
                }
            }
            Timestamping::Software => {
                let enable: libc::c_int = 1;
                // SAFETY: setsockopt with a valid c_int reference.
                unsafe {
                    libc::setsockopt(
                        fd,
                        libc::SOL_SOCKET,
                        libc::SO_TIMESTAMPNS,
                        (&enable as *const libc::c_int).cast::<libc::c_void>(),
                        mem::size_of::<libc::c_int>() as libc::socklen_t,
                    );
                }
            }
            Timestamping::None => {}
        }

        if let Some(timeout) = opts.recv_timeout {
            let tv = libc::timeval {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_usec: timeout.subsec_micros() as libc::suseconds_t,
            };
            // SAFETY: setsockopt with a valid timeval reference.
            unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_RCVTIMEO,
                    (&tv as *const libc::timeval).cast::<libc::c_void>(),
                    mem::size_of::<libc::timeval>() as libc::socklen_t,
                );
            }
        }

        if opts.nonblocking {
            // SAFETY: fcntl with F_SETFL on a valid fd.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
            if flags >= 0 {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }

        let mut addr: SockaddrCan = unsafe { mem::zeroed() };
        addr.can_family = AF_CAN as libc::sa_family_t;
        addr.can_ifindex = ifr.ifr_ifindex;
        // SAFETY: bind with a correctly-sized SockaddrCan.
        let r = unsafe {
            libc::bind(
                fd,
                (&addr as *const SockaddrCan).cast::<libc::sockaddr>(),
                mem::size_of::<SockaddrCan>() as libc::socklen_t,
            )
        };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }

        let fd = guard.defuse();
        Ok(Self { fd, fd_rx_enabled })
    }

    /// Whether `CAN_RAW_FD_FRAMES` is enabled on this socket.
    ///
    /// **This only describes the receive path.** It says nothing about
    /// whether the underlying controller can *transmit* CAN-FD frames. Many
    /// classic-only controllers (notably the gs_usb family) accept the
    /// setsockopt but reject FD frames at TX with `EINVAL`. The honest test
    /// for FD-TX is to send a small FD frame and inspect the result.
    pub fn fd_rx_enabled(&self) -> bool {
        self.fd_rx_enabled
    }

    /// Receive a single data frame.
    ///
    /// Returns `Ok(None)` on `SO_RCVTIMEO` expiry, on `EAGAIN` (non-blocking),
    /// or when the kernel delivered a frame the caller usually doesn't want
    /// (RTR, error frame). Use [`Socket::recv_decoded`] to surface those.
    pub fn recv(&self) -> io::Result<Option<Frame>> {
        match self.recv_decoded()? {
            Some(DecodedFrame::Data(frame)) => Ok(Some(frame)),
            Some(DecodedFrame::Rtr { .. }) | Some(DecodedFrame::Error { .. }) | None => Ok(None),
        }
    }

    /// Receive a single frame and report it as a [`DecodedFrame`], so callers
    /// can distinguish data, RTR, and error frames.
    pub fn recv_decoded(&self) -> io::Result<Option<DecodedFrame>> {
        let mut buf = [0u8; CANFD_FRAME_SIZE];
        let mut cmsg_buf = [0u8; 256];
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast::<libc::c_void>(),
            iov_len: CANFD_FRAME_SIZE,
        };
        // SAFETY: zero-init a POD msghdr; we fill the fields below.
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsg_buf.as_mut_ptr().cast::<libc::c_void>();
        msg.msg_controllen = cmsg_buf.len() as _;

        // SAFETY: msg is correctly initialised; recvmsg writes into our buffers.
        let n = unsafe { libc::recvmsg(self.fd, &mut msg, 0) };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                // Timeout, non-blocking would-block, or signal interruption →
                // treat as "no frame this round" so the caller's stop flag
                // gets a chance to break the loop on the next iteration.
                Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(None),
                _ => Err(err),
            };
        }
        let n = n as usize;
        if n < CAN_FRAME_SIZE {
            return Ok(None);
        }

        let timestamp_ns = extract_timestamp_ns(&msg).or_else(|| {
            // Synthesised fallback so callers always get *something*.
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as u64)
        });

        Ok(decode_wire(&buf[..n], timestamp_ns))
    }

    /// Send a single frame. Errors are propagated verbatim from the kernel —
    /// notably `ENOBUFS` (TX queue full) and `ENETDOWN` (interface dropped).
    pub fn send(&self, frame: &Frame) -> io::Result<()> {
        let wire = WireFrame::from_frame(frame);
        let size = WireFrame::wire_size(frame.is_fd());
        // SAFETY: write to a valid fd from a buffer of `size` bytes that lives
        // inside the local `wire`. WireFrame is repr(C) and >= 72 bytes.
        let n = unsafe {
            libc::write(
                self.fd,
                (&wire as *const WireFrame).cast::<libc::c_void>(),
                size,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if (n as usize) != size {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short CAN frame write",
            ));
        }
        Ok(())
    }

    /// Receive up to `out.len()` frames in a single `recvmmsg` syscall.
    ///
    /// Returns the number of frames actually delivered. On timeout / EAGAIN
    /// returns `Ok(0)`. Each delivered slot is filled with a
    /// [`DecodedFrame::Data`] when possible; RTR / error frames are
    /// **dropped** here (they have no good place in a batch). Use
    /// [`Socket::recv_decoded`] in a loop if you need them.
    ///
    /// `out` is a slice of `MaybeUninit`-style holders; use [`Frame::default`]
    /// or [`Frame`]-shaped zeros to size the buffer (simplest is
    /// `let mut buf = [Frame::zeroed(); 64]`).
    pub fn recv_batch(&self, out: &mut [Frame]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        const MAX_BATCH: usize = 64;
        let count = out.len().min(MAX_BATCH);

        // Per-slot scratch buffers for frame payload + cmsg ancillary data.
        let mut bufs = [[0u8; CANFD_FRAME_SIZE]; MAX_BATCH];
        let mut cmsgs = [[0u8; 128]; MAX_BATCH];
        let mut iovs: [libc::iovec; MAX_BATCH] = unsafe { mem::zeroed() };
        let mut msgs: [libc::mmsghdr; MAX_BATCH] = unsafe { mem::zeroed() };

        for i in 0..count {
            iovs[i] = libc::iovec {
                iov_base: bufs[i].as_mut_ptr().cast::<libc::c_void>(),
                iov_len: CANFD_FRAME_SIZE,
            };
            msgs[i].msg_hdr.msg_iov = &mut iovs[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
            msgs[i].msg_hdr.msg_control = cmsgs[i].as_mut_ptr().cast::<libc::c_void>();
            msgs[i].msg_hdr.msg_controllen = cmsgs[i].len() as _;
        }

        // SAFETY: msgs is initialised; recvmmsg fills msg_len per slot.
        let n = unsafe {
            libc::recvmmsg(
                self.fd,
                msgs.as_mut_ptr(),
                count as libc::c_uint,
                libc::MSG_WAITFORONE,
                std::ptr::null_mut(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::EAGAIN) | Some(libc::EINTR) => Ok(0),
                _ => Err(err),
            };
        }
        let n = n as usize;

        let mut delivered = 0;
        for i in 0..n {
            let bytes = msgs[i].msg_len as usize;
            if bytes < CAN_FRAME_SIZE {
                continue;
            }
            let timestamp_ns = extract_timestamp_ns(&msgs[i].msg_hdr).or_else(|| {
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .map(|d| d.as_nanos() as u64)
            });
            if let Some(DecodedFrame::Data(frame)) = decode_wire(&bufs[i][..bytes], timestamp_ns) {
                out[delivered] = frame;
                delivered += 1;
            }
        }
        Ok(delivered)
    }

    /// Send up to `frames.len()` frames in a single `sendmmsg` syscall.
    /// Returns the number of frames actually accepted by the kernel.
    pub fn send_batch(&self, frames: &[Frame]) -> io::Result<usize> {
        if frames.is_empty() {
            return Ok(0);
        }
        const MAX_BATCH: usize = 64;
        let count = frames.len().min(MAX_BATCH);

        let mut wires = [WireFrame::zeroed(); MAX_BATCH];
        let mut iovs: [libc::iovec; MAX_BATCH] = unsafe { mem::zeroed() };
        let mut msgs: [libc::mmsghdr; MAX_BATCH] = unsafe { mem::zeroed() };

        for i in 0..count {
            wires[i] = WireFrame::from_frame(&frames[i]);
            iovs[i] = libc::iovec {
                iov_base: (&wires[i] as *const WireFrame as *mut libc::c_void),
                iov_len: WireFrame::wire_size(frames[i].is_fd()),
            };
            msgs[i].msg_hdr.msg_iov = &mut iovs[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
        }

        // SAFETY: msgs is initialised, points to live wires/iovs on this stack frame.
        let n = unsafe { libc::sendmmsg(self.fd, msgs.as_mut_ptr(), count as libc::c_uint, 0) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    /// Duplicate the socket's file descriptor (`dup`). Useful when one thread
    /// owns the receiver and another owns the sender.
    pub fn try_clone(&self) -> io::Result<Self> {
        // SAFETY: dup of a valid fd; on success we own the new fd.
        let new_fd = unsafe { libc::dup(self.fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: new_fd,
            fd_rx_enabled: self.fd_rx_enabled,
        })
    }
}

// ── Timestamp extraction from cmsg ────────────────────────────────────────

fn extract_timestamp_ns(msg: &libc::msghdr) -> Option<u64> {
    let mut best: Option<u64> = None;

    // SAFETY: we walk the cmsg chain using the documented CMSG_FIRSTHDR /
    // CMSG_NXTHDR macros against a msghdr we either zeroed or that recvmsg
    // populated. Each cmsg dereference is guarded by `is_null()`.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            let hdr = &*cmsg;
            if hdr.cmsg_level == libc::SOL_SOCKET {
                if hdr.cmsg_type == libc::SO_TIMESTAMPING {
                    let data = libc::CMSG_DATA(cmsg) as *const libc::timespec;
                    let hw = &*data.add(2);
                    if hw.tv_sec != 0 || hw.tv_nsec != 0 {
                        return Some(timespec_to_ns(hw));
                    }
                    let sw = &*data;
                    if sw.tv_sec != 0 || sw.tv_nsec != 0 {
                        best = Some(timespec_to_ns(sw));
                    }
                } else if hdr.cmsg_type == libc::SO_TIMESTAMPNS && best.is_none() {
                    let ts = &*(libc::CMSG_DATA(cmsg) as *const libc::timespec);
                    best = Some(timespec_to_ns(ts));
                }
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }

    best
}

#[inline]
fn timespec_to_ns(ts: &libc::timespec) -> u64 {
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

// ── Frame helper for batch buffers ────────────────────────────────────────

impl Frame {
    /// A zero-initialised placeholder useful when sizing batch RX buffers.
    /// The result is a valid classic frame with id = 0 and no payload; it
    /// will be overwritten by [`Socket::recv_batch`].
    pub const fn zeroed() -> Self {
        Self {
            id: crate::frame::CanId::Standard(0),
            kind: crate::frame::FrameKind::Classic,
            len: 0,
            data_buf: [0; crate::frame::MAX_FD_DATA_LEN],
            timestamp_ns: None,
        }
    }
}
