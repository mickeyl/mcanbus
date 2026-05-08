//! CAN interface enumeration and netlink control.
//!
//! Higher-level helpers for talking to the kernel about CAN devices —
//! bringing them up/down, querying [`CanState`], cycling them out of
//! BUS-OFF, and listing what's attached.
//!
//! All netlink helpers require `CAP_NET_ADMIN` (root, or fine-grained
//! capabilities); EPERM is returned untranslated when missing.

use std::ffi::CString;
use std::fs;
use std::io;
use std::mem;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::consts::*;

// ── CanState ──────────────────────────────────────────────────────────────

/// CAN controller state, mirroring `enum can_state` from `linux/can/netlink.h`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanState {
    /// Error-active (normal operation).
    ErrorActive,
    /// Error-warning (TEC or REC ≥ 96).
    ErrorWarning,
    /// Error-passive (TEC or REC ≥ 128).
    ErrorPassive,
    /// Bus-off (TEC ≥ 256). The controller is disconnected from the bus
    /// until administrative recovery (the kernel may auto-restart depending
    /// on driver and `restart-ms`).
    BusOff,
    /// Stopped (administratively).
    Stopped,
    /// Sleeping.
    Sleeping,
}

impl CanState {
    fn from_raw(v: u32) -> Option<Self> {
        Some(match v {
            CAN_STATE_ERROR_ACTIVE => Self::ErrorActive,
            CAN_STATE_ERROR_WARNING => Self::ErrorWarning,
            CAN_STATE_ERROR_PASSIVE => Self::ErrorPassive,
            CAN_STATE_BUS_OFF => Self::BusOff,
            CAN_STATE_STOPPED => Self::Stopped,
            CAN_STATE_SLEEPING => Self::Sleeping,
            _ => return None,
        })
    }
}

// ── Interface ─────────────────────────────────────────────────────────────

/// A handle to a Linux network interface, named by string.
#[derive(Clone, Debug)]
pub struct Interface {
    /// Interface name as known to the kernel (e.g. `"can0"`, `"vcan0"`).
    pub name: String,
}

impl Interface {
    /// Construct from a name. Does **not** verify the interface exists.
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    /// Look up the kernel ifindex.
    pub fn index(&self) -> io::Result<u32> {
        let cstr = CString::new(self.name.as_str()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "interface name has nul byte")
        })?;
        // SAFETY: passing a valid NUL-terminated C string.
        let idx = unsafe { libc::if_nametoindex(cstr.as_ptr()) };
        if idx == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(idx)
        }
    }

    /// Whether the interface currently has the IFF_UP flag set, by reading
    /// `/sys/class/net/<iface>/flags`. `Ok(None)` if the file is missing
    /// (interface gone or not a netdev).
    pub fn is_up(&self) -> io::Result<Option<bool>> {
        let path = format!("/sys/class/net/{}/flags", self.name);
        let s = match fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let s = s.trim();
        let s = s.strip_prefix("0x").unwrap_or(s);
        let flags = u32::from_str_radix(s, 16)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // IFF_UP == 0x1.
        Ok(Some(flags & 1 != 0))
    }

    /// Bring the interface up via netlink (`RTM_NEWLINK` with IFF_UP set).
    pub fn set_up(&self) -> io::Result<()> {
        nl_set_iface_up(self.index()?, true)
    }

    /// Bring the interface down via netlink.
    pub fn set_down(&self) -> io::Result<()> {
        nl_set_iface_up(self.index()?, false)
    }

    /// Bring down, settle ~150 ms, bring back up. The standard recipe for
    /// recovering gs_usb-class controllers from BUS-OFF, where the kernel
    /// can't restart the device on its own.
    pub fn cycle(&self) -> io::Result<()> {
        let idx = self.index()?;
        nl_set_iface_up(idx, false)?;
        std::thread::sleep(Duration::from_millis(150));
        nl_set_iface_up(idx, true)
    }

    /// Query the CAN controller state via `RTM_GETLINK`.
    ///
    /// Returns `Ok(None)` when the interface exists but is not a CAN device,
    /// or its driver does not expose a state attribute.
    pub fn state(&self) -> io::Result<Option<CanState>> {
        let raw = get_can_state_raw(self.index()?)?;
        Ok(raw.and_then(CanState::from_raw))
    }

    /// Bitrate as reported by `ip -details -json link show <iface>`. Returns
    /// `Ok(None)` if `ip` is unavailable or the device exposes no bitrate
    /// (e.g. `vcan`). This shells out so we don't have to reproduce the
    /// nested IFLA_LINKINFO/IFLA_INFO_DATA/IFLA_CAN_BITTIMING parsing.
    pub fn bitrate(&self) -> io::Result<Option<u32>> {
        let out = Command::new("ip")
            .args(["-details", "-json", "link", "show", &self.name])
            .output();
        let out = match out {
            Ok(o) if o.status.success() => o,
            _ => return Ok(None),
        };
        let s = std::str::from_utf8(&out.stdout).unwrap_or("");
        // Parse out the first occurrence of `"bitrate":<digits>` without
        // pulling in a JSON crate. The `ip` output format is stable enough.
        let key = "\"bitrate\":";
        let Some(start) = s.find(key) else {
            return Ok(None);
        };
        let tail = &s[start + key.len()..];
        let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        Ok(digits.parse::<u32>().ok())
    }
}

// ── Enumeration ───────────────────────────────────────────────────────────

/// Enumerate all CAN-class interfaces visible in `/sys/class/net`.
///
/// Identifies CAN interfaces by reading `/sys/class/net/<iface>/type`, which
/// for SocketCAN devices reports `280` (`ARPHRD_CAN`). Both real CAN
/// controllers and `vcan` devices are included.
pub fn list_can_interfaces() -> io::Result<Vec<Interface>> {
    const ARPHRD_CAN: &str = "280";
    let mut out = Vec::new();
    let dir = match fs::read_dir("/sys/class/net") {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in dir {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let type_path = entry.path().join("type");
        if !Path::new(&type_path).exists() {
            continue;
        }
        let kind = fs::read_to_string(&type_path).unwrap_or_default();
        if kind.trim() == ARPHRD_CAN {
            out.push(Interface::new(name_str.to_string()));
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// ── Netlink: bring iface up/down ──────────────────────────────────────────

/// Send `RTM_NEWLINK` with the IFF_UP bit toggled and parse the kernel's
/// `NLMSG_ERROR` ack. Returns `Err(EPERM)` when CAP_NET_ADMIN is missing.
fn nl_set_iface_up(ifindex: u32, up: bool) -> io::Result<()> {
    // SAFETY: every libc call below has its return value checked, the fd is
    // closed on every exit path, and the buffers are sized exactly for the
    // structs we encode/decode.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut local: libc::sockaddr_nl = mem::zeroed();
        local.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        if libc::bind(
            fd,
            (&local as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        ) < 0
        {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        let hdr_len = mem::size_of::<libc::nlmsghdr>();
        let info_len = mem::size_of::<libc::ifinfomsg>();
        let total_len = hdr_len + info_len;

        let mut buf = [0u8; 64];
        let hdr = libc::nlmsghdr {
            nlmsg_len: total_len as u32,
            nlmsg_type: libc::RTM_NEWLINK,
            nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
            nlmsg_seq: 1,
            nlmsg_pid: 0,
        };
        std::ptr::write_unaligned(buf.as_mut_ptr() as *mut libc::nlmsghdr, hdr);

        let mut info: libc::ifinfomsg = mem::zeroed();
        info.ifi_family = libc::AF_UNSPEC as u8;
        info.ifi_index = ifindex as libc::c_int;
        info.ifi_flags = if up { libc::IFF_UP as libc::c_uint } else { 0 };
        info.ifi_change = libc::IFF_UP as libc::c_uint;
        std::ptr::write_unaligned(buf.as_mut_ptr().add(hdr_len) as *mut libc::ifinfomsg, info);

        let mut kernel: libc::sockaddr_nl = mem::zeroed();
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;

        let n = libc::sendto(
            fd,
            buf.as_ptr().cast::<libc::c_void>(),
            total_len,
            0,
            (&kernel as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if n < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        let mut rbuf = [0u8; 4096];
        let n = libc::recv(fd, rbuf.as_mut_ptr().cast::<libc::c_void>(), rbuf.len(), 0);
        let recv_err = if n < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        libc::close(fd);
        if let Some(e) = recv_err {
            return Err(e);
        }

        let n = n as usize;
        if n < hdr_len {
            return Err(io::Error::other("netlink response shorter than nlmsghdr"));
        }
        let resp = std::ptr::read_unaligned(rbuf.as_ptr() as *const libc::nlmsghdr);
        if i32::from(resp.nlmsg_type) != libc::NLMSG_ERROR {
            return Err(io::Error::other(format!(
                "unexpected netlink response type {}",
                resp.nlmsg_type
            )));
        }
        if n < hdr_len + mem::size_of::<i32>() {
            return Err(io::Error::other("netlink error response truncated"));
        }
        // First field of nlmsgerr is `int error` — negative errno on failure, 0 on ack.
        let err_code = std::ptr::read_unaligned(rbuf.as_ptr().add(hdr_len) as *const i32);
        if err_code == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(-err_code))
        }
    }
}

// ── Netlink: read CAN controller state ────────────────────────────────────

const IFLA_LINKINFO: u16 = 18;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const IFLA_CAN_STATE: u16 = 4;
const NLA_TYPE_MASK: u16 = 0x3fff;

#[inline]
fn nla_align(len: usize) -> usize {
    (len + 3) & !3
}

/// Find the first netlink attribute with the given type in a TLV blob.
fn nl_find_attr(buf: &[u8], nla_type: u16) -> Option<&[u8]> {
    let mut off = 0;
    while off + 4 <= buf.len() {
        let nla_len = u16::from_ne_bytes(buf[off..off + 2].try_into().unwrap()) as usize;
        let ty = u16::from_ne_bytes(buf[off + 2..off + 4].try_into().unwrap()) & NLA_TYPE_MASK;
        if nla_len < 4 || off + nla_len > buf.len() {
            break;
        }
        if ty == nla_type {
            return Some(&buf[off + 4..off + nla_len]);
        }
        off += nla_align(nla_len);
    }
    None
}

fn get_can_state_raw(ifindex: u32) -> io::Result<Option<u32>> {
    // SAFETY: same playbook as `nl_set_iface_up` — every fd path closes,
    // every kernel buffer access is bounded.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC,
            libc::NETLINK_ROUTE,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let mut local: libc::sockaddr_nl = mem::zeroed();
        local.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        if libc::bind(
            fd,
            (&local as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        ) < 0
        {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        let hdr_len = mem::size_of::<libc::nlmsghdr>();
        let info_len = mem::size_of::<libc::ifinfomsg>();
        let total_len = hdr_len + info_len;

        let mut buf = [0u8; 64];
        let hdr = libc::nlmsghdr {
            nlmsg_len: total_len as u32,
            nlmsg_type: libc::RTM_GETLINK,
            nlmsg_flags: (libc::NLM_F_REQUEST | libc::NLM_F_ACK) as u16,
            nlmsg_seq: 2,
            nlmsg_pid: 0,
        };
        std::ptr::write_unaligned(buf.as_mut_ptr() as *mut libc::nlmsghdr, hdr);

        let mut info: libc::ifinfomsg = mem::zeroed();
        info.ifi_family = libc::AF_UNSPEC as u8;
        info.ifi_index = ifindex as libc::c_int;
        std::ptr::write_unaligned(buf.as_mut_ptr().add(hdr_len) as *mut libc::ifinfomsg, info);

        let mut kernel: libc::sockaddr_nl = mem::zeroed();
        kernel.nl_family = libc::AF_NETLINK as libc::sa_family_t;

        let n = libc::sendto(
            fd,
            buf.as_ptr().cast::<libc::c_void>(),
            total_len,
            0,
            (&kernel as *const libc::sockaddr_nl).cast::<libc::sockaddr>(),
            mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        );
        if n < 0 {
            let e = io::Error::last_os_error();
            libc::close(fd);
            return Err(e);
        }

        let mut rbuf = [0u8; 8192];
        let n = libc::recv(fd, rbuf.as_mut_ptr().cast::<libc::c_void>(), rbuf.len(), 0);
        let recv_err = if n < 0 {
            Some(io::Error::last_os_error())
        } else {
            None
        };
        libc::close(fd);
        if let Some(e) = recv_err {
            return Err(e);
        }

        let n = n as usize;
        if n < hdr_len + info_len {
            return Ok(None);
        }
        let resp = std::ptr::read_unaligned(rbuf.as_ptr() as *const libc::nlmsghdr);
        if i32::from(resp.nlmsg_type) == libc::NLMSG_ERROR {
            // `error` is the first field of nlmsgerr (signed; negative errno).
            let err_code = std::ptr::read_unaligned(rbuf.as_ptr().add(hdr_len) as *const i32);
            if err_code == 0 {
                return Ok(None);
            }
            return Err(io::Error::from_raw_os_error(-err_code));
        }

        // Skip ifinfomsg (16 bytes), then walk top-level attributes for IFLA_LINKINFO.
        let attrs_start = hdr_len + info_len;
        if attrs_start >= n {
            return Ok(None);
        }
        let attrs = &rbuf[attrs_start..n];
        let Some(linkinfo) = nl_find_attr(attrs, IFLA_LINKINFO) else {
            return Ok(None);
        };

        // Within IFLA_LINKINFO, find IFLA_INFO_KIND (must be "can") and IFLA_INFO_DATA.
        let kind = nl_find_attr(linkinfo, IFLA_INFO_KIND)
            .map(|b| {
                // Trim trailing NUL.
                let len = b.iter().position(|&c| c == 0).unwrap_or(b.len());
                std::str::from_utf8(&b[..len]).unwrap_or("")
            })
            .unwrap_or("");
        if kind != "can" {
            return Ok(None);
        }
        let Some(info_data) = nl_find_attr(linkinfo, IFLA_INFO_DATA) else {
            return Ok(None);
        };

        let Some(state_bytes) = nl_find_attr(info_data, IFLA_CAN_STATE) else {
            return Ok(None);
        };
        if state_bytes.len() < 4 {
            return Ok(None);
        }
        let state = u32::from_ne_bytes(state_bytes[..4].try_into().unwrap());
        Ok(Some(state))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_does_not_crash() {
        // We don't assert content (CI may or may not have CAN devices) — just
        // that the call returns without error on a /sys-equipped system.
        let _ = list_can_interfaces().unwrap_or_default();
    }

    #[test]
    fn nonexistent_index_errs() {
        let r = Interface::new("definitely_not_an_iface_xyz_42").index();
        assert!(r.is_err());
    }

    #[test]
    fn canstate_round_trip() {
        for raw in 0..=5 {
            let s = CanState::from_raw(raw).unwrap();
            // Just ensure mapping is stable.
            let _ = format!("{s:?}");
        }
        assert!(CanState::from_raw(99).is_none());
    }
}
