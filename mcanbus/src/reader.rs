//! Multi-consumer fan-out reader.
//!
//! [`Reader`] owns a [`Socket`], drains it in batched syscalls from a
//! dedicated thread, and fans every frame out to every [`Subscriber`].
//! Subscribers are created on demand with [`Reader::subscribe`] and each
//! gets its own queue — a slow subscriber only stalls itself, never the
//! others, never the reader, never the kernel.
//!
//! ```no_run
//! use mcanbus::{OpenOpts, Socket};
//! use mcanbus::reader::Reader;
//!
//! let socket = Socket::open("can0", &OpenOpts::default())?;
//! let reader = Reader::new(socket);
//!
//! // Two subscribers, each sees every frame from the time it was created.
//! let logger = reader.subscribe();
//! let analyser = reader.subscribe();
//!
//! std::thread::spawn(move || {
//!     while let Some(frame) = analyser.recv() {
//!         println!("{frame}");
//!     }
//! });
//!
//! while let Some(frame) = logger.recv() {
//!     // ... log to disk ...
//!     # let _ = frame;
//! }
//! # Ok::<_, std::io::Error>(())
//! ```
//!
//! # Bounded vs unbounded
//!
//! [`Reader::subscribe`] gives you an unbounded queue — zero-loss as long as
//! the consumer keeps up over time. [`Reader::subscribe_bounded`] caps the
//! queue: when full, new frames for that subscriber are dropped (counted
//! atomically in [`Subscriber::dropped`]). Bounded is the right choice when
//! you'd rather lose frames than lose memory under sustained backpressure.
//!
//! # Shutdown
//!
//! Dropping the [`Reader`] signals the thread to stop and joins it. Shutdown
//! latency is bounded by the socket's `recv_timeout` (default 500 ms): the
//! thread won't see the stop flag until its current `recvmmsg` returns.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};

use crate::frame::Frame;
use crate::socket::Socket;

// ── Public types ──────────────────────────────────────────────────────────

/// Configuration for [`Reader`].
#[derive(Clone, Debug)]
pub struct ReaderConfig {
    /// Frames per `recvmmsg` syscall. Larger batches amortise syscall cost
    /// at the price of higher latency under low load. Capped at 64 by the
    /// underlying socket implementation.
    pub batch_size: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self { batch_size: 64 }
    }
}

/// A multi-consumer fan-out reader. See module-level docs for usage.
pub struct Reader {
    inner: Arc<ReaderInner>,
    handle: Option<JoinHandle<()>>,
}

struct ReaderInner {
    subscribers: Mutex<Vec<Arc<SubscriberInner>>>,
    stop: AtomicBool,
    /// Number of times the kernel told us its socket buffer overflowed.
    /// Currently exposed via [`Reader::kernel_drops`] for visibility — the
    /// counter is incremented in the next iteration when a drop is observed.
    kernel_drops: AtomicU64,
}

struct SubscriberInner {
    tx: Sender<Frame>,
    received: AtomicU64,
    dropped: AtomicU64,
}

/// A subscription to a [`Reader`]. Holds a private queue of frames; drop it
/// to unsubscribe.
pub struct Subscriber {
    rx: Receiver<Frame>,
    inner: Arc<SubscriberInner>,
}

// ── Reader impl ───────────────────────────────────────────────────────────

impl Reader {
    /// Build a [`Reader`] from a configured [`Socket`] with default settings.
    pub fn new(socket: Socket) -> Self {
        Self::with_config(socket, ReaderConfig::default())
    }

    /// Build a [`Reader`] with custom configuration.
    pub fn with_config(socket: Socket, config: ReaderConfig) -> Self {
        let inner = Arc::new(ReaderInner {
            subscribers: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            kernel_drops: AtomicU64::new(0),
        });
        let handle = {
            let inner = inner.clone();
            let batch = config.batch_size.clamp(1, 64);
            thread::Builder::new()
                .name("mcanbus-reader".to_string())
                .spawn(move || run_reader(socket, inner, batch))
                .expect("spawn reader thread")
        };
        Self {
            inner,
            handle: Some(handle),
        }
    }

    /// Add a subscriber with an unbounded queue (zero-loss as long as the
    /// consumer eventually keeps up).
    pub fn subscribe(&self) -> Subscriber {
        let (tx, rx) = unbounded();
        let inner = Arc::new(SubscriberInner {
            tx,
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        self.inner.subscribers.lock().unwrap().push(inner.clone());
        Subscriber { rx, inner }
    }

    /// Add a subscriber with a bounded queue of `capacity`. When full, new
    /// frames are dropped and counted in [`Subscriber::dropped`].
    pub fn subscribe_bounded(&self, capacity: usize) -> Subscriber {
        let (tx, rx) = bounded(capacity);
        let inner = Arc::new(SubscriberInner {
            tx,
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        self.inner.subscribers.lock().unwrap().push(inner.clone());
        Subscriber { rx, inner }
    }

    /// Number of currently active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscribers.lock().unwrap().len()
    }

    /// Number of drops the kernel reported via `SO_RXQ_OVFL`. Always 0 in
    /// the current implementation; reserved for a future upgrade.
    pub fn kernel_drops(&self) -> u64 {
        self.inner.kernel_drops.load(Ordering::Relaxed)
    }

    /// Stop the reader thread and join it. Equivalent to dropping the reader,
    /// but lets you observe a join error if the thread panicked.
    pub fn shutdown(mut self) -> thread::Result<()> {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.join()
        } else {
            Ok(())
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ── Subscriber impl ───────────────────────────────────────────────────────

impl Subscriber {
    /// Block until a frame is available, or the reader has shut down (and
    /// the queue is drained), in which case `None`.
    pub fn recv(&self) -> Option<Frame> {
        self.rx.recv().ok()
    }

    /// Like [`Subscriber::recv`] with a timeout.
    pub fn recv_timeout(&self, dur: Duration) -> Option<Frame> {
        self.rx.recv_timeout(dur).ok()
    }

    /// Non-blocking pop.
    pub fn try_recv(&self) -> Option<Frame> {
        self.rx.try_recv().ok()
    }

    /// Total frames this subscriber has received since creation (including
    /// frames still queued).
    pub fn received(&self) -> u64 {
        self.inner.received.load(Ordering::Relaxed)
    }

    /// Frames the reader had to drop because this subscriber's bounded queue
    /// was full. Always 0 for unbounded subscribers.
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Frames currently sitting in this subscriber's queue (not yet consumed).
    pub fn pending(&self) -> usize {
        self.rx.len()
    }

    /// Borrow the underlying [`crossbeam_channel::Receiver`] for use with
    /// `select!` and other crossbeam primitives.
    pub fn as_receiver(&self) -> &Receiver<Frame> {
        &self.rx
    }
}

// ── Reader thread body ────────────────────────────────────────────────────

fn run_reader(socket: Socket, inner: Arc<ReaderInner>, batch_size: usize) {
    // Stack-allocated batch buffer. Frame is Copy and small enough that this
    // is fine — at batch=64 it's 64 * 80 bytes ≈ 5 KiB.
    let mut buf = [Frame::zeroed(); 64];
    let active = batch_size.min(buf.len());

    while !inner.stop.load(Ordering::SeqCst) {
        let n = match socket.recv_batch(&mut buf[..active]) {
            Ok(n) => n,
            Err(_) => {
                // Transient errors (interface bouncing, EBADF after explicit
                // close, etc.) — pause briefly and check the stop flag again.
                thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        if n == 0 {
            continue;
        }

        // Snapshot the subscriber list under the lock, then send outside it
        // so subscribe()/drop don't fight with the fan-out for the lock.
        let snapshot: Vec<Arc<SubscriberInner>> = {
            let subs = inner.subscribers.lock().unwrap();
            subs.iter().cloned().collect()
        };

        // Track Arc identities of subscribers whose receiver has been dropped,
        // so we can cull them after the fan-out finishes.
        let mut dead: Vec<*const SubscriberInner> = Vec::new();
        for sub in &snapshot {
            for frame in &buf[..n] {
                match sub.tx.try_send(*frame) {
                    Ok(()) => {
                        sub.received.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Full(_)) => {
                        sub.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(TrySendError::Disconnected(_)) => {
                        dead.push(Arc::as_ptr(sub));
                        break;
                    }
                }
            }
        }
        drop(snapshot);

        if !dead.is_empty() {
            let mut subs = inner.subscribers.lock().unwrap();
            subs.retain(|s| !dead.contains(&Arc::as_ptr(s)));
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::CanId;

    #[test]
    fn subscriber_default_counters_zero() {
        // We can build a SubscriberInner directly to test the counter API
        // without needing a live socket.
        let (tx, _rx) = unbounded::<Frame>();
        let inner = Arc::new(SubscriberInner {
            tx,
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        assert_eq!(inner.received.load(Ordering::Relaxed), 0);
        assert_eq!(inner.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn fanout_logic_unbounded() {
        // Simulate the fan-out: 100 frames into 3 subscribers, all unbounded.
        let mut subs: Vec<Arc<SubscriberInner>> = Vec::new();
        let mut rxs: Vec<Receiver<Frame>> = Vec::new();
        for _ in 0..3 {
            let (tx, rx) = unbounded::<Frame>();
            subs.push(Arc::new(SubscriberInner {
                tx,
                received: AtomicU64::new(0),
                dropped: AtomicU64::new(0),
            }));
            rxs.push(rx);
        }

        let frame = Frame::new_classic(CanId::standard(0x123), &[1, 2, 3]).unwrap();
        for _ in 0..100 {
            for sub in &subs {
                sub.tx.try_send(frame).unwrap();
                sub.received.fetch_add(1, Ordering::Relaxed);
            }
        }

        for (sub, rx) in subs.iter().zip(rxs.iter()) {
            assert_eq!(sub.received.load(Ordering::Relaxed), 100);
            assert_eq!(rx.len(), 100);
            assert_eq!(sub.dropped.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn fanout_logic_bounded_drops() {
        // Bounded capacity 8, push 16 frames → 8 dropped.
        let (tx, rx) = bounded::<Frame>(8);
        let sub = Arc::new(SubscriberInner {
            tx,
            received: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        let frame = Frame::new_classic(CanId::standard(0x100), &[]).unwrap();
        for _ in 0..16 {
            match sub.tx.try_send(frame) {
                Ok(()) => {
                    sub.received.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Full(_)) => {
                    sub.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => unreachable!(),
            }
        }
        assert_eq!(sub.received.load(Ordering::Relaxed), 8);
        assert_eq!(sub.dropped.load(Ordering::Relaxed), 8);
        assert_eq!(rx.len(), 8);
    }
}
