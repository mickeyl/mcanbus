//! Minimal cangen — generate frames at a target rate.
//!
//! ```sh
//! cargo run --example cangen -- can0                    # 100 fps, default
//! cargo run --example cangen -- can0 --rate 1000        # 1000 fps
//! cargo run --example cangen -- can0 --rate 5000 --batch 32  # batched send
//! cargo run --example cangen -- can0 --id 0x123 --data DEADBEEF
//! ```

use std::env;
use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use mcanbus::{CanId, Frame, OpenOpts, Socket};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_sigint() {
    // SAFETY: standard signal-handler registration; the handler only stores
    // an atomic bool, which is async-signal-safe.
    unsafe {
        let h = signal_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

struct Args {
    iface: String,
    rate: u64,
    batch: usize,
    id: Option<u32>,
    data: Vec<u8>,
}

fn usage() -> ! {
    eprintln!(
        "usage: cangen <iface> [--rate FPS] [--batch N] [--id 0xNNN] [--data HEX]\n\
         \n\
         Defaults: rate=100, batch=1, id=cycling 0x100..0x110, data=8 random-looking bytes."
    );
    std::process::exit(2);
}

fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let s = s.trim_start_matches("0x");
    if !s.len().is_multiple_of(2) {
        return Err("hex must have even length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

fn parse_args() -> Args {
    let mut it = env::args().skip(1);
    let iface = it.next().unwrap_or_else(|| usage());
    let mut rate: u64 = 100;
    let mut batch: usize = 1;
    let mut id = None;
    let mut data = vec![0xDEu8, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rate" => {
                rate = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--batch" => {
                batch = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--id" => {
                let s = it.next().unwrap_or_else(|| usage());
                let s = s.trim_start_matches("0x");
                id = Some(u32::from_str_radix(s, 16).unwrap_or_else(|_| usage()));
            }
            "--data" => {
                data = parse_hex(&it.next().unwrap_or_else(|| usage())).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    std::process::exit(2);
                });
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    if batch == 0 || rate == 0 {
        usage();
    }
    Args {
        iface,
        rate,
        batch,
        id,
        data,
    }
}

fn build_frame(args: &Args, seq: u64) -> Frame {
    let id = match args.id {
        Some(raw) => {
            if raw <= 0x7FF {
                CanId::standard(raw as u16)
            } else {
                CanId::extended(raw)
            }
        }
        None => CanId::standard(0x100 + (seq as u16 & 0x0F)),
    };
    Frame::new_classic(id, &args.data).expect("payload <= 8 bytes")
}

fn run() -> io::Result<()> {
    let args = parse_args();
    install_sigint();

    let opts = OpenOpts {
        // We're TX-only here; a smaller RX buffer is fine.
        recv_buf_bytes: Some(64 * 1024),
        ..OpenOpts::default()
    };
    let sock = Socket::open(&args.iface, &opts)?;
    eprintln!(
        "cangen: bound to {} (rate={} fps, batch={})",
        args.iface, args.rate, args.batch
    );

    // Per-batch frame buffer, reused across iterations.
    let mut batch_buf: Vec<Frame> = Vec::with_capacity(args.batch);

    let burst_period = Duration::from_nanos(1_000_000_000 * args.batch as u64 / args.rate);
    let start = Instant::now();
    let mut next_deadline = start;
    let mut sent: u64 = 0;
    let mut seq: u64 = 0;

    while !STOP.load(Ordering::SeqCst) {
        batch_buf.clear();
        for _ in 0..args.batch {
            batch_buf.push(build_frame(&args, seq));
            seq += 1;
        }

        let n = if args.batch == 1 {
            sock.send(&batch_buf[0])?;
            1
        } else {
            sock.send_batch(&batch_buf)?
        };
        sent += n as u64;

        next_deadline += burst_period;
        let now = Instant::now();
        if next_deadline > now {
            std::thread::sleep(next_deadline - now);
        } else if (now - next_deadline) > Duration::from_millis(500) {
            // We've fallen too far behind — re-anchor to avoid runaway catch-up.
            next_deadline = now;
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "\ncangen: stopped — {sent} frames in {elapsed:.2}s ({:.0} fps)",
        sent as f64 / elapsed.max(1e-9)
    );
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cangen: {e}");
            ExitCode::FAILURE
        }
    }
}
