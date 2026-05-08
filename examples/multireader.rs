//! Demonstration of the multi-consumer fan-out [`Reader`].
//!
//! Spawns N worker threads, each owning a [`Subscriber`]. They all consume
//! frames concurrently with no coordination — the reader thread fans out
//! every frame to every subscriber. Run with high traffic to see zero-loss
//! semantics in action:
//!
//! ```sh
//! # Terminal 1: feed traffic.
//! cargo run --example cangen -- can0 --rate 5000 --batch 32
//!
//! # Terminal 2: 8 concurrent subscribers on can1.
//! cargo run --example multireader -- can1 --workers 8
//! ```

use std::env;
use std::io;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mcanbus::reader::Reader;
use mcanbus::{OpenOpts, Socket};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn signal_handler(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_sigint() {
    // SAFETY: standard signal-handler registration; only stores an atomic.
    unsafe {
        let h = signal_handler as *const () as libc::sighandler_t;
        libc::signal(libc::SIGINT, h);
        libc::signal(libc::SIGTERM, h);
    }
}

fn usage() -> ! {
    eprintln!("usage: multireader <iface> [--workers N]");
    std::process::exit(2);
}

fn run() -> io::Result<()> {
    let mut it = env::args().skip(1);
    let iface = it.next().unwrap_or_else(|| usage());
    let mut workers = 4usize;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--workers" => {
                workers = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }

    install_sigint();

    let socket = Socket::open(
        &iface,
        &OpenOpts {
            recv_timeout: Some(Duration::from_millis(200)),
            ..OpenOpts::default()
        },
    )?;
    let reader = Reader::new(socket);
    eprintln!("multireader: {workers} subscribers on {iface}");

    let start = Instant::now();
    let handles: Vec<_> = (0..workers)
        .map(|i| {
            let sub = reader.subscribe();
            thread::Builder::new()
                .name(format!("worker-{i}"))
                .spawn(move || {
                    while !STOP.load(Ordering::SeqCst) {
                        if let Some(frame) = sub.recv_timeout(Duration::from_millis(200)) {
                            // Pretend to do work proportional to the worker
                            // index so we can observe slow-consumer behaviour.
                            std::hint::black_box(frame);
                        }
                    }
                    (i, sub.received(), sub.dropped(), sub.pending())
                })
                .expect("spawn worker")
        })
        .collect();

    // Periodic progress to stderr so the user sees activity.
    while !STOP.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
    }

    eprintln!("\nshutting down…");
    let mut total_received = 0u64;
    let mut total_dropped = 0u64;
    for h in handles {
        let (i, received, dropped, pending) = h.join().unwrap();
        eprintln!("  worker {i}: received={received} dropped={dropped} pending_at_exit={pending}");
        total_received += received;
        total_dropped += dropped;
    }
    let elapsed = start.elapsed().as_secs_f64();
    eprintln!(
        "total across all workers: received={total_received} dropped={total_dropped} ({:.1}s)",
        elapsed
    );

    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("multireader: {e}");
            ExitCode::FAILURE
        }
    }
}
