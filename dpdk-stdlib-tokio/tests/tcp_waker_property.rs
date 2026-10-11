//! Property test: AtomicWaker signaling under register-first-then-recheck.
//!
//! Validates Property 20 from the design doc: when the engine delivers data
//! to rx_ring AND a read_waker is registered, the waker is called. No data
//! can arrive between register and recheck without a wake.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use dpdk_stdlib_tcp::contract::{
    AtomicWaker, CommandSender, ConnectionHandle, EngineWakeup,
};
use dpdk_stdlib_tcp::state::FourTuple;

/// Create a test waker that sets an atomic flag when woken.
fn make_test_waker(flag: &'static AtomicBool) -> Waker {
    fn clone_fn(ptr: *const ()) -> RawWaker {
        RawWaker::new(ptr, &VTABLE)
    }
    fn wake_fn(ptr: *const ()) {
        let flag = unsafe { &*(ptr as *const AtomicBool) };
        flag.store(true, Ordering::Release);
    }
    fn wake_by_ref_fn(ptr: *const ()) {
        let flag = unsafe { &*(ptr as *const AtomicBool) };
        flag.store(true, Ordering::Release);
    }
    fn drop_fn(_: *const ()) {}

    static VTABLE: RawWakerVTable =
        RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

    let raw = RawWaker::new(flag as *const AtomicBool as *const (), &VTABLE);
    unsafe { Waker::from_raw(raw) }
}

/// Create a test ConnectionHandle with given ring capacities.
fn make_handle(rx_cap: usize, tx_cap: usize) -> Arc<ConnectionHandle> {
    let (tx, _rx) = mpsc::channel();
    let wakeup = Arc::new(EngineWakeup::new());
    let cmd_tx = CommandSender::new(tx, wakeup);
    let key = FourTuple {
        local: "10.0.0.1:1234".parse().unwrap(),
        remote: "10.0.0.2:80".parse().unwrap(),
    };
    Arc::new(ConnectionHandle::new(rx_cap, tx_cap, cmd_tx, key))
}

/// Property 20: AtomicWaker signaling with register-first-then-recheck.
///
/// When engine delivers data to rx_ring AND a read_waker is registered,
/// the waker is called.
#[test]
fn waker_called_when_data_arrives_after_register() {
    static WOKEN: AtomicBool = AtomicBool::new(false);

    let handle = make_handle(1024, 1024);
    let waker = make_test_waker(&WOKEN);

    // 1. Register waker (simulating poll_read registering before checking ring)
    handle.read_waker.register(&waker);
    WOKEN.store(false, Ordering::Release);

    // 2. Engine delivers data to rx_ring
    let data = b"hello";
    handle.rx_ring.write(data);

    // 3. Engine calls notify_all (as it does after rx_ring push)
    handle.notify_all();

    // 4. Waker must have been called
    assert!(
        WOKEN.load(Ordering::Acquire),
        "waker must be called when data arrives after registration"
    );
}

/// No false wake: if waker is NOT registered, notify_all doesn't panic.
#[test]
fn notify_without_registered_waker_is_safe() {
    let handle = make_handle(1024, 1024);
    handle.rx_ring.write(b"data");
    handle.notify_all(); // Must not panic
}

/// Register-first-then-recheck pattern: even if data arrives between register
/// and recheck, the waker was registered so it will be woken.
#[test]
fn register_first_then_recheck_no_lost_wake() {
    static WOKEN: AtomicBool = AtomicBool::new(false);

    let handle = make_handle(1024, 1024);
    let waker = make_test_waker(&WOKEN);

    // Simulate poll_read:
    // 1. Register waker FIRST
    handle.read_waker.register(&waker);
    WOKEN.store(false, Ordering::Release);

    // 2. Concurrent engine delivers data + wakes (between register and recheck)
    handle.rx_ring.write(b"concurrent data");
    handle.notify_all();

    // 3. The recheck would find data, but the waker was already called
    assert!(WOKEN.load(Ordering::Acquire));

    // 4. Read the data (simulating the recheck finding it)
    let mut buf = [0u8; 64];
    let n = handle.rx_ring.read(&mut buf);
    assert_eq!(&buf[..n], b"concurrent data");
}

/// Write waker: woken when send window opens (tx_ring space available).
#[test]
fn write_waker_called_when_tx_space_opens() {
    static WOKEN: AtomicBool = AtomicBool::new(false);

    // Small ring so we can fill it
    let handle = make_handle(1024, 64);
    let waker = make_test_waker(&WOKEN);

    // Fill the tx_ring
    let fill_data = vec![0xAA; 64];
    let written = handle.tx_ring.write(&fill_data);
    assert!(written > 0);

    // Register write waker (simulating poll_write when ring is full)
    handle.write_waker.register(&waker);
    WOKEN.store(false, Ordering::Release);

    // Engine drains some data (making space)
    let mut drain_buf = [0u8; 16];
    handle.tx_ring.read(&mut drain_buf);

    // Engine signals write_waker
    handle.write_waker.wake();

    assert!(
        WOKEN.load(Ordering::Acquire),
        "write waker must be called when tx space opens"
    );
}

/// Property: multiple register calls replace the waker — only latest is woken.
#[test]
fn latest_registered_waker_is_woken() {
    static WOKEN_1: AtomicBool = AtomicBool::new(false);
    static WOKEN_2: AtomicBool = AtomicBool::new(false);

    let handle = make_handle(1024, 1024);
    let waker1 = make_test_waker(&WOKEN_1);
    let waker2 = make_test_waker(&WOKEN_2);

    // Register first waker
    handle.read_waker.register(&waker1);
    // Replace with second waker
    handle.read_waker.register(&waker2);

    WOKEN_1.store(false, Ordering::Release);
    WOKEN_2.store(false, Ordering::Release);

    // Wake
    handle.read_waker.wake();

    // Only the latest waker should be called
    assert!(
        WOKEN_2.load(Ordering::Acquire),
        "latest registered waker must be woken"
    );
    // The first waker is NOT guaranteed to be woken (implementation takes it on replace)
}

/// Property test with proptest: for arbitrary data sizes, register-then-write-then-wake
/// always results in waker being called.
#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn register_write_wake_always_fires(
            data_len in 1usize..512,
        ) {
            static WOKEN: AtomicBool = AtomicBool::new(false);

            let handle = make_handle(1024, 1024);
            let waker = make_test_waker(&WOKEN);

            // Register
            handle.read_waker.register(&waker);
            WOKEN.store(false, Ordering::Release);

            // Engine writes data of arbitrary length
            let data: Vec<u8> = vec![0x42; data_len];
            handle.rx_ring.write(&data);

            // Engine notifies
            handle.notify_all();

            // Waker must be called
            prop_assert!(WOKEN.load(Ordering::Acquire));
        }

        #[test]
        fn eof_after_register_wakes(
            set_eof in proptest::bool::ANY,
        ) {
            static WOKEN: AtomicBool = AtomicBool::new(false);

            let handle = make_handle(1024, 1024);
            let waker = make_test_waker(&WOKEN);

            handle.read_waker.register(&waker);
            WOKEN.store(false, Ordering::Release);

            if set_eof {
                handle.set_eof();
            }
            handle.notify_all();

            // If eof was set, waker must fire (notify_all always wakes)
            prop_assert!(WOKEN.load(Ordering::Acquire));
        }
    }
}
