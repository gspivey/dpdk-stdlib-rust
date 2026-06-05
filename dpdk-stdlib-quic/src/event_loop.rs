//! Event loop driving the s2n-quic endpoint.
//!
//! The event loop runs on a dedicated `std::thread` and drives:
//! - `endpoint.poll_wakeups()` for application wakeups
//! - RX: `recv_frames()` → parse → `endpoint.receive()`
//! - TX: `endpoint.transmit()` → drain → `send_frame()`
//! - Timer sleep until `endpoint.timeout()`
