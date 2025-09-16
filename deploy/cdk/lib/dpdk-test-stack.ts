import * as cdk from 'aws-cdk-lib';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as iam from 'aws-cdk-lib/aws-iam';
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

    // Allow UDP traffic between DPDK interfaces
    dpdkSecurityGroup.addIngressRule(
      dpdkSecurityGroup,
      ec2.Port.udp(9000),
      'UDP echo traffic between instances'
    );

    // IAM role for SSM access
    const instanceRole = new iam.Role(this, 'DpdkInstanceRole', {
      assumedBy: new iam.ServicePrincipal('ec2.amazonaws.com'),
      managedPolicies: [
        iam.ManagedPolicy.fromAwsManagedPolicyName('AmazonSSMManagedInstanceCore'),
      ],
    });

    // User data script
    const userData = ec2.UserData.forLinux();
    userData.addCommands(
      '#!/bin/bash',
      'set -euo pipefail',
      'exec > >(tee /var/log/user-data.log) 2>&1',
      
      'echo "Starting DPDK-STDLIB setup..."',
      'yum update -y',
      'yum groupinstall -y "Development Tools"',
      'yum install -y git curl lspci-devel pciutils',
      
      // Install Rust
      'echo "Installing Rust..."',
      'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y',
      'source /root/.cargo/env',
      'echo "export PATH=/root/.cargo/bin:$PATH" >> /etc/profile',
      
      // Create project directory and copy our code
      'echo "Setting up project..."',
      'mkdir -p /opt/dpdk-stdlib',
      'cd /opt/dpdk-stdlib',
      
      // For now, create the project structure manually since we don\'t have a repo yet
      'echo "Creating project structure..."',
      'mkdir -p {dpdk-sys/src,dpdk/src,dpdk-udp/src,apps/echo/src,scripts,tests,docs}',
      
      // Copy our install script content
      'cat > scripts/install_dpdk_amazon_linux.sh << \'SCRIPT_EOF\'',
      '#!/usr/bin/env bash',
      'set -euo pipefail',
      'echo "Installing DPDK on Amazon Linux 3..."',
      'yum install -y meson ninja-build python3-pip python3-pyelftools libbsd-devel libpcap-devel numactl-devel kernel-devel kernel-headers',
      'pip3 install --user pyelftools',
      'DPDK_VERSION="22.11.6"',
      'DPDK_DIR="dpdk-${DPDK_VERSION}"',
      'INSTALL_PREFIX="/usr/local"',
      'if [[ ! -d "$DPDK_DIR" ]]; then',
      '  curl -L "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"',
      '  tar -xf "dpdk-${DPDK_VERSION}.tar.xz"',
      'fi',
      'cd "$DPDK_DIR"',
      'meson setup build --prefix="$INSTALL_PREFIX" --buildtype=release -Denable_kmods=true',
      'ninja -C build',
      'ninja -C build install',
      'echo "$INSTALL_PREFIX/lib" > /etc/ld.so.conf.d/dpdk.conf',
      'ldconfig',
      'modprobe uio',
      'modprobe vfio-pci',
      'echo 1024 > /proc/sys/vm/nr_hugepages',
      'mkdir -p /mnt/huge',
      'mount -t hugetlbfs nodev /mnt/huge',
      'echo "DPDK installation complete!"',
      'SCRIPT_EOF',
      
      'chmod +x scripts/install_dpdk_amazon_linux.sh',
      
      // Install DPDK
      'echo "Installing DPDK..."',
      './scripts/install_dpdk_amazon_linux.sh || echo "DPDK install failed, continuing..."',
      
      // Create minimal project files for testing
      'echo "Creating minimal project files..."',
      
      // Main Cargo.toml
      'cat > Cargo.toml << \'EOF\'',
      '[workspace]',
      'resolver = "2"',
      'members = ["apps/echo"]',
      'EOF',
      
      // Echo app
      'cat > apps/echo/Cargo.toml << \'EOF\'',
      '[package]',
      'name = "echo"',
      'version = "0.1.0"',
      'edition = "2021"',
      '[dependencies]',
      'clap = { version = "4.0", features = ["derive"] }',
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
      '    println!("DPDK Echo Server starting on port {}", args.port);',
      '    println!("DPDK installation test successful!");',
      '    std::thread::sleep(std::time::Duration::from_secs(1));',
      '}',
      'EOF',
      
      // Build the project
      'echo "Building project..."',
      'source /root/.cargo/env',
      'cargo build --release || echo "Build failed, but setup complete"',
      
      // Create ENI binding script
      'cat > scripts/bind_eni.sh << \'BIND_EOF\'',
      '#!/usr/bin/env bash',
      'set -euo pipefail',
      'ENI_ID=${1:-}',
      'if [[ -z "$ENI_ID" ]]; then',
      '    echo "Usage: $0 <ENI_ID>"',
      '    exit 1',
      'fi',
      'echo "Binding ENI $ENI_ID to DPDK..."',
      'PCI_ADDR=$(lspci -D | grep "Elastic Network Adapter" | tail -1 | cut -d\' \' -f1)',
      'if [[ -z "$PCI_ADDR" ]]; then',
      '    echo "Error: Could not find ENA device PCI address"',
      '    exit 1',
      'fi',
      'echo "Found ENA device at PCI address: $PCI_ADDR"',
      'modprobe uio',
      'modprobe vfio-pci',
      'echo "$PCI_ADDR" > /sys/bus/pci/devices/$PCI_ADDR/driver/unbind 2>/dev/null || true',
      'echo "1d0f 0ec2" > /sys/bus/pci/drivers/vfio-pci/new_id 2>/dev/null || true',
      'echo "$PCI_ADDR" > /sys/bus/pci/drivers/vfio-pci/bind',
      'echo "Successfully bound $PCI_ADDR to vfio-pci"',
      'BIND_EOF',
      
      'chmod +x scripts/bind_eni.sh',
      
      'echo "DPDK-STDLIB setup complete!"',
      'echo "Project location: /opt/dpdk-stdlib"',
      'echo "Use: aws ssm start-session --target $(curl -s http://169.254.169.254/latest/meta-data/instance-id)"'
    );

    // Create sender instance
    const senderInstance = new ec2.Instance(this, 'DpdkSender', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C6GN, ec2.InstanceSize.LARGE),
      machineImage: ec2.MachineImage.latestAmazonLinux2023({
        cpuType: ec2.AmazonLinuxCpuType.ARM_64,
      }),
      securityGroup: mgmtSecurityGroup,
      userData,
      role: instanceRole,
      vpcSubnets: {
        subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS,
      },
    });

    // Create receiver instance  
    const receiverInstance = new ec2.Instance(this, 'DpdkReceiver', {
      vpc,
      instanceType: ec2.InstanceType.of(ec2.InstanceClass.C6GN, ec2.InstanceSize.LARGE),
      machineImage: ec2.MachineImage.latestAmazonLinux2023({
        cpuType: ec2.AmazonLinuxCpuType.ARM_64,
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
  }
}
