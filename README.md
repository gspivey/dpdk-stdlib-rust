# DPDK-STDLIB

A Rust library providing safe, high-level abstractions for DPDK (Data Plane Development Kit) with focus on UDP networking.

## Features

- **Safe DPDK Wrapper**: Memory-safe Rust bindings for DPDK
- **UDP Protocol Layer**: High-level UDP socket abstraction
- **Synthetic Mode**: Test without real DPDK installation
- **Cross-Platform**: macOS (synthetic) and Linux (real DPDK)
- **Echo Server**: Example UDP echo application

## Quick Start

### Synthetic Mode (Development)
```bash
# Works on any platform without DPDK
cargo run -p echo
```

### Real DPDK Mode (Linux)
```bash
# Install DPDK first
sudo ./scripts/install_dpdk_amazon_linux.sh

# Build with DPDK support
cargo run -p echo --features dpdk-support -- --dpdk --dpdk-args="-l 0-1 -n 4"
```

### Test Client
```bash
cargo run -p test-client -- --target 10.0.0.2 --port 9000 --message "hello world"
```

## Architecture

```
┌─────────────────┐
│   Applications  │  (echo, test-client)
├─────────────────┤
│    dpdk-udp     │  (UDP protocol layer)
├─────────────────┤
│      dpdk       │  (Safe wrapper)
├─────────────────┤
│    dpdk-sys     │  (Raw FFI bindings)
└─────────────────┘
```

## Installation

### Amazon Linux 3
```bash
./scripts/install_dpdk_amazon_linux.sh
```

### macOS (Synthetic Only)
```bash
# No DPDK installation needed
cargo build
```

## AWS Deployment

Deploy test infrastructure to EC2:
```bash
cd deploy/cdk
npm install
cdk deploy --profile your-aws-profile
```

This creates:
- 2x c6gn.large instances (sender/receiver)
- Dual ENIs (management + DPDK)
- SSM access (no SSH keys needed)

## Usage Examples

### Echo Server
```bash
# Synthetic mode (default)
cargo run -p echo

# DPDK mode with custom IP/port
cargo run -p echo --features dpdk-support -- \
  --dpdk --dpdk-args="-l 0-1 -n 4" \
  --ip 192.168.1.100 --port 8080
```

### Test Client
```bash
# Send single packet
cargo run -p test-client -- --target 192.168.1.100 --port 8080

# Send multiple packets with delay
cargo run -p test-client -- \
  --target 192.168.1.100 \
  --count 10 \
  --delay 500 \
  --message "test packet"
```

## Development

### Project Structure
```
dpdk-stdlib/
├── dpdk-sys/          # Raw DPDK FFI bindings
├── dpdk/              # Safe Rust wrapper
├── dpdk-udp/          # UDP protocol implementation
├── apps/
│   ├── echo/          # UDP echo server
│   └── test-client/   # UDP test client
├── scripts/           # Installation scripts
├── deploy/cdk/        # AWS CDK deployment
└── docs/              # Documentation
```

### Building
```bash
# Check all crates
cargo check

# Build with DPDK support
cargo build --features dpdk-support

# Run tests
cargo test
```

### Platform Support

| Platform | Synthetic Mode | Real DPDK | Notes |
|----------|---------------|-----------|-------|
| macOS    | ✅            | ❌        | DPDK 23.11+ lacks macOS support |
| Linux    | ✅            | ✅        | Full DPDK functionality |
| Windows  | ❌            | ❌        | Not implemented |

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make changes with tests
4. Submit a pull request

## License

MIT License - see LICENSE file for details.
