## YouTube video plan and script

### Series outline

- **Episode 1:** Why userspace networking? Repo tour, UDP parser with synthetic tests, echo demo.
- **Episode 2:** DPDK setup, hugepages, EAL init, NIC selection without getting stranded.
- **Episode 3:** Wiring RX/TX loop, batching, and the first real echo on hardware.
- **Episode 4:** Multi-queue, RSS, core pinning, and scaling measurements.
- **Episode 5:** Building a DNS forwarder, parsing, caching, and validating correctness.

### Episode 1 structure

- **Hook (0:00–0:30):** Quick demo of echo payloads and a teaser of performance motivation.
- **Context (0:30–2:00):** Why userspace vs std sockets, what we’re building, what we won’t.
- **Repo tour (2:00–4:30):** Workspace, crates, docs, and tests.
- **Code (4:30–10:30):** UDP/IPv4 parsing, checksum, handler trait; run unit tests.
- **Demo (10:30–12:00):** Synthetic echo generating a response frame.
- **Next steps (12:00–13:00):** DPDK environment and EAL for Episode 2.

### Episode 1 script (A-roll)

- **Opening:** “Today we’re starting a userspace networking journey in Rust. We won’t touch NICs yet. Instead, we’ll write and test the core of UDP parsing so the data path is rock solid before we add complexity.”
- **Value:** “Userspace stacks cut out syscalls and copies to hit high throughput. But they come with trade-offs. We’ll keep it honest, building this in phases and proving each stage with tests.”
- **Tour:** “The repo has three library crates: dpdk-sys for unsafe bindings, dpdk for safe wrappers, and dpdk-udp for protocol logic. Apps live under apps/, starting with an echo server.”
- **Code walkthrough:** “IPv4 parsing validates version, IHL, and total length. UDP parsing checks length and optionally checksum. The handler trait returns an optional reply payload, which we wrap back into IPv4+UDP with correct checksums.”
- **Test run:** “Let’s run the synthetic test. No NIC required. We build a frame, parse it, and confirm the payload echoes back. Green tests mean we’re ready to wire real RX/TX next.”
- **Close:** “In the next episode, we’ll set up hugepages and bring up DPDK’s EAL so this code can talk to a real NIC safely, without stranding our SSH session.”

### B-roll and overlays

- **B-roll:** Terminal running cargo test; highlighted code segments for ipv4.rs, udp.rs, checksum.rs; the repo tree; a diagram of RX→parse→dispatch→reply.
- **Overlays:** Bullet callouts for “Batching”, “Zero-copy”, “Safe wrapper”, “Synthetic tests”.

### On-screen checklist (end card)

- **Built:** UDP/IPv4 parse + checksum, handler interface, synthetic echo, tests.
- **Next:** Hugepages, EAL init, NIC binding, RX/TX loop.

### Description and links

- **Links:** Repo root, tag v0.0.1-phase0, docs/phase0_setup.md, docs/architecture.md.
- **Commands:** cargo test, cargo run -p echo.

---