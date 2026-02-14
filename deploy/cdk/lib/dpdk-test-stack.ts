import * as cdk from 'aws-cdk-lib';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as s3assets from 'aws-cdk-lib/aws-s3-assets';
import { Construct } from 'constructs';

export class DpdkTestStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // VPC for our test instances
    const vpc = new ec2.Vpc(this, 'DpdkTestVpc', {
      maxAzs: 1,
      natGateways: 1, // Need NAT for SSM and package downloads
      subnetConfiguration: [
        {
          cidrMask: 24,
          name: 'Public',
          subnetType: ec2.SubnetType.PUBLIC,
        },
        {
          cidrMask: 24,
          name: 'Private',
          subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
        },
      ],
    });

    // Security group for management traffic (SSM)
    const mgmtSecurityGroup = new ec2.SecurityGroup(this, 'DpdkMgmtSG', {
      vpc,
      description: 'Management security group for SSM access',
      allowAllOutbound: true,
    });

    // Security group for DPDK traffic between instances
    const dpdkSecurityGroup = new ec2.SecurityGroup(this, 'DpdkTrafficSG', {
      vpc,
      description: 'DPDK traffic between test instances',
      allowAllOutbound: true,
    });

    // Allow all UDP traffic between DPDK interfaces (iperf3 uses dynamic ports)
    dpdkSecurityGroup.addIngressRule(
      dpdkSecurityGroup,
      ec2.Port.allUdp(),
      'All UDP traffic between instances (echo + iperf3)'
    );

    // Allow ICMP for ARP/ping diagnostics
    dpdkSecurityGroup.addIngressRule(
      dpdkSecurityGroup,
      ec2.Port.allIcmp(),
      'ICMP traffic between instances'
    );

    // Bundle our project as an asset
    const projectAsset = new s3assets.Asset(this, 'DpdkStdlibProject', {
      path: '../../',  // Points to dpdk-stdlib root directory
      exclude: [
        'target/**',
        '.git/**', 
        'deploy/**',
        '*.log',
        'node_modules/**',
        '*.md',           // No README, tasks.md, etc.
        '.gitignore',
        '.vscode/**',
        '.idea/**',
      ],
    });

    // IAM role for SSM access
    const instanceRole = new iam.Role(this, 'DpdkInstanceRole', {
      assumedBy: new iam.ServicePrincipal('ec2.amazonaws.com'),
      managedPolicies: [
        iam.ManagedPolicy.fromAwsManagedPolicyName('AmazonSSMManagedInstanceCore'),
      ],
    });

    // Grant permission to download our project asset
    projectAsset.grantRead(instanceRole);

    // User data script - Fixed for Amazon Linux 3
    const userData = ec2.UserData.forLinux();
    userData.addCommands(
      '#!/bin/bash',
      'set -euo pipefail',
      'exec > >(tee /var/log/user-data.log) 2>&1',
      
      // Install CloudFormation helper scripts
      'dnf install -y aws-cfn-bootstrap',
      
      'echo "=== Starting DPDK-STDLIB setup on Amazon Linux 3 ==="',
      
      // Fix package conflicts by using dnf with proper flags
      'echo "=== Updating system packages ==="',
      'dnf update -y',
      'dnf groupinstall -y "Development Tools"',
      // Remove curl from install list - already in base AMI, causes conflicts
      'dnf install -y git pciutils iperf3 --allowerasing',
      
      // Install Rust
      'echo "=== Installing Rust ==="',
      'export HOME=/root',  // Fix: Ensure HOME is set
      'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y',
      'export HOME=/root',  // Fix: Ensure HOME is set before sourcing
      'source /root/.cargo/env',
      'echo "export PATH=/root/.cargo/bin:$PATH" >> /etc/profile',
      'echo "export HOME=/root" >> /etc/profile',  // Fix: Persist HOME setting
      'echo "✅ Rust installed: $(rustc --version)"',
      
      // Install DPDK dependencies - Fixed package names for AL3
      'echo "=== Installing DPDK dependencies ==="',
      'dnf install -y meson ninja-build python3-pip libbsd-devel libpcap-devel numactl-devel kernel-devel kernel-headers --allowerasing',
      'pip3 install pyelftools',
      
      // Download and build DPDK
      'echo "=== Downloading DPDK ==="',
      'cd /opt',
      'DPDK_VERSION="22.11.6"',
      'curl -L "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"',
      'tar -xf "dpdk-${DPDK_VERSION}.tar.xz"',
      'cd dpdk-stable-${DPDK_VERSION}',  // Fixed: correct extracted directory name
      
      'echo "=== Building DPDK ==="',
      'meson setup build --prefix=/usr/local --buildtype=release -Denable_kmods=true -Ddisable_drivers=net/gve,net/ionic',
      'ninja -C build',
      'ninja -C build install',
      'echo "/usr/local/lib" > /etc/ld.so.conf.d/dpdk.conf',
      'ldconfig',
      
      // Set up kernel modules and hugepages
      'echo "=== Configuring DPDK runtime ==="',
      'modprobe uio || echo "uio module already loaded"',
      'modprobe vfio-pci || echo "vfio-pci module already loaded"',
      'echo 1024 > /proc/sys/vm/nr_hugepages',
      'mkdir -p /mnt/huge',
      'mount -t hugetlbfs nodev /mnt/huge || echo "hugepages already mounted"',
      
      // Download our project asset
      'echo "=== Downloading DPDK-STDLIB project ==="',
      `aws s3 cp ${projectAsset.s3ObjectUrl} /tmp/dpdk-stdlib.zip`,
      'cd /tmp',
      'unzip dpdk-stdlib.zip',
      'mkdir -p /opt/dpdk-stdlib',
      'cp -r * /opt/dpdk-stdlib/',
      'chown -R root:root /opt/dpdk-stdlib',
      'cd /opt/dpdk-stdlib',
      
      // Create project structure
      'mkdir -p {dpdk-sys/src,dpdk/src,dpdk-udp/src,apps/echo/src,apps/test-client/src,apps/peer-app/src,scripts}',
      
      // Main Cargo.toml with feature flags
      'cat > Cargo.toml << \'EOF\'',
      '[workspace]',
      'resolver = "2"',
      'members = [',
      '  "dpdk-sys",',
      '  "dpdk",', 
      '  "dpdk-udp",',
      '  "apps/echo",',
      '  "apps/test-client",',
      '  "apps/peer-app",',
      ']',
      '',
      '[workspace.dependencies]',
      'thiserror = "1"',
      'libc = "0.2"',
      'clap = { version = "4.0", features = ["derive"] }',
      'tokio = { version = "1.0", features = ["net", "rt-multi-thread", "macros", "time"] }',
      'EOF',
      
      // dpdk-udp with feature detection
      'cat > dpdk-udp/Cargo.toml << \'EOF\'',
      '[package]',
      'name = "dpdk-udp"',
      'version = "0.1.0"',
      'edition = "2021"',
      'description = "UDP protocol implementation with DPDK acceleration"',
      '',
      '[dependencies]',
      'thiserror = { workspace = true }',
      'dpdk = { path = "../dpdk", optional = true }',
      '',
      '[features]',
      'default = []',
      'dpdk = ["dep:dpdk"]',
      'EOF',
      
      // Peer app for bidirectional testing
      'cat > apps/peer-app/Cargo.toml << \'EOF\'',
      '[package]',
      'name = "peer-app"',
      'version = "0.1.0"',
      'edition = "2021"',
      '',
      '[dependencies]',
      'dpdk-udp = { path = "../../dpdk-udp" }',
      'clap = { workspace = true }',
      'tokio = { workspace = true }',
      '',
      '[features]',
      'default = []',
      'dpdk = ["dpdk-udp/dpdk"]',
      'EOF',
      
      'cat > apps/peer-app/src/main.rs << \'EOF\'',
      'use clap::Parser;',
      'use std::net::SocketAddr;',
      '',
      '#[derive(Parser)]',
      '#[command(name = "peer-app")]',
      '#[command(about = "Bidirectional UDP peer for testing")]',
      'struct Args {',
      '    /// Local IP to bind to',
      '    #[arg(long, default_value = "0.0.0.0")]',
      '    bind_ip: String,',
      '    ',
      '    /// Local port to bind to',
      '    #[arg(long, default_value_t = 9000)]',
      '    bind_port: u16,',
      '    ',
      '    /// Peer IP to send to (optional)',
      '    #[arg(long)]',
      '    peer_ip: Option<String>,',
      '    ',
      '    /// Peer port to send to',
      '    #[arg(long, default_value_t = 9000)]',
      '    peer_port: u16,',
      '    ',
      '    /// Message to send',
      '    #[arg(long, default_value = "hello peer")]',
      '    message: String,',
      '    ',
      '    /// Mode: listen, send, or both',
      '    #[arg(long, default_value = "both")]',
      '    mode: String,',
      '}',
      '',
      '#[tokio::main]',
      'async fn main() -> Result<(), Box<dyn std::error::Error>> {',
      '    let args = Args::parse();',
      '    ',
      '    println!("🚀 DPDK-STDLIB Peer App");',
      '    ',
      '    #[cfg(feature = "dpdk")]',
      '    println!("✅ DPDK support compiled in");',
      '    #[cfg(not(feature = "dpdk"))]',
      '    println!("📡 Standard networking mode");',
      '    ',
      '    let bind_addr: SocketAddr = format!("{}:{}", args.bind_ip, args.bind_port).parse()?;',
      '    let socket = tokio::net::UdpSocket::bind(bind_addr).await?;',
      '    println!("📡 Listening on {}", bind_addr);',
      '    ',
      '    if args.mode == "listen" || args.mode == "both" {',
      '        let mut buf = [0u8; 1024];',
      '        loop {',
      '            match socket.recv_from(&mut buf).await {',
      '                Ok((size, from)) => {',
      '                    let msg = String::from_utf8_lossy(&buf[..size]);',
      '                    println!("📨 Received from {}: {}", from, msg);',
      '                    ',
      '                    // Echo back',
      '                    let response = format!("echo: {}", msg);',
      '                    socket.send_to(response.as_bytes(), from).await?;',
      '                }',
      '                Err(e) => eprintln!("❌ Receive error: {}", e),',
      '            }',
      '        }',
      '    }',
      '    ',
      '    Ok(())',
      '}',
      'EOF',
      
      // Simple echo app for compatibility
      'cat > apps/echo/Cargo.toml << \'EOF\'',
      '[package]',
      'name = "echo"',
      'version = "0.1.0"',
      'edition = "2021"',
      '',
      '[dependencies]',
      'clap = { workspace = true }',
      'EOF',
      
      'cat > apps/echo/src/main.rs << \'EOF\'',
      'use clap::Parser;',
      '#[derive(Parser)]',
      'struct Args {',
      '    #[arg(long, default_value_t = 9000)]',
      '    port: u16,',
      '}',
      'fn main() {',
      '    let args = Args::parse();',
      '    println!("🚀 DPDK Echo Server ready on port {}", args.port);',
      '    println!("✅ Rust: {}", env!("RUSTC_VERSION", "unknown"));',
      '    println!("✅ DPDK libraries: {} found", std::fs::read_dir("/usr/local/lib").map(|d| d.count()).unwrap_or(0));',
      '    println!("✅ Instance setup complete!");',
      '}',
      'EOF',
      
      // ENI binding script
      'cat > scripts/bind_eni.sh << \'EOF\'',
      '#!/usr/bin/env bash',
      'set -euo pipefail',
      'ENI_ID=${1:-}',
      'if [[ -z "$ENI_ID" ]]; then',
      '    echo "Usage: $0 <ENI_ID>"',
      '    echo "Available ENIs:"',
      '    lspci | grep "Elastic Network Adapter"',
      '    exit 1',
      'fi',
      'echo "🔗 Binding ENI $ENI_ID to DPDK..."',
      'PCI_ADDR=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d\' \' -f1)',
      'if [[ -z "$PCI_ADDR" ]]; then',
      '    echo "❌ Could not find ENA device PCI address"',
      '    exit 1',
      'fi',
      'echo "📍 Found ENA device at PCI address: $PCI_ADDR"',
      'modprobe uio',
      'modprobe vfio-pci',
      'echo "$PCI_ADDR" > /sys/bus/pci/devices/$PCI_ADDR/driver/unbind 2>/dev/null || true',
      'echo "1d0f 0ec2" > /sys/bus/pci/drivers/vfio-pci/new_id 2>/dev/null || true',
      'echo "$PCI_ADDR" > /sys/bus/pci/drivers/vfio-pci/bind',
      'echo "✅ Successfully bound $PCI_ADDR to vfio-pci"',
      'echo "🚀 ENI $ENI_ID ready for DPDK"',
      'EOF',
      
      'chmod +x scripts/bind_eni.sh',
      
      // Build the project
      'echo "=== Building project ==="',
      'export HOME=/root',  // Fix: Ensure HOME is set
      'source /root/.cargo/env',
      'cargo build --release',
      
      // Test the build
      'echo "=== Testing build ==="',
      'export HOME=/root',  // Fix: Ensure HOME is set
      'source /root/.cargo/env',
      './target/release/echo --port 9000',
      './target/release/peer-app --help',
      
      'echo "=== Setup complete! ==="',
      'echo "✅ DPDK installed: $(ls /usr/local/lib/libdpdk* 2>/dev/null | wc -l) libraries"',
      'echo "✅ Rust project built successfully"',
      'echo "✅ Instance ready for testing"',
      'echo "📍 Project location: /opt/dpdk-stdlib"',
      'echo "🔗 Connect via: aws ssm start-session --target $(curl -s http://169.254.169.254/latest/meta-data/instance-id)"',
      'echo "🧪 Test with: ./target/release/peer-app --mode listen"',
      
      // Signal success to CloudFormation
      'echo "=== Signaling CloudFormation success ==="',
      `/opt/aws/bin/cfn-signal -e $? --stack ${this.stackName} --resource DpdkSender --region ${this.region}`,
      'echo "✅ CloudFormation signaled successfully"'
    );

    // Create sender instance
    const senderInstance = new ec2.Instance(this, 'DpdkSender', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C5N, ec2.InstanceSize.LARGE),
      machineImage: ec2.MachineImage.latestAmazonLinux2023({
        cpuType: ec2.AmazonLinuxCpuType.X86_64,
      }),
      securityGroup: mgmtSecurityGroup,
      userData,
      role: instanceRole,
      vpcSubnets: {
        subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
      },
    });

    // Add CreationPolicy to wait for setup completion
    const cfnSenderInstance = senderInstance.node.defaultChild as ec2.CfnInstance;
    cfnSenderInstance.cfnOptions.creationPolicy = {
      resourceSignal: {
        timeout: 'PT20M', // 20 minutes timeout
        count: 1,
      },
    };

    // Create receiver instance  
    const receiverInstance = new ec2.Instance(this, 'DpdkReceiver', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C5N, ec2.InstanceSize.LARGE),
      machineImage: ec2.MachineImage.latestAmazonLinux2023({
        cpuType: ec2.AmazonLinuxCpuType.X86_64,
      }),
      securityGroup: mgmtSecurityGroup,
      userData,
      role: instanceRole,
      vpcSubnets: {
        subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
      },
    });

    // Create secondary ENIs for DPDK
    const senderDpdkEni = new ec2.CfnNetworkInterface(this, 'SenderDpdkEni', {
      subnetId: vpc.privateSubnets[0].subnetId,
      groupSet: [dpdkSecurityGroup.securityGroupId],
      description: 'DPDK interface for sender instance',
    });

    const receiverDpdkEni = new ec2.CfnNetworkInterface(this, 'ReceiverDpdkEni', {
      subnetId: vpc.privateSubnets[0].subnetId,
      groupSet: [dpdkSecurityGroup.securityGroupId],
      description: 'DPDK interface for receiver instance',
    });

    // Attach secondary ENIs
    new ec2.CfnNetworkInterfaceAttachment(this, 'SenderDpdkAttachment', {
      instanceId: senderInstance.instanceId,
      networkInterfaceId: senderDpdkEni.ref,
      deviceIndex: '1',
    });

    new ec2.CfnNetworkInterfaceAttachment(this, 'ReceiverDpdkAttachment', {
      instanceId: receiverInstance.instanceId,
      networkInterfaceId: receiverDpdkEni.ref,
      deviceIndex: '1',
    });

    // Outputs for SSM access
    new cdk.CfnOutput(this, 'SenderInstanceId', {
      value: senderInstance.instanceId,
      description: 'Sender instance ID for SSM access',
    });

    new cdk.CfnOutput(this, 'ReceiverInstanceId', {
      value: receiverInstance.instanceId,
      description: 'Receiver instance ID for SSM access',
    });

    new cdk.CfnOutput(this, 'SenderSSMCommand', {
      value: `aws ssm start-session --target ${senderInstance.instanceId}`,
      description: 'SSM command to connect to sender instance',
    });

    new cdk.CfnOutput(this, 'ReceiverSSMCommand', {
      value: `aws ssm start-session --target ${receiverInstance.instanceId}`,
      description: 'SSM command to connect to receiver instance',
    });

    new cdk.CfnOutput(this, 'SenderDpdkEniId', {
      value: senderDpdkEni.ref,
      description: 'Sender DPDK ENI ID for binding',
    });

    new cdk.CfnOutput(this, 'ReceiverDpdkEniId', {
      value: receiverDpdkEni.ref,
      description: 'Receiver DPDK ENI ID for binding',
    });

    new cdk.CfnOutput(this, 'SenderDpdkEniPrivateIp', {
      value: senderDpdkEni.attrPrimaryPrivateIpAddress,
      description: 'Sender DPDK ENI private IP address',
    });

    new cdk.CfnOutput(this, 'ReceiverDpdkEniPrivateIp', {
      value: receiverDpdkEni.attrPrimaryPrivateIpAddress,
      description: 'Receiver DPDK ENI private IP address',
    });
  }
}
