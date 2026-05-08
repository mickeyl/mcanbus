//! Integration tests against real SocketCAN interfaces.
//!
//! Set `MCANBUS_TEST_IFACE=<iface>` to opt in. Without it every test in
//! this file silently passes, so plain `cargo test` doesn't fail on hosts
//! that lack a CAN setup.
//!
//! Optional environment variables:
//!
//! | Variable                     | Meaning                                                                  |
//! | ---------------------------- | ------------------------------------------------------------------------ |
//! | `MCANBUS_TEST_IFACE`         | TX interface (required to enable tests).                                 |
//! | `MCANBUS_TEST_IFACE_RX`      | RX interface (default: same as TX). Useful when TX/RX share a wire.      |
//! | `MCANBUS_TEST_FD`            | Set to `1` to run the FD round-trip test (controller must support FD-TX). |
//!
//! Each test uses a **per-test ID** drawn from a private range
//! (`0x7F0..=0x7FF`) so different tests don't see each other's frames, and
//! drains its RX socket before sending to discard any in-flight traffic
//! from the previous test or other bus participants.

use std::time::{Duration, Instant};

use mcanbus::reader::Reader;
use mcanbus::{CanFilter, CanId, FdFlags, Frame, OpenOpts, Socket, Timestamping};

const TEST_SFF_BASE: u16 = 0x7F0;
const TEST_EFF_BASE: u32 = 0x1FFF_FFE0;

// Per-test SFF IDs. Keep these unique within the file so a stray frame from
// test A can't satisfy a recv in test B.
const ID_CLASSIC_RT: u16 = TEST_SFF_BASE | 0x1;
const ID_FD_RT: u16 = TEST_SFF_BASE | 0x3;
const ID_BATCH_BASE: u16 = TEST_SFF_BASE | 0x4; // uses 0x4..=0xB (8 frames)
const ID_TRY_CLONE: u16 = TEST_SFF_BASE | 0xD;
const ID_EFF_RT: u32 = TEST_EFF_BASE | 0x5;

fn tx_iface() -> Option<String> {
    std::env::var("MCANBUS_TEST_IFACE").ok()
}

fn rx_iface() -> Option<String> {
    std::env::var("MCANBUS_TEST_IFACE_RX")
        .ok()
        .or_else(tx_iface)
}

/// Filter accepting only the given standard ID.
fn filter_only_sff(id: u16) -> Vec<CanFilter> {
    vec![CanFilter::standard_exact(id)]
}

/// Filter accepting only the given extended ID.
fn filter_only_eff(id: u32) -> Vec<CanFilter> {
    vec![CanFilter::extended_exact(id)]
}

fn opts_with(filter: Vec<CanFilter>, timeout_ms: u64) -> OpenOpts {
    OpenOpts {
        recv_timeout: Some(Duration::from_millis(timeout_ms)),
        filter: Some(filter),
        ..OpenOpts::default()
    }
}

fn tx_opts() -> OpenOpts {
    OpenOpts {
        recv_timeout: Some(Duration::from_millis(500)),
        ..OpenOpts::default()
    }
}

/// Drain whatever frames are currently waiting on `sock` for `dur`.
/// Done by switching to a tight non-blocking loop until it sees no frame.
fn drain(sock: &Socket, dur: Duration) {
    let until = Instant::now() + dur;
    while Instant::now() < until {
        // The socket already has a (short) recv_timeout, so this loop won't
        // run hot; each empty recv yields after the socket's RCVTIMEO.
        match sock.recv() {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
}

#[test]
fn open_close_cycle() {
    let Some(iface) = tx_iface() else {
        return;
    };
    for _ in 0..16 {
        let s = Socket::open(&iface, &tx_opts()).expect("open");
        drop(s);
    }
}

#[test]
fn classic_round_trip() {
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();
    let id = CanId::standard(ID_CLASSIC_RT);

    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    let rx = Socket::open(
        &rx_name,
        &opts_with(filter_only_sff(ID_CLASSIC_RT), 500),
    )
    .expect("open rx");
    drain(&rx, Duration::from_millis(50));

    let payload = [0xDE, 0xAD, 0xBE, 0xEF];
    let frame = Frame::new_classic(id, &payload).unwrap();
    tx.send(&frame).expect("send");

    let received = rx.recv().expect("recv").expect("got a frame");
    assert_eq!(received.id, id);
    assert_eq!(received.data(), &payload);
    assert!(received.timestamp_ns.is_some(), "expected a timestamp");
}

#[test]
fn extended_round_trip() {
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();
    let id = CanId::extended(ID_EFF_RT);

    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    let rx = Socket::open(&rx_name, &opts_with(filter_only_eff(ID_EFF_RT), 500)).expect("open rx");
    drain(&rx, Duration::from_millis(50));

    let payload = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let frame = Frame::new_classic(id, &payload).unwrap();
    tx.send(&frame).expect("send");

    let received = rx.recv().expect("recv").expect("got a frame");
    assert_eq!(received.id, id);
    assert_eq!(received.data(), &payload);
}

#[test]
fn fd_round_trip() {
    if std::env::var("MCANBUS_TEST_FD").as_deref() != Ok("1") {
        return;
    }
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();
    let id = CanId::standard(ID_FD_RT);

    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    let rx = Socket::open(&rx_name, &opts_with(filter_only_sff(ID_FD_RT), 500)).expect("open rx");
    drain(&rx, Duration::from_millis(50));

    if !tx.fd_rx_enabled() {
        eprintln!("CAN_RAW_FD_FRAMES not enabled on {tx_name}, skipping fd test");
        return;
    }

    let payload: Vec<u8> = (0..32).collect();
    let frame = Frame::new_fd(id, &payload, FdFlags { brs: true, esi: false }).unwrap();
    match tx.send(&frame) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            eprintln!("controller refuses FD TX (EINVAL) — skipping");
            return;
        }
        Err(e) => panic!("unexpected FD send error: {e}"),
    }

    let received = rx.recv().expect("recv").expect("got a frame");
    assert_eq!(received.id, id);
    assert!(received.is_fd());
    assert_eq!(received.data(), &payload[..]);
}

#[test]
fn batch_send_and_batch_recv() {
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();

    // Use 8 IDs starting at ID_BATCH_BASE.
    const N: usize = 8;
    // Build a filter allowing exactly the 8 batch IDs.
    let filter: Vec<CanFilter> = (0..N as u16)
        .map(|i| CanFilter::standard_exact(ID_BATCH_BASE + i))
        .collect();

    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    let rx = Socket::open(&rx_name, &opts_with(filter, 500)).expect("open rx");
    drain(&rx, Duration::from_millis(50));

    let frames: Vec<Frame> = (0..N as u16)
        .map(|i| {
            Frame::new_classic(
                CanId::standard(ID_BATCH_BASE + i),
                &[i as u8, i as u8, i as u8, i as u8],
            )
            .unwrap()
        })
        .collect();

    let sent = tx.send_batch(&frames).expect("send_batch");
    assert_eq!(sent, N);

    let mut buf = [Frame::zeroed(); 64];
    let mut total = 0;
    let deadline = Instant::now() + Duration::from_secs(2);
    while total < N && Instant::now() < deadline {
        let n = rx.recv_batch(&mut buf[..]).expect("recv_batch");
        total += n;
    }
    assert!(total >= N, "received {total}/{N}");
}

#[test]
fn timeout_returns_none() {
    let Some(name) = rx_iface() else {
        return;
    };
    // Use a filter with a single ID nobody is sending. 0x7FF unused by other tests.
    const NOBODY_ID: u16 = TEST_SFF_BASE | 0xF;
    let opts = OpenOpts {
        recv_timeout: Some(Duration::from_millis(100)),
        timestamping: Timestamping::None,
        filter: Some(filter_only_sff(NOBODY_ID)),
        ..OpenOpts::default()
    };
    let rx = Socket::open(&name, &opts).expect("open");
    let r = rx.recv().expect("recv");
    assert!(r.is_none(), "expected timeout to yield None, got {r:?}");
}

#[test]
fn try_clone_works() {
    let Some(name) = tx_iface() else {
        return;
    };
    let opts = opts_with(filter_only_sff(ID_TRY_CLONE), 500);
    let s1 = Socket::open(&name, &opts).expect("open");
    drain(&s1, Duration::from_millis(50));
    let s2 = s1.try_clone().expect("dup");

    let frame =
        Frame::new_classic(CanId::standard(ID_TRY_CLONE), &[1, 2, 3]).unwrap();
    s1.send(&frame).expect("send via s1");
    let _ = s2.recv().expect("recv via s2 ok");
}

#[test]
fn reader_fans_out_to_multiple_subscribers() {
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();

    // Reader-side: 4 IDs in our range.
    const N_FRAMES: usize = 200;
    const BASE_ID: u16 = TEST_SFF_BASE | 0x2;
    let filters: Vec<CanFilter> = (0..4u16)
        .map(|i| CanFilter::standard_exact(BASE_ID + i))
        .collect();

    let rx_socket = Socket::open(
        &rx_name,
        &OpenOpts {
            filter: Some(filters),
            recv_timeout: Some(Duration::from_millis(200)),
            ..OpenOpts::default()
        },
    )
    .expect("open rx");

    let reader = Reader::new(rx_socket);

    // Drain whatever is already on the bus before subscribers join.
    std::thread::sleep(Duration::from_millis(50));

    let sub_unbounded = reader.subscribe();
    let sub_bounded = reader.subscribe_bounded(N_FRAMES * 2); // big enough → no drops
    let sub_tiny = reader.subscribe_bounded(8); // small → expect drops

    assert_eq!(reader.subscriber_count(), 3);

    // TX side.
    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    for i in 0..N_FRAMES {
        let id = CanId::standard(BASE_ID + (i as u16 % 4));
        let frame = Frame::new_classic(id, &[(i & 0xFF) as u8; 4]).unwrap();
        tx.send(&frame).expect("send");
    }

    // Give the reader thread a moment to drain everything.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && (sub_unbounded.received() < N_FRAMES as u64
            || sub_bounded.received() + sub_bounded.dropped() < N_FRAMES as u64
            || sub_tiny.received() + sub_tiny.dropped() < N_FRAMES as u64)
    {
        std::thread::sleep(Duration::from_millis(20));
    }

    let unbounded_total = sub_unbounded.received();
    let bounded_total = sub_bounded.received() + sub_bounded.dropped();
    let tiny_total = sub_tiny.received() + sub_tiny.dropped();

    // All three subscribers must observe the same number of *attempts* — the
    // reader fan-outs every frame to every subscriber.
    assert_eq!(
        unbounded_total, N_FRAMES as u64,
        "unbounded subscriber missed frames"
    );
    assert_eq!(
        bounded_total, N_FRAMES as u64,
        "bounded subscriber attempts {bounded_total} != {N_FRAMES}"
    );
    assert_eq!(
        tiny_total, N_FRAMES as u64,
        "tiny subscriber attempts {tiny_total} != {N_FRAMES}"
    );

    // The unbounded queue must report zero drops.
    assert_eq!(sub_unbounded.dropped(), 0);
    // The big-bounded queue should also have zero drops (we sized it big).
    assert_eq!(sub_bounded.dropped(), 0);
    // We can't *require* the tiny queue to have drops on a slow bus where the
    // consumer keeps up, but we can require capacity ≥ tiny.received() at any
    // point in time — i.e. once we drain we should see ≤ 8 pending.
    assert!(sub_tiny.pending() <= 8);

    // Now drain the unbounded subscriber and verify content arrived.
    let mut consumed = 0u64;
    while sub_unbounded.try_recv().is_some() {
        consumed += 1;
    }
    assert_eq!(consumed, N_FRAMES as u64);
}

#[test]
fn reader_culls_dropped_subscriber() {
    let Some(tx_name) = tx_iface() else {
        return;
    };
    let rx_name = rx_iface().unwrap();

    const ID: u16 = TEST_SFF_BASE | 0xC;
    let rx_socket = Socket::open(
        &rx_name,
        &OpenOpts {
            filter: Some(filter_only_sff(ID)),
            recv_timeout: Some(Duration::from_millis(200)),
            ..OpenOpts::default()
        },
    )
    .expect("open rx");

    let reader = Reader::new(rx_socket);
    let _keeper = reader.subscribe(); // stays alive
    {
        let _short_lived = reader.subscribe();
        // _short_lived is dropped here.
    }
    assert_eq!(reader.subscriber_count(), 2); // not yet pruned

    // Send some frames so the reader's fan-out runs and notices the dead sub.
    let tx = Socket::open(&tx_name, &tx_opts()).expect("open tx");
    let frame = Frame::new_classic(CanId::standard(ID), &[1, 2, 3]).unwrap();
    for _ in 0..5 {
        tx.send(&frame).expect("send");
    }

    // Wait for the reader thread to observe and cull.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && reader.subscriber_count() > 1 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(reader.subscriber_count(), 1, "dead subscriber not culled");
}

#[test]
fn drops_traffic_outside_filter() {
    // Sanity check the filter machinery: with a filter for an ID nobody
    // sends, every recv must be a timeout, even when the bus has traffic.
    let Some(name) = rx_iface() else {
        return;
    };
    const NOBODY_ID: u16 = TEST_SFF_BASE | 0xE;
    let opts = OpenOpts {
        recv_timeout: Some(Duration::from_millis(50)),
        filter: Some(filter_only_sff(NOBODY_ID)),
        ..OpenOpts::default()
    };
    let rx = Socket::open(&name, &opts).expect("open");
    // Try a few times in case of any startup quirk.
    for _ in 0..3 {
        if rx.recv().expect("recv").is_some() {
            panic!("filter should not deliver any frames here");
        }
    }
}
