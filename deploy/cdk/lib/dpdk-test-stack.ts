import * as cdk from 'aws-cdk-lib';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as s3assets from 'aws-cdk-lib/aws-s3-assets';
import { Construct } from 'constructs';

export class DpdkTestStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: cdk.StackProps) {
    super(scope, id, props);

    // Check for pre-built AMI via CDK context
    const amiId = this.node.tryGetContext('amiId');
    const usePrebuiltAmi = !!amiId;

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

    // Select machine image: pre-built DPDK AMI or stock AL2023
    const machineImage = usePrebuiltAmi
      ? ec2.MachineImage.genericLinux({ [this.region]: amiId })
      : ec2.MachineImage.latestAmazonLinux2023({
          cpuType: ec2.AmazonLinuxCpuType.X86_64,
        });

    // Timeout: 20 min with pre-built AMI (cargo build takes 8-12 min on c5n.large), 35 min for full bootstrap
    const creationTimeout = usePrebuiltAmi ? 'PT20M' : 'PT35M';

    // Helper: generate user-data commands for an instance
    const createUserData = (cfnResourceName: string): ec2.UserData => {
      const ud = ec2.UserData.forLinux();

      // Common preamble
      const preamble = [
        '#!/bin/bash',
        'set -euo pipefail',
        'exec > >(tee /var/log/user-data.log) 2>&1',
        'dnf install -y aws-cfn-bootstrap',
      ];

      // Bootstrap commands: install system packages, Rust, and DPDK from source
      const fullBootstrap = [
        'echo "=== Starting DPDK-STDLIB setup on Amazon Linux 3 ==="',

        // System packages
        'echo "=== Updating system packages ==="',
        'dnf update -y',
        'dnf groupinstall -y "Development Tools"',
        'dnf install -y git pciutils iperf3 --allowerasing',

        // Install Rust
        'echo "=== Installing Rust ==="',
        'export HOME=/root',
        'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y',
        'export HOME=/root',
        'source /root/.cargo/env',
        'echo "export PATH=/root/.cargo/bin:$PATH" >> /etc/profile',
        'echo "export HOME=/root" >> /etc/profile',

        // Install DPDK dependencies
        'echo "=== Installing DPDK dependencies ==="',
        'dnf install -y meson ninja-build python3-pip libbsd-devel libpcap-devel numactl-devel kernel-devel kernel-headers --allowerasing',
        'pip3 install pyelftools',

        // Download and build DPDK
        'echo "=== Downloading DPDK ==="',
        'cd /opt',
        'DPDK_VERSION="22.11.6"',
        'curl -L "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"',
        'tar -xf "dpdk-${DPDK_VERSION}.tar.xz"',
        'cd dpdk-stable-${DPDK_VERSION}',

        'echo "=== Building DPDK ==="',
        'meson setup build --prefix=/usr/local --buildtype=release -Denable_kmods=true -Ddisable_drivers=net/gve,net/ionic',
        'ninja -C build',
        'ninja -C build install',
        'echo "/usr/local/lib" > /etc/ld.so.conf.d/dpdk.conf',
        'ldconfig',
      ];

      // Pre-built AMI: DPDK, Rust, and system packages are already installed
      const prebuiltPreamble = [
        'echo "=== Using pre-built DPDK AMI ==="',
      ];

      // Runtime config: kernel modules + hugepages (needed on every boot)
      const runtimeConfig = [
        'echo "=== Configuring DPDK runtime ==="',
        'modprobe uio || echo "uio module already loaded"',
        'modprobe vfio-pci || echo "vfio-pci module already loaded"',
        'echo 1024 > /proc/sys/vm/nr_hugepages',
        'mkdir -p /mnt/huge',
        'mount -t hugetlbfs nodev /mnt/huge || echo "hugepages already mounted"',
      ];

      // Download project and set up workspace
      const projectSetup = [
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
      ];

      // Inline project files (Cargo.toml overrides, apps, etc.)
      const inlineProjectFiles = [
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

        // dpdk-udp Cargo.toml - must match the real crate (dpdk and libc are required deps)
        'cat > dpdk-udp/Cargo.toml << \'EOF\'',
        '[package]',
        'name = "dpdk-udp"',
        'version = "0.1.0"',
        'edition = "2021"',
        'description = "UDP protocol implementation with DPDK acceleration"',
        '',
        '[dependencies]',
        'thiserror = { workspace = true }',
        'libc = { workspace = true }',
        'dpdk = { path = "../dpdk" }',
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
        '    println!("DPDK-STDLIB Peer App");',
        '    ',
        '    #[cfg(feature = "dpdk")]',
        '    println!("DPDK support compiled in");',
        '    #[cfg(not(feature = "dpdk"))]',
        '    println!("Standard networking mode");',
        '    ',
        '    let bind_addr: SocketAddr = format!("{}:{}", args.bind_ip, args.bind_port).parse()?;',
        '    let socket = tokio::net::UdpSocket::bind(bind_addr).await?;',
        '    println!("Listening on {}", bind_addr);',
        '    ',
        '    if args.mode == "listen" || args.mode == "both" {',
        '        let mut buf = [0u8; 1024];',
        '        loop {',
        '            match socket.recv_from(&mut buf).await {',
        '                Ok((size, from)) => {',
        '                    let msg = String::from_utf8_lossy(&buf[..size]);',
        '                    println!("Received from {}: {}", from, msg);',
        '                    ',
        '                    // Echo back',
        '                    let response = format!("echo: {}", msg);',
        '                    socket.send_to(response.as_bytes(), from).await?;',
        '                }',
        '                Err(e) => eprintln!("Receive error: {}", e),',
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
        '    println!("DPDK Echo Server ready on port {}", args.port);',
        '    println!("Rust: {}", env!("RUSTC_VERSION", "unknown"));',
        '    println!("DPDK libraries: {} found", std::fs::read_dir("/usr/local/lib").map(|d| d.count()).unwrap_or(0));',
        '    println!("Instance setup complete!");',
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
        'echo "Binding ENI $ENI_ID to DPDK..."',
        'PCI_ADDR=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d\' \' -f1)',
        'if [[ -z "$PCI_ADDR" ]]; then',
        '    echo "Could not find ENA device PCI address"',
        '    exit 1',
        'fi',
        'echo "Found ENA device at PCI address: $PCI_ADDR"',
        'modprobe uio',
        'modprobe vfio-pci',
        'echo "$PCI_ADDR" > /sys/bus/pci/devices/$PCI_ADDR/driver/unbind 2>/dev/null || true',
        'echo "1d0f 0ec2" > /sys/bus/pci/drivers/vfio-pci/new_id 2>/dev/null || true',
        'echo "$PCI_ADDR" > /sys/bus/pci/drivers/vfio-pci/bind',
        'echo "Successfully bound $PCI_ADDR to vfio-pci"',
        'echo "ENI $ENI_ID ready for DPDK"',
        'EOF',

        'chmod +x scripts/bind_eni.sh',
      ];

      // Build the project
      const buildProject = [
        'echo "=== Building project ==="',
        'export HOME=/root',
        'source /root/.cargo/env',
        'PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release',

        'echo "=== Testing build ==="',
        'export HOME=/root',
        'source /root/.cargo/env',
        './target/release/echo --port 9000',
        './target/release/peer-app --help',

        'echo "=== Setup complete! ==="',
        'echo "DPDK libraries: $(ls /usr/local/lib/libdpdk* 2>/dev/null | wc -l)"',
        'echo "Rust project built successfully"',
        'echo "Instance ready for testing"',
        'echo "Project location: /opt/dpdk-stdlib"',
      ];

      // Signal CloudFormation with the correct resource name
      const cfnSignal = [
        'echo "=== Signaling CloudFormation success ==="',
        `/opt/aws/bin/cfn-signal -e $? --stack ${this.stackName} --resource ${cfnResourceName} --region ${this.region}`,
        'echo "CloudFormation signaled successfully"',
      ];

      // Assemble the full command list based on AMI type
      if (usePrebuiltAmi) {
        ud.addCommands(
          ...preamble,
          ...prebuiltPreamble,
          ...runtimeConfig,
          ...projectSetup,
          ...inlineProjectFiles,
          ...buildProject,
          ...cfnSignal,
        );
      } else {
        ud.addCommands(
          ...preamble,
          ...fullBootstrap,
          ...runtimeConfig,
          ...projectSetup,
          ...inlineProjectFiles,
          ...buildProject,
          ...cfnSignal,
        );
      }

      return ud;
    };

    // Create per-instance user data with correct cfn-signal resource names
    const senderUserData = createUserData('DpdkSender');
    const receiverUserData = createUserData('DpdkReceiver');

    // Create sender instance
    const senderInstance = new ec2.Instance(this, 'DpdkSender', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C5N, ec2.InstanceSize.LARGE),
      machineImage,
      securityGroup: mgmtSecurityGroup,
      userData: senderUserData,
      role: instanceRole,
      vpcSubnets: {
        subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
      },
    });

    // Add CreationPolicy to wait for setup completion
    const cfnSenderInstance = senderInstance.node.defaultChild as ec2.CfnInstance;
    // Override logical ID so cfn-signal --resource DpdkSender matches the CloudFormation resource name
    cfnSenderInstance.overrideLogicalId('DpdkSender');
    cfnSenderInstance.cfnOptions.creationPolicy = {
      resourceSignal: {
        timeout: creationTimeout,
        count: 1,
      },
    };

    // Create receiver instance
    const receiverInstance = new ec2.Instance(this, 'DpdkReceiver', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C5N, ec2.InstanceSize.LARGE),
      machineImage,
      securityGroup: mgmtSecurityGroup,
      userData: receiverUserData,
      role: instanceRole,
      vpcSubnets: {
        subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
      },
    });

    // Add CreationPolicy for receiver too (bug fix: was previously missing)
    const cfnReceiverInstance = receiverInstance.node.defaultChild as ec2.CfnInstance;
    // Override logical ID so cfn-signal --resource DpdkReceiver matches the CloudFormation resource name
    cfnReceiverInstance.overrideLogicalId('DpdkReceiver');
    cfnReceiverInstance.cfnOptions.creationPolicy = {
      resourceSignal: {
        timeout: creationTimeout,
        count: 1,
      },
    };

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

    if (usePrebuiltAmi) {
      new cdk.CfnOutput(this, 'PrebuiltAmiId', {
        value: amiId,
        description: 'Pre-built DPDK AMI ID used for this deployment',
      });
    }
  }
}
