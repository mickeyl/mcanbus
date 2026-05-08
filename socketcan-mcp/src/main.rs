//! MCP server exposing SocketCAN interfaces to AI agents.
//!
//! Transport is stdio. Tools are gated by an allowlist of interface names —
//! agents cannot touch any device that isn't explicitly permitted.
//!
//! Configuration via environment variables:
//!
//! | Variable                        | Effect                                                                                  |
//! | ------------------------------- | --------------------------------------------------------------------------------------- |
//! | `SOCKETCAN_MCP_INTERFACES`      | Comma-separated allowlist (e.g. `vcan0,can0`). If unset, **no** interface is allowed.   |
//! | `SOCKETCAN_MCP_READONLY`        | When `1`/`true`, send-side tools refuse with a permission error.                        |
//! | `RUST_LOG`                      | Standard `tracing` filter; defaults to `info` on stderr.                                |

use std::time::{Duration, Instant};

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, ErrorData as McpError, Implementation, ProtocolVersion,
    ServerCapabilities, ServerInfo,
};
use rmcp::transport::stdio;
use rmcp::{tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

use mcanbus::iface::{list_can_interfaces, Interface};
use mcanbus::isotp;
use mcanbus::{CanFilter, CanId, FdFlags, Frame, OpenOpts, Socket};

// ── Config ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct Config {
    allowlist: Vec<String>,
    readonly: bool,
}

impl Config {
    fn from_env() -> Self {
        let allowlist = std::env::var("SOCKETCAN_MCP_INTERFACES")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let readonly = matches!(
            std::env::var("SOCKETCAN_MCP_READONLY")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "on"
        );
        Self {
            allowlist,
            readonly,
        }
    }

    fn check_iface(&self, iface: &str) -> Result<(), McpError> {
        if !self.allowlist.iter().any(|a| a == iface) {
            return Err(McpError::invalid_params(
                format!(
                    "interface '{iface}' is not in the allowlist (set SOCKETCAN_MCP_INTERFACES)"
                ),
                None,
            ));
        }
        Ok(())
    }

    fn check_writable(&self) -> Result<(), McpError> {
        if self.readonly {
            return Err(McpError::invalid_params(
                "server is in read-only mode (SOCKETCAN_MCP_READONLY=1)",
                None,
            ));
        }
        Ok(())
    }
}

// ── Tool input/output types ───────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct IfaceArg {
    /// Interface name (e.g. `"can0"`, `"vcan0"`).
    iface: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SendFrameArgs {
    /// Interface name to transmit on (must be in the server allowlist).
    iface: String,
    /// CAN identifier as a hex string (with or without `0x` prefix).
    id: String,
    /// Whether `id` is a 29-bit extended identifier. Default `false`.
    #[serde(default)]
    extended: bool,
    /// Payload as a hex string (even number of hex digits).
    #[serde(default)]
    data: String,
    /// Mark this frame as CAN-FD. Default `false`.
    #[serde(default)]
    fd: bool,
    /// Bit-Rate Switch (only meaningful when `fd=true`). Default `false`.
    #[serde(default)]
    brs: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct CaptureArgs {
    /// Interface to listen on.
    iface: String,
    /// Maximum time to wait, in milliseconds. The capture returns as soon as
    /// `max_frames` is reached or the timeout expires, whichever comes first.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Maximum number of frames to return. Default 256, hard cap 4096.
    #[serde(default = "default_max_frames")]
    max_frames: usize,
    /// Optional filter — if set, only matching frames are delivered.
    #[serde(default)]
    filter: Option<FilterSpec>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SendAndCaptureArgs {
    /// Interface for both TX and RX. (Two interfaces on the same wire is also
    /// fine — set `rx_iface` to the second one.)
    iface: String,
    /// Optional separate RX interface; defaults to `iface`.
    #[serde(default)]
    rx_iface: Option<String>,
    /// Frame to send (object with id/data/extended/fd/brs).
    frame: SendFrameInline,
    /// Capture window after sending, in milliseconds.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Maximum number of response frames to capture.
    #[serde(default = "default_max_frames")]
    max_frames: usize,
    /// Filter for the response capture.
    #[serde(default)]
    filter: Option<FilterSpec>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SendFrameInline {
    /// CAN identifier as a hex string.
    id: String,
    /// Whether `id` is a 29-bit extended identifier.
    #[serde(default)]
    extended: bool,
    /// Payload as a hex string.
    #[serde(default)]
    data: String,
    /// CAN-FD frame.
    #[serde(default)]
    fd: bool,
    /// Bit-Rate Switch (FD only).
    #[serde(default)]
    brs: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct FilterSpec {
    /// One or more filter entries; a frame is delivered if it matches any.
    entries: Vec<FilterEntry>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct IsoTpRequestArgs {
    /// Interface to use for both transmit and receive.
    iface: String,
    /// Tester-side ID we transmit on. Hex string (with or without `0x`).
    tx_id: String,
    /// ECU-side ID we receive on. Hex string.
    rx_id: String,
    /// Whether `tx_id` and `rx_id` are 29-bit extended identifiers.
    #[serde(default)]
    extended: bool,
    /// Request payload as a hex string. The library wraps it in ISO-TP
    /// (Single Frame for ≤ 7 bytes, otherwise First Frame + Consecutive
    /// Frames with Flow Control).
    payload: String,
    /// Padding byte for unused bytes within an 8-byte frame, hex.
    /// Default `"CC"` (Scania-typical).
    #[serde(default = "default_padding")]
    padding: String,
    /// Total timeout for the entire request/response cycle, milliseconds.
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    /// Block Size we advertise in our outgoing Flow Control. Default `0`.
    #[serde(default)]
    block_size: u8,
    /// SeparationTime (ms) we ask the ECU to honour between consecutive
    /// frames it sends to us. Default `0`.
    #[serde(default)]
    st_min_ms: u8,
}

fn default_padding() -> String {
    "CC".to_string()
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct FilterEntry {
    /// ID pattern as hex string.
    id: String,
    /// Mask as hex string.
    mask: String,
    /// Whether this is an extended-ID filter.
    #[serde(default)]
    extended: bool,
}

fn default_timeout_ms() -> u64 {
    1000
}
fn default_max_frames() -> usize {
    256
}

#[derive(Debug, Serialize)]
struct FrameJson {
    id: String,
    extended: bool,
    fd: bool,
    brs: bool,
    esi: bool,
    dlc: u8,
    data: String,
    timestamp_ns: Option<u64>,
}

impl FrameJson {
    fn from_frame(f: &Frame) -> Self {
        let extended = matches!(f.id, CanId::Extended(_));
        let id_str = match f.id {
            CanId::Standard(id) => format!("{id:03X}"),
            CanId::Extended(id) => format!("{id:08X}"),
        };
        let kind = f.kind;
        let (fd, brs, esi) = match kind {
            mcanbus::FrameKind::Classic => (false, false, false),
            mcanbus::FrameKind::Fd(flags) => (true, flags.brs, flags.esi),
        };
        let data_hex = f
            .data()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<String>();
        Self {
            id: id_str,
            extended,
            fd,
            brs,
            esi,
            dlc: f.len,
            data: data_hex,
            timestamp_ns: f.timestamp_ns,
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn parse_hex_id(s: &str, extended: bool) -> Result<CanId, McpError> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    let raw = u32::from_str_radix(s, 16)
        .map_err(|e| McpError::invalid_params(format!("bad id '{s}': {e}"), None))?;
    Ok(if extended {
        CanId::extended(raw)
    } else {
        CanId::standard(raw as u16)
    })
}

fn parse_single_hex_byte(s: &str) -> Result<u8, McpError> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u8::from_str_radix(s, 16)
        .map_err(|e| McpError::invalid_params(format!("bad hex byte '{s}': {e}"), None))
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, McpError> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    if !s.len().is_multiple_of(2) {
        return Err(McpError::invalid_params(
            "data hex string must have even length",
            None,
        ));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| {
                McpError::invalid_params(format!("bad hex byte at offset {i}: {e}"), None)
            })
        })
        .collect()
}

fn build_frame(args: &SendFrameInline) -> Result<Frame, McpError> {
    let id = parse_hex_id(&args.id, args.extended)?;
    let data = parse_hex_bytes(&args.data)?;
    let frame = if args.fd {
        Frame::new_fd(
            id,
            &data,
            FdFlags {
                brs: args.brs,
                esi: false,
            },
        )
    } else {
        Frame::new_classic(id, &data)
    };
    frame.map_err(|e| McpError::invalid_params(format!("invalid frame: {e}"), None))
}

fn build_filters(spec: &FilterSpec) -> Result<Vec<CanFilter>, McpError> {
    spec.entries
        .iter()
        .map(|e| {
            let id = u32::from_str_radix(e.id.trim_start_matches("0x"), 16)
                .map_err(|err| McpError::invalid_params(format!("bad filter id: {err}"), None))?;
            let mask = u32::from_str_radix(e.mask.trim_start_matches("0x"), 16)
                .map_err(|err| McpError::invalid_params(format!("bad filter mask: {err}"), None))?;
            Ok(if e.extended {
                CanFilter {
                    id: id | mcanbus::consts::CAN_EFF_FLAG,
                    mask: mask | mcanbus::consts::CAN_EFF_FLAG,
                }
            } else {
                CanFilter { id, mask }
            })
        })
        .collect()
}

fn iface_summary(iface: &Interface) -> serde_json::Value {
    json!({
        "name": iface.name,
        "up": iface.is_up().ok().flatten(),
        "state": iface.state().ok().flatten().map(|s| format!("{s:?}")),
        "bitrate": iface.bitrate().ok().flatten(),
    })
}

fn ok_json(value: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(value.to_string())])
}

fn run_capture(
    iface: &str,
    timeout_ms: u64,
    max_frames: usize,
    filter: Option<&FilterSpec>,
) -> Result<Vec<FrameJson>, McpError> {
    let max_frames = max_frames.min(4096);
    let filter = filter.map(build_filters).transpose()?;
    let opts = OpenOpts {
        recv_timeout: Some(Duration::from_millis(100)),
        filter,
        ..OpenOpts::default()
    };
    let sock = Socket::open(iface, &opts)
        .map_err(|e| McpError::internal_error(format!("open {iface}: {e}"), None))?;
    let mut out = Vec::with_capacity(max_frames.min(64));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while out.len() < max_frames && Instant::now() < deadline {
        match sock.recv() {
            Ok(Some(frame)) => out.push(FrameJson::from_frame(&frame)),
            Ok(None) => continue,
            Err(e) => {
                return Err(McpError::internal_error(format!("recv: {e}"), None));
            }
        }
    }
    Ok(out)
}

// ── Server ────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct CanServer {
    config: Config,
    tool_router: ToolRouter<CanServer>,
}

#[tool_router]
impl CanServer {
    fn new(config: Config) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List every CAN-class network interface visible to the kernel, with state and bitrate. Read-only."
    )]
    async fn list_interfaces(&self) -> Result<CallToolResult, McpError> {
        let ifaces = list_can_interfaces()
            .map_err(|e| McpError::internal_error(format!("enumerate: {e}"), None))?;
        let summaries: Vec<serde_json::Value> = ifaces.iter().map(iface_summary).collect();
        Ok(ok_json(json!({
            "interfaces": summaries,
            "allowlist": self.config.allowlist,
            "readonly": self.config.readonly,
        })))
    }

    #[tool(
        description = "Detailed status for a single CAN interface: up/down, controller state (error-active/passive/bus-off/...), bitrate. Read-only."
    )]
    async fn iface_state(
        &self,
        Parameters(args): Parameters<IfaceArg>,
    ) -> Result<CallToolResult, McpError> {
        // Read-only: no allowlist check. Information about a non-allowed
        // interface still has to come from somewhere, and `ip link show`
        // shows it anyway — we're not granting capability here.
        let iface = Interface::new(args.iface);
        Ok(ok_json(iface_summary(&iface)))
    }

    #[tool(
        description = "Send a single CAN frame. Requires the interface to be in the server allowlist. Refuses if SOCKETCAN_MCP_READONLY=1."
    )]
    async fn send_frame(
        &self,
        Parameters(args): Parameters<SendFrameArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.config.check_iface(&args.iface)?;
        self.config.check_writable()?;

        let inline = SendFrameInline {
            id: args.id,
            extended: args.extended,
            data: args.data,
            fd: args.fd,
            brs: args.brs,
        };
        let frame = build_frame(&inline)?;

        let sock = Socket::open(&args.iface, &OpenOpts::default())
            .map_err(|e| McpError::internal_error(format!("open {}: {e}", args.iface), None))?;
        sock.send(&frame)
            .map_err(|e| McpError::internal_error(format!("send: {e}"), None))?;

        Ok(ok_json(json!({
            "ok": true,
            "frame": FrameJson::from_frame(&frame),
        })))
    }

    #[tool(
        description = "Listen on a CAN interface for up to `timeout_ms` milliseconds (default 1000) and return up to `max_frames` (default 256, max 4096) matching frames. Optional filter narrows what counts. Read-only on the wire."
    )]
    async fn capture(
        &self,
        Parameters(args): Parameters<CaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.config.check_iface(&args.iface)?;
        // The blocking syscalls would otherwise pin the runtime worker.
        let frames = tokio::task::spawn_blocking(move || {
            run_capture(
                &args.iface,
                args.timeout_ms,
                args.max_frames,
                args.filter.as_ref(),
            )
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))??;

        Ok(ok_json(json!({
            "frames": frames,
            "count": frames.len(),
        })))
    }

    #[tool(
        description = "ISO 15765-2 (ISO-TP) request/response in one call. Sends a payload of any length up to 4095 bytes — the server segments into Single Frame or First Frame + Consecutive Frames automatically, handles Flow Control in both directions, and returns the reassembled response payload. Use this for UDS, KWP2000, and OBD-II reads larger than 7 bytes (e.g. VIN reads). Classic CAN only in v0.2; CAN-FD ISO-TP on the roadmap."
    )]
    async fn isotp_request(
        &self,
        Parameters(args): Parameters<IsoTpRequestArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.config.check_iface(&args.iface)?;
        self.config.check_writable()?;

        let tx_id = parse_hex_id(&args.tx_id, args.extended)?;
        let rx_id = parse_hex_id(&args.rx_id, args.extended)?;
        let payload = parse_hex_bytes(&args.payload)?;
        let padding = parse_single_hex_byte(&args.padding)?;

        let started = Instant::now();
        let result: Result<Vec<u8>, McpError> = tokio::task::spawn_blocking(move || {
            let opts = OpenOpts {
                recv_timeout: Some(Duration::from_millis(50)),
                ..OpenOpts::default()
            };
            let sock = Socket::open(&args.iface, &opts)
                .map_err(|e| McpError::internal_error(format!("open {}: {e}", args.iface), None))?;
            let isotp_opts = isotp::Options {
                padding,
                block_size: args.block_size,
                st_min_ms: args.st_min_ms,
                timeout: Duration::from_millis(args.timeout_ms),
                ..isotp::Options::default()
            };
            isotp::request(&sock, tx_id, rx_id, &payload, &isotp_opts)
                .map_err(|e| McpError::internal_error(format!("isotp: {e}"), None))
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))?;

        let response = result?;
        let response_hex: String = response.iter().map(|b| format!("{b:02X}")).collect();
        let response_ascii: String = response
            .iter()
            .map(|&b| {
                if (0x20..=0x7E).contains(&b) {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        Ok(ok_json(json!({
            "response": {
                "hex": response_hex,
                "ascii": response_ascii,
                "len": response.len(),
            },
            "duration_ms": started.elapsed().as_millis() as u64,
        })))
    }

    #[tool(
        description = "Send a frame and capture replies in one call. The capture window starts immediately after the send. Useful for request/response patterns (UDS, OBD-II) without orchestrating two tool calls."
    )]
    async fn send_and_capture(
        &self,
        Parameters(args): Parameters<SendAndCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.config.check_iface(&args.iface)?;
        self.config.check_writable()?;
        let rx_iface = args.rx_iface.clone().unwrap_or_else(|| args.iface.clone());
        if rx_iface != args.iface {
            self.config.check_iface(&rx_iface)?;
        }

        let frame = build_frame(&args.frame)?;

        let result = tokio::task::spawn_blocking(move || -> Result<_, McpError> {
            // Open RX *before* we send so we don't miss a fast reply.
            let filter = args.filter.as_ref().map(build_filters).transpose()?;
            let rx_opts = OpenOpts {
                recv_timeout: Some(Duration::from_millis(50)),
                filter,
                ..OpenOpts::default()
            };
            let rx = Socket::open(&rx_iface, &rx_opts)
                .map_err(|e| McpError::internal_error(format!("open rx {rx_iface}: {e}"), None))?;

            let tx = if rx_iface == args.iface {
                rx.try_clone()
                    .map_err(|e| McpError::internal_error(format!("clone tx: {e}"), None))?
            } else {
                Socket::open(&args.iface, &OpenOpts::default()).map_err(|e| {
                    McpError::internal_error(format!("open tx {}: {e}", args.iface), None)
                })?
            };

            tx.send(&frame)
                .map_err(|e| McpError::internal_error(format!("send: {e}"), None))?;

            let max_frames = args.max_frames.min(4096);
            let deadline = Instant::now() + Duration::from_millis(args.timeout_ms);
            let mut out: Vec<FrameJson> = Vec::with_capacity(16);
            while out.len() < max_frames && Instant::now() < deadline {
                match rx.recv() {
                    Ok(Some(f)) => out.push(FrameJson::from_frame(&f)),
                    Ok(None) => continue,
                    Err(e) => {
                        return Err(McpError::internal_error(format!("recv: {e}"), None));
                    }
                }
            }
            Ok((FrameJson::from_frame(&frame), out))
        })
        .await
        .map_err(|e| McpError::internal_error(format!("join: {e}"), None))??;

        let (sent, captured) = result;
        Ok(ok_json(json!({
            "sent": sent,
            "responses": captured,
            "count": captured.len(),
        })))
    }
}

#[tool_handler]
impl ServerHandler for CanServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "socketcan-mcp".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                title: Some("SocketCAN MCP server".into()),
                website_url: Some("https://github.com/mickeyl/mcanbus".into()),
                icons: None,
            },
            instructions: Some(
                "Linux SocketCAN access for AI agents. Use list_interfaces to discover \
                 available buses, capture to observe traffic, send_frame to transmit, \
                 send_and_capture for raw single-frame request/response, and \
                 isotp_request for ISO 15765-2 segmented transport (UDS, KWP2000, \
                 OBD-II reads larger than 7 bytes). All write operations are gated \
                 by SOCKETCAN_MCP_INTERFACES (allowlist) and SOCKETCAN_MCP_READONLY."
                    .into(),
            ),
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = Config::from_env();
    if config.allowlist.is_empty() {
        tracing::warn!(
            "SOCKETCAN_MCP_INTERFACES is empty — every send/capture call will fail with 'not in allowlist'"
        );
    } else {
        tracing::info!(
            "allowlist: {:?}, readonly: {}",
            config.allowlist,
            config.readonly
        );
    }

    let server = CanServer::new(config);
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve: {e:?}"))?;
    service.waiting().await?;
    Ok(())
}
