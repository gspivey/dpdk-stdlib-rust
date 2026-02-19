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
        'mkdir -p /opt/dpdk-stdlib',
        'unzip -q /tmp/dpdk-stdlib.zip -d /opt/dpdk-stdlib',
        'chown -R root:root /opt/dpdk-stdlib',
        'cd /opt/dpdk-stdlib',
      ];



      // Build the project
      const buildProject = [
        'echo "=== Building project ==="',
        'export HOME=/root',
        'source /root/.cargo/env',
        'PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release',
        'echo "=== Build complete ==="',
        'ls -la target/release/echo target/release/test-client',
        'echo "=== Setup complete! ==="',
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
          ...buildProject,
          ...cfnSignal,
        );
      } else {
        ud.addCommands(
          ...preamble,
          ...fullBootstrap,
          ...runtimeConfig,
          ...projectSetup,
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
