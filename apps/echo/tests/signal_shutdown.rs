use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

/// Test that the echo server exits gracefully on SIGTERM.
///
/// This is critical for the perf test pipeline: between benchmark configs,
/// we send SIGTERM to stop the echo server. If it doesn't exit cleanly,
/// DPDK cleanup (rte_eal_cleanup) never runs and the vfio-pci device is
/// left in a dirty state, blocking the next DPDK app.
#[test]
fn echo_exits_gracefully_on_sigterm() {
    // Build the echo binary first (without dpdk feature — uses std networking)
    let build = Command::new("cargo")
        .args(["build", "--bin", "echo"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("failed to build echo binary");
    assert!(build.status.success(), "cargo build failed: {}", String::from_utf8_lossy(&build.stderr));

    // Find the binary
    let binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/echo");
    assert!(binary.exists(), "echo binary not found at {:?}", binary);

    // Start the echo server on a random high port
    let mut child = Command::new(&binary)
        .args(["--ip", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start echo server");

    let pid = child.id();

    // Wait for the server to start (it prints to stdout when ready)
    std::thread::sleep(Duration::from_millis(500));

    // Send SIGTERM
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

    // Wait for exit with a timeout
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(5) {
                    child.kill().ok();
                    panic!("echo server did not exit within 5s after SIGTERM");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for child: {}", e),
        }
    };

    // Read stdout/stderr
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout).ok();
    }
    if let Some(mut err) = child.stderr.take() {
        err.read_to_string(&mut stderr).ok();
    }

    // The process should exit cleanly (code 0)
    assert!(
        status.success(),
        "echo server exited with non-zero status {:?}\nstdout: {}\nstderr: {}",
        status.code(),
        stdout,
        stderr,
    );

    // Should print the graceful shutdown message
    assert!(
        stdout.contains("Shutting down gracefully"),
        "expected 'Shutting down gracefully' in stdout.\nstdout: {}\nstderr: {}",
        stdout,
        stderr,
    );
}

/// Test that the echo server also handles SIGINT (Ctrl+C) gracefully.
#[test]
fn echo_exits_gracefully_on_sigint() {
    let binary = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/echo");
    if !binary.exists() {
        // Build was done in the SIGTERM test; skip if binary is missing
        return;
    }

    let mut child = Command::new(&binary)
        .args(["--ip", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to start echo server");

    let pid = child.id();
    std::thread::sleep(Duration::from_millis(500));

    // Send SIGINT (Ctrl+C equivalent)
    unsafe {
        libc::kill(pid as i32, libc::SIGINT);
    }

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(5) {
                    child.kill().ok();
                    panic!("echo server did not exit within 5s after SIGINT");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => panic!("error waiting for child: {}", e),
        }
    };

    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut stdout).ok();
    }

    assert!(
        status.success(),
        "echo server should exit cleanly on SIGINT, got {:?}",
        status.code(),
    );
    assert!(
        stdout.contains("Shutting down gracefully"),
        "expected graceful shutdown message on SIGINT, got: {}",
        stdout,
    );
}
