//! Minimal candump replacement.
//!
//! Reads frames from one interface and prints them in candump-style format
//! until SIGINT.
//!
//! ```sh
//! cargo run --example candump -- can0
//! cargo run --example candump -- can0 --filter 7F0:7F0
//! ```

use std::env;
use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mcanbus::{CanFilter, OpenOpts, Socket};

static STOP: AtomicBool = AtomicBool::new(false);

fn usage() -> ! {
    eprintln!("usage: candump <iface> [--filter ID:MASK[,ID:MASK,...]]");
    std::process::exit(2);
}

fn parse_filters(spec: &str) -> Result<Vec<CanFilter>, String> {
    spec.split(',')
        .map(|entry| {
            let (id, mask) = entry
                .split_once(':')
                .ok_or_else(|| format!("filter '{entry}': expected ID:MASK"))?;
            let id = u32::from_str_radix(id.trim_start_matches("0x"), 16)
                .map_err(|e| format!("filter '{entry}': bad id: {e}"))?;
            let mask = u32::from_str_radix(mask.trim_start_matches("0x"), 16)
                .map_err(|e| format!("filter '{entry}': bad mask: {e}"))?;
            Ok(CanFilter { id, mask })
        })
        .collect()
}

extern "C" fn signal_handler(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_sigint() {
    // SAFETY: registering a `extern "C"` function pointer as a signal handler
    // is the standard contract; the handler only touches an atomic flag.
    unsafe {
        let h = signal_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

fn format_timestamp(ns: u64) -> String {
    let dur = Duration::from_nanos(ns);
    let secs = dur.as_secs();
    let micros = dur.subsec_micros();
    // Cheap "wall clock" formatting without chrono — HH:MM:SS.uuuuuu in UTC.
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}.{micros:06}")
}

fn run() -> io::Result<()> {
    let mut args = env::args().skip(1);
    let iface = args.next().unwrap_or_else(|| usage());
    let mut filter = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--filter" => {
                let spec = args.next().unwrap_or_else(|| usage());
                filter = Some(parse_filters(&spec).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(2);
                }));
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }

    install_sigint();

    let opts = OpenOpts {
        filter,
        recv_timeout: Some(Duration::from_millis(500)),
        ..OpenOpts::default()
    };
    let sock = Socket::open(&iface, &opts)?;
    eprintln!(
        "candump: bound to {iface} (FD-RX {})",
        if sock.fd_rx_enabled() { "on" } else { "off" }
    );

    let mut count = 0u64;
    while !STOP.load(Ordering::SeqCst) {
        let Some(frame) = sock.recv()? else {
            continue;
        };
        let ts = frame
            .timestamp_ns
            .map(format_timestamp)
            .unwrap_or_else(|| "??:??:??.??????".into());
        println!("({ts}) {iface}  {frame}");
        count += 1;
    }

    eprintln!("\ncandump: stopped after {count} frames");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("candump: {e}");
            ExitCode::FAILURE
        }
    }
}

