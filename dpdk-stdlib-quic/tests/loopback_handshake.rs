//! Full QUIC handshake integration test over LoopbackBackend.
//!
//! Server and client each get their own provider backed by a paired loopback:
//! frames sent by one side are received by the other. Exercises the complete
//! QUIC handshake path with rcgen TLS, stream open, data send/echo, and
//! integrity verification.

use dpdk_udp::{PacketBackend, RxReadiness};
use dpdk_stdlib_quic::DpdkProvider;
use s2n_quic::{client::Connect, Client, Server};
use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A paired loopback backend: frames sent here appear in the partner's recv.
struct PairedLoopback {
    rx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    tx_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    mac: [u8; 6],
    promiscuous: AtomicBool,
    allmulticast: AtomicBool,
}

impl PairedLoopback {
    fn new_pair() -> (Arc<Self>, Arc<Self>) {
        let q1 = Arc::new(Mutex::new(VecDeque::new()));
        let q2 = Arc::new(Mutex::new(VecDeque::new()));

        let a = Arc::new(Self {
            rx_queue: Arc::clone(&q1),
            tx_queue: Arc::clone(&q2),
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            promiscuous: AtomicBool::new(false),
            allmulticast: AtomicBool::new(false),
        });

        let b = Arc::new(Self {
            rx_queue: q2,
            tx_queue: q1,
            mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
            promiscuous: AtomicBool::new(false),
            allmulticast: AtomicBool::new(false),
        });

        (a, b)
    }
}

impl PacketBackend for PairedLoopback {
    fn send_frame(&self, frame: &[u8]) -> io::Result<usize> {
        self.tx_queue.lock().unwrap().push_back(frame.to_vec());
        Ok(frame.len())
    }

    fn recv_frames(&self, max_frames: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut q = self.rx_queue.lock().unwrap();
        let n = max_frames.min(q.len());
        Ok(q.drain(..n).collect())
    }

    fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    fn backend_name(&self) -> &'static str {
        "paired-loopback"
    }

    fn set_promiscuous(&self, enable: bool) -> io::Result<()> {
        self.promiscuous.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_promiscuous(&self) -> bool {
        self.promiscuous.load(Ordering::Relaxed)
    }

    fn set_allmulticast(&self, enable: bool) -> io::Result<()> {
        self.allmulticast.store(enable, Ordering::Relaxed);
        Ok(())
    }

    fn is_allmulticast(&self) -> bool {
        self.allmulticast.load(Ordering::Relaxed)
    }

    fn rx_readiness(&self) -> RxReadiness {
        RxReadiness::PollOnly
    }
}

fn generate_tls_pair() -> (String, String) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen cert generation failed");
    (cert.cert.pem(), cert.key_pair.serialize_pem())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loopback_full_quic_handshake() {
    let (server_backend, client_backend) = PairedLoopback::new_pair();
    let (cert_pem, key_pem) = generate_tls_pair();

    let server_addr: std::net::SocketAddr = "10.0.0.1:4433".parse().unwrap();
    let client_addr: std::net::SocketAddr = "10.0.0.2:5000".parse().unwrap();

    // Save MACs before moving Arc
    let server_mac = server_backend.mac_address();
    let client_mac = client_backend.mac_address();

    // Build server provider with paired loopback
    let (server_provider, mut server_handle) = DpdkProvider::builder()
        .with_addr(server_addr)
        .with_gateway_mac(client_mac)
        .with_backend(server_backend as Arc<dyn PacketBackend>)
        .build();

    // Build client provider with paired loopback
    let (client_provider, mut client_handle) = DpdkProvider::builder()
        .with_addr(client_addr)
        .with_gateway_mac(server_mac)
        .with_backend(client_backend as Arc<dyn PacketBackend>)
        .build();

    // Start server
    let mut server = Server::builder()
        .with_tls((cert_pem.as_str(), key_pem.as_str()))
        .unwrap()
        .with_io(server_provider)
        .unwrap()
        .start()
        .unwrap();

    // Start client (trust the self-signed cert)
    let client = Client::builder()
        .with_tls(cert_pem.as_str())
        .unwrap()
        .with_io(client_provider)
        .unwrap()
        .start()
        .unwrap();

    let test_data: &[u8] = b"hello from dpdk-stdlib-quic loopback test!";

    // Server task: accept connection, read stream, echo back
    let server_task = tokio::spawn(async move {
        let mut connection = server.accept().await.unwrap();
        let mut stream = connection
            .accept_bidirectional_stream()
            .await
            .unwrap()
            .unwrap();

        // Read all data until stream is finished
        let mut received = Vec::new();
        while let Ok(Some(chunk)) = stream.receive().await {
            received.extend_from_slice(&chunk);
        }

        // Echo back
        stream
            .send(bytes::Bytes::from(received))
            .await
            .unwrap();
        stream.finish().unwrap();
    });

    // Client: connect, open stream, send data, read echo
    let connect = Connect::new(server_addr).with_server_name("localhost");
    let mut connection = client.connect(connect).await.unwrap();
    let mut stream = connection.open_bidirectional_stream().await.unwrap();

    // Send test data
    stream.send(bytes::Bytes::copy_from_slice(test_data)).await.unwrap();
    stream.finish().unwrap();

    // Read echo
    let mut received = Vec::new();
    while let Ok(Some(chunk)) = stream.receive().await {
        received.extend_from_slice(&chunk);
    }

    // Verify data integrity
    assert_eq!(received, test_data, "echoed data must match original");

    // Clean up
    server_task.await.unwrap();
    server_handle.shutdown();
    client_handle.shutdown();
}
