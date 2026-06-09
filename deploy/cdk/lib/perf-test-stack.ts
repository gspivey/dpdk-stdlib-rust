import * as cdk from 'aws-cdk-lib';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import * as iam from 'aws-cdk-lib/aws-iam';
import * as s3assets from 'aws-cdk-lib/aws-s3-assets';
import { Construct } from 'constructs';

export interface PerfTestStackProps extends cdk.StackProps {
  /** EC2 instance class for the DUT. Default: C6IN */
  dutInstanceClass?: ec2.InstanceClass;
  /** EC2 instance size for the DUT. Default: XLARGE */
  dutInstanceSize?: ec2.InstanceSize;
  /** CPU architecture for the DUT stock AL2023 AMI fallback. Default: X86_64 */
  dutCpuType?: ec2.AmazonLinuxCpuType;
  /** CDK context key used to pass a pre-built DUT AMI ID. Default: 'dpdkAmiId' */
  dutAmiContextKey?: string;
  /** Architecture suffix for the DUT SSM agent RPM fallback URL. Default: 'linux_amd64' */
  dutSsmAgentRpmArch?: string;
}

/**
 * PerfTestStack deploys a TRex traffic generator and a DUT (Device Under Test)
 * instance for performance benchmarking of the dpdk-stdlib-rust UDP stack.
 *
 * Architecture:
 *   TRex (c6in.xlarge, x86_64)  <--UDP-->  DUT (configurable, default c6in.xlarge x86_64)
 *   ENI-0: mgmt/SSM                        ENI-0: mgmt/SSM
 *   ENI-1: DPDK traffic                    ENI-1: DPDK/kernel traffic
 *
 * TRex always runs on x86_64 (it does not support ARM).
 * Pass dutInstanceClass/dutCpuType to benchmark a Graviton DUT against an x86 TRex generator.
 */
export class PerfTestStack extends cdk.Stack {
  constructor(scope: Construct, id: string, props?: PerfTestStackProps) {
    super(scope, id, props);

    const dutInstanceClass    = props?.dutInstanceClass    ?? ec2.InstanceClass.C6IN;
    const dutInstanceSize     = props?.dutInstanceSize     ?? ec2.InstanceSize.XLARGE;
    const dutCpuType          = props?.dutCpuType          ?? ec2.AmazonLinuxCpuType.X86_64;
    const dutAmiContextKey    = props?.dutAmiContextKey    ?? 'dpdkAmiId';
    const dutSsmAgentRpmArch  = props?.dutSsmAgentRpmArch  ?? 'linux_amd64';

    // AMI IDs via CDK context
    const dpdkAmiId = this.node.tryGetContext(dutAmiContextKey);
    const trexAmiId = this.node.tryGetContext('trexAmiId');
    const usePrebuiltDpdkAmi = !!dpdkAmiId;
    const usePrebuiltTrexAmi = !!trexAmiId;

    // VPC with same topology as integration tests
    const vpc = new ec2.Vpc(this, 'PerfTestVpc', {
      maxAzs: 1,
      natGateways: 1,
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

    // Enable IPv6 on VPC for IPv6 perf tests
    const cfnVpc = vpc.node.defaultChild as ec2.CfnVPC;
    const ipv6Cidr = new ec2.CfnVPCCidrBlock(this, 'Ipv6Cidr', {
      vpcId: vpc.vpcId,
      amazonProvidedIpv6CidrBlock: true,
    });

    // Assign IPv6 CIDR to private subnet (used by data-plane ENIs)
    const privateSubnet = vpc.privateSubnets[0];
    const cfnPrivateSubnet = privateSubnet.node.defaultChild as ec2.CfnSubnet;
    cfnPrivateSubnet.ipv6CidrBlock = cdk.Fn.select(0, cdk.Fn.cidr(
      cdk.Fn.select(0, vpc.vpcIpv6CidrBlocks),
      2, // 2 subnets
      '64', // /64 subnets
    ));
    cfnPrivateSubnet.addDependency(ipv6Cidr);

    // VPC Interface Endpoints for SSM
    vpc.addInterfaceEndpoint('SsmEndpoint', {
      service: ec2.InterfaceVpcEndpointAwsService.SSM,
    });
    vpc.addInterfaceEndpoint('SsmMessagesEndpoint', {
      service: ec2.InterfaceVpcEndpointAwsService.SSM_MESSAGES,
    });
    vpc.addInterfaceEndpoint('Ec2MessagesEndpoint', {
      service: ec2.InterfaceVpcEndpointAwsService.EC2_MESSAGES,
    });

    // Security groups
    const mgmtSecurityGroup = new ec2.SecurityGroup(this, 'PerfMgmtSG', {
      vpc,
      description: 'Management security group for SSM access',
      allowAllOutbound: true,
    });

    const dataSecurityGroup = new ec2.SecurityGroup(this, 'PerfDataSG', {
      vpc,
      description: 'Data plane traffic between TRex and DUT',
      allowAllOutbound: true,
    });

    // Allow all UDP/TCP/ICMP between data plane interfaces
    dataSecurityGroup.addIngressRule(
      dataSecurityGroup,
      ec2.Port.allUdp(),
      'All UDP traffic between TRex and DUT'
    );
    dataSecurityGroup.addIngressRule(
      dataSecurityGroup,
      ec2.Port.allTcp(),
      'All TCP traffic between TRex and DUT'
    );
    dataSecurityGroup.addIngressRule(
      dataSecurityGroup,
      ec2.Port.allIcmp(),
      'ICMP traffic between TRex and DUT'
    );
    // IPv6 ICMP (NDP, etc.)
    dataSecurityGroup.addIngressRule(
      dataSecurityGroup,
      ec2.Port.allTraffic(),
      'All IPv6 traffic between TRex and DUT (NDP + data)'
    );
    // Allow traffic from mgmt to data plane (for kernel-mode tests)
    dataSecurityGroup.addIngressRule(
      mgmtSecurityGroup,
      ec2.Port.allUdp(),
      'UDP from management interfaces'
    );

    // Bundle project as S3 asset (for DUT)
    const projectAsset = new s3assets.Asset(this, 'PerfTestProject', {
      path: '../../',
      exclude: [
        'target/**',
        '.git/**',
        'deploy/**',
        '*.log',
        'node_modules/**',
        '*.md',
        '.gitignore',
        '.vscode/**',
        '.idea/**',
      ],
    });

    // IAM role for both instances
    const instanceRole = new iam.Role(this, 'PerfTestInstanceRole', {
      assumedBy: new iam.ServicePrincipal('ec2.amazonaws.com'),
      managedPolicies: [
        iam.ManagedPolicy.fromAwsManagedPolicyName('AmazonSSMManagedInstanceCore'),
      ],
    });
    projectAsset.grantRead(instanceRole);

    const trexInstanceType = ec2.InstanceType.of(ec2.InstanceClass.C6IN, ec2.InstanceSize.XLARGE);
    const dutInstanceType  = ec2.InstanceType.of(dutInstanceClass, dutInstanceSize);

    // ── TRex Instance (always x86_64 — TRex does not support ARM) ───────────

    const trexMachineImage = usePrebuiltTrexAmi
      ? ec2.MachineImage.genericLinux({ [this.region]: trexAmiId })
      : ec2.MachineImage.latestAmazonLinux2023({
          cpuType: ec2.AmazonLinuxCpuType.X86_64,
        });

    const trexCreationTimeout = usePrebuiltTrexAmi ? 'PT15M' : 'PT30M';

    const trexUserData = ec2.UserData.forLinux();
    const trexPreamble = [
      'exec > >(tee /var/log/user-data.log) 2>&1',
      'echo "=== TRex user-data starting at $(date -u) ==="',
      'dnf install -y aws-cfn-bootstrap 2>/dev/null || echo "cfn-bootstrap already present"',
      `trap 'CFN_EXIT=$?; CFN_REASON=$(tail -3 /var/log/user-data.log 2>/dev/null | tr "\\n" " " | cut -c1-200); /opt/aws/bin/cfn-signal -e $CFN_EXIT --reason "$CFN_REASON" --stack ${this.stackName} --resource TrexInstance --region ${this.region} 2>/dev/null || true' EXIT`,
      'set -euo pipefail',
    ];

    const trexPrebuiltPreamble = [
      'echo "=== Using pre-built TRex AMI ==="',
      'if ! rpm -q amazon-ssm-agent >/dev/null 2>&1; then dnf install -y amazon-ssm-agent; fi',
      'systemctl stop amazon-ssm-agent 2>/dev/null || true',
      'rm -rf /var/lib/amazon/ssm/ipc/ /var/lib/amazon/ssm/Vault/ /var/lib/amazon/ssm/registration',
      'systemctl enable amazon-ssm-agent 2>/dev/null || true',
      'systemctl start amazon-ssm-agent 2>/dev/null || true',
    ];

    const trexFullBootstrap = [
      'echo "=== Installing TRex from scratch ==="',
      'dnf update -y',
      'dnf groupinstall -y "Development Tools"',
      'dnf install -y pciutils numactl numactl-devel python3 python3-pip amazon-ssm-agent',
      'systemctl enable amazon-ssm-agent',
      'cd /opt',
      'TREX_VERSION="v3.08"',
      'curl -fL --retry 3 --retry-delay 10 "https://trex-tgn.cisco.com/trex/release/${TREX_VERSION}.tar.gz" -o trex.tar.gz || curl -fLk --retry 3 --retry-delay 10 "https://trex-tgn.cisco.com/trex/release/${TREX_VERSION}.tar.gz" -o trex.tar.gz',
      'tar -xzf trex.tar.gz',
      'mv ${TREX_VERSION} trex',
      'rm -f trex.tar.gz',
      'pip3 install PyYAML scapy || pip3 install --break-system-packages PyYAML scapy',
    ];

    const trexRuntimeConfig = [
      'echo "=== Configuring TRex runtime ==="',
      'modprobe vfio-pci || echo "vfio-pci already loaded"',
      'echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode || echo "noiommu already set"',
      'echo 1024 > /proc/sys/vm/nr_hugepages',
      'mkdir -p /mnt/huge',
      'mount -t hugetlbfs nodev /mnt/huge || echo "hugepages already mounted"',
      // Wait for secondary ENI — attachment is a separate CloudFormation resource,
      // so it may not be ready during instance boot. Make this non-fatal; the test
      // orchestrator configures and starts TRex via SSM after CFN deploy completes.
      // Note: TRex AMI does NOT have dpdk-devbind.py — TRex binds the NIC itself.
      'echo "=== Waiting for secondary ENI (best-effort) ==="',
      'ENI_FOUND=false',
      'for i in {1..180}; do',
      '  TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600")',
      '  MACS=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/)',
      '  for mac in $MACS; do',
      '    DEVICE_NUM=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}device-number)',
      '    if [ "$DEVICE_NUM" = "1" ]; then',
      '      echo "Found secondary ENI at device-number 1 (MAC: ${mac})"',
      '      ENI_FOUND=true',
      '      break 2',
      '    fi',
      '  done',
      '  if [ $((i % 30)) -eq 0 ]; then echo "Attempt $i: waiting for secondary ENI..."; fi',
      '  sleep 1',
      'done',
      'if [ "$ENI_FOUND" = "false" ]; then echo "WARNING: Secondary ENI not found during boot — orchestrator will handle via SSM"; fi',
      // Collect environment info
      'echo "=== TRex Environment ==="',
      'echo "Instance type: $(curl -s -H \"X-aws-ec2-metadata-token: $(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600)\" http://169.254.169.254/latest/meta-data/instance-type)"',
      'echo "Hugepages: $(cat /proc/meminfo | grep HugePages_Total)"',
      'echo "CPUs: $(nproc)"',
      'echo "Kernel: $(uname -r)"',
      'lspci | grep -i eth || echo "No ethernet PCI devices"',
      'echo "=== TRex instance ready ==="',
    ];

    if (usePrebuiltTrexAmi) {
      trexUserData.addCommands(...trexPreamble, ...trexPrebuiltPreamble, ...trexRuntimeConfig);
    } else {
      trexUserData.addCommands(...trexPreamble, ...trexFullBootstrap, ...trexRuntimeConfig);
    }

    const trexInstance = new ec2.Instance(this, 'TrexInstance', {
      vpc,
      instanceType: trexInstanceType,
      machineImage: trexMachineImage,
      securityGroup: mgmtSecurityGroup,
      userData: trexUserData,
      role: instanceRole,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
    });

    const cfnTrexInstance = trexInstance.node.defaultChild as ec2.CfnInstance;
    cfnTrexInstance.overrideLogicalId('TrexInstance');
    cfnTrexInstance.cfnOptions.creationPolicy = {
      resourceSignal: { timeout: trexCreationTimeout, count: 1 },
    };

    // ── DUT Instance ─────────────────────────────────────────────────────────

    const dutMachineImage = usePrebuiltDpdkAmi
      ? ec2.MachineImage.genericLinux({ [this.region]: dpdkAmiId })
      : ec2.MachineImage.latestAmazonLinux2023({ cpuType: dutCpuType });

    const dutCreationTimeout = usePrebuiltDpdkAmi ? 'PT20M' : 'PT35M';

    const dutUserData = ec2.UserData.forLinux();
    const dutPreamble = [
      'exec > >(tee /var/log/user-data.log) 2>&1',
      'echo "=== DUT user-data starting at $(date -u) ==="',
      'dnf install -y aws-cfn-bootstrap 2>/dev/null || echo "cfn-bootstrap already present"',
      `trap 'CFN_EXIT=$?; CFN_REASON=$(tail -3 /var/log/user-data.log 2>/dev/null | tr "\\n" " " | cut -c1-200); /opt/aws/bin/cfn-signal -e $CFN_EXIT --reason "$CFN_REASON" --stack ${this.stackName} --resource DutInstance --region ${this.region} 2>/dev/null || true' EXIT`,
      'set -euo pipefail',
    ];

    const dutPrebuiltPreamble = [
      'echo "=== Using pre-built DPDK AMI for DUT ==="',
      `if ! rpm -q amazon-ssm-agent >/dev/null 2>&1; then dnf install -y amazon-ssm-agent 2>/dev/null || (curl -s https://s3.amazonaws.com/ec2-downloads-windows/SSMAgent/latest/${dutSsmAgentRpmArch}/amazon-ssm-agent.rpm -o /tmp/amazon-ssm-agent.rpm && rpm -ivh /tmp/amazon-ssm-agent.rpm); fi`,
      'systemctl stop amazon-ssm-agent 2>/dev/null || true',
      'rm -rf /var/lib/amazon/ssm/ipc/ /var/lib/amazon/ssm/Vault/ /var/lib/amazon/ssm/registration',
      'systemctl enable amazon-ssm-agent 2>/dev/null || true',
      'systemctl start amazon-ssm-agent 2>/dev/null || true',
      'dnf install -y clang-devel unzip 2>/dev/null || echo "packages already installed"',
    ];

    const dutFullBootstrap = [
      'echo "=== Full DUT bootstrap ==="',
      'dnf update -y',
      'dnf groupinstall -y "Development Tools"',
      'dnf install -y git pciutils iperf3 clang-devel unzip numactl --allowerasing',
      'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y',
      'export HOME=/root',
      'source /root/.cargo/env',
      'echo "export PATH=/root/.cargo/bin:$PATH" >> /etc/profile',
      'echo "export HOME=/root" >> /etc/profile',
      'dnf install -y meson ninja-build python3-pip libbsd-devel libpcap-devel numactl-devel kernel-devel kernel-headers --allowerasing',
      'pip3 install pyelftools || pip3 install --break-system-packages pyelftools',
      'cd /opt',
      'DPDK_VERSION="22.11.6"',
      'curl -L "https://fast.dpdk.org/rel/dpdk-${DPDK_VERSION}.tar.xz" -o "dpdk-${DPDK_VERSION}.tar.xz"',
      'tar -xf "dpdk-${DPDK_VERSION}.tar.xz"',
      'cd dpdk-stable-${DPDK_VERSION}',
      'meson setup build --prefix=/usr/local --libdir=lib --buildtype=release -Denable_kmods=false -Ddisable_drivers=net/gve,net/ionic',
      'ninja -C build',
      'ninja -C build install',
      'echo "/usr/local/lib" > /etc/ld.so.conf.d/dpdk.conf',
      'ldconfig',
    ];

    const dutRuntimeConfig = [
      'echo "=== Configuring DUT runtime ==="',
      'modprobe uio || echo "uio already loaded"',
      'modprobe vfio-pci || echo "vfio-pci already loaded"',
      'echo 1 > /sys/module/vfio/parameters/enable_unsafe_noiommu_mode || echo "noiommu already set"',
      'echo 1024 > /proc/sys/vm/nr_hugepages',
      'mkdir -p /mnt/huge',
      'mount -t hugetlbfs nodev /mnt/huge || echo "hugepages already mounted"',
      'ulimit -c unlimited',
      'mkdir -p /tmp/coredumps',
      'echo "/tmp/coredumps/core.%e.%p.%t" > /proc/sys/kernel/core_pattern',
      // Wait for secondary ENI — attachment is a separate CloudFormation resource,
      // so it may not be ready during instance boot. Make this non-fatal; the test
      // orchestrator handles binding via SSM after CFN deploy completes.
      'echo "=== Waiting for secondary ENI (best-effort, orchestrator handles binding) ==="',
      'ENI_FOUND=false',
      'for i in {1..180}; do',
      '  TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 21600")',
      '  MACS=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/)',
      '  for mac in $MACS; do',
      '    DEVICE_NUM=$(curl -s -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/network/interfaces/macs/${mac}device-number)',
      '    if [ "$DEVICE_NUM" = "1" ]; then',
      '      echo "Found secondary ENI at device-number 1"',
      '      ip link set ens6 down 2>/dev/null || true',
      '      /usr/local/bin/dpdk-devbind.py --bind=vfio-pci 0000:00:06.0 2>/dev/null || echo "devbind failed, orchestrator will handle"',
      '      /usr/local/bin/dpdk-devbind.py --status 2>/dev/null | head -10 || true',
      '      ENI_FOUND=true',
      '      break 2',
      '    fi',
      '  done',
      '  if [ $((i % 30)) -eq 0 ]; then echo "Attempt $i: waiting for secondary ENI..."; fi',
      '  sleep 1',
      'done',
      'if [ "$ENI_FOUND" = "false" ]; then echo "WARNING: Secondary ENI not found during boot — orchestrator will bind via SSM"; fi',
    ];

    const dutProjectSetup = [
      'echo "=== Downloading project ==="',
      `aws s3 cp ${projectAsset.s3ObjectUrl} /tmp/dpdk-stdlib.zip`,
      'mkdir -p /opt/dpdk-stdlib',
      'unzip -q /tmp/dpdk-stdlib.zip -d /opt/dpdk-stdlib',
      'chown -R root:root /opt/dpdk-stdlib',
    ];

    const dutBuildProject = [
      'echo "=== Building project ==="',
      'export HOME=/root',
      'source /root/.cargo/env',
      'cd /opt/dpdk-stdlib',
      'PKG_CONFIG_PATH=/usr/local/lib/pkgconfig pkg-config --modversion libdpdk',
      // Single workspace build with bindgen → produces real-DPDK binaries for echo,
      // tokio-echo (which has `dpdk` as a default feature), and plain-echo (no dpdk dep).
      'PKG_CONFIG_PATH=/usr/local/lib/pkgconfig cargo build --release --features dpdk-sys/bindgen',
      'echo "=== Build complete ==="',
      'ls -la target/release/echo target/release/plain-echo target/release/tokio-echo',
      // Collect environment info
      'echo "=== DUT Environment ==="',
      'echo "Instance type: $(curl -s -H \"X-aws-ec2-metadata-token: $(curl -s -X PUT http://169.254.169.254/latest/api/token -H X-aws-ec2-metadata-token-ttl-seconds:21600)\" http://169.254.169.254/latest/meta-data/instance-type)"',
      'echo "DPDK version: $(PKG_CONFIG_PATH=/usr/local/lib/pkgconfig pkg-config --modversion libdpdk)"',
      'echo "Rust version: $(rustc --version)"',
      'echo "Hugepages: $(cat /proc/meminfo | grep HugePages_Total)"',
      'echo "CPUs: $(nproc)"',
      'echo "Kernel: $(uname -r)"',
      'lspci | grep -i eth || echo "No ethernet PCI devices"',
      '/usr/local/bin/dpdk-devbind.py --status',
      'echo "=== DUT instance ready ==="',
    ];

    if (usePrebuiltDpdkAmi) {
      dutUserData.addCommands(
        ...dutPreamble, ...dutPrebuiltPreamble, ...dutRuntimeConfig,
        ...dutProjectSetup, ...dutBuildProject,
      );
    } else {
      dutUserData.addCommands(
        ...dutPreamble, ...dutFullBootstrap, ...dutRuntimeConfig,
        ...dutProjectSetup, ...dutBuildProject,
      );
    }

    const dutInstance = new ec2.Instance(this, 'DutInstance', {
      vpc,
      instanceType: dutInstanceType,
      machineImage: dutMachineImage,
      securityGroup: mgmtSecurityGroup,
      userData: dutUserData,
      role: instanceRole,
      vpcSubnets: { subnetType: ec2.SubnetType.PRIVATE_WITH_EGRESS },
    });

    const cfnDutInstance = dutInstance.node.defaultChild as ec2.CfnInstance;
    cfnDutInstance.overrideLogicalId('DutInstance');
    cfnDutInstance.cfnOptions.creationPolicy = {
      resourceSignal: { timeout: dutCreationTimeout, count: 1 },
    };

    // ── Secondary ENIs (data plane) ──────────────────────────────────────────

    // TRex needs 2 data ENIs: one for TX, one for RX (TRex requires port pairs).
    // Device index 1 = TX (ens6 / 0000:00:06.0), device index 2 = RX (ens7 / 0000:00:07.0).
    const trexDataEniTx = new ec2.CfnNetworkInterface(this, 'TrexDataEni', {
      subnetId: vpc.privateSubnets[0].subnetId,
      groupSet: [dataSecurityGroup.securityGroupId],
      description: 'TRex data plane TX interface',
      ipv6AddressCount: 1,
    });

    const trexDataEniRx = new ec2.CfnNetworkInterface(this, 'TrexDataEniRx', {
      subnetId: vpc.privateSubnets[0].subnetId,
      groupSet: [dataSecurityGroup.securityGroupId],
      description: 'TRex data plane RX interface',
      ipv6AddressCount: 1,
    });

    const dutDataEni = new ec2.CfnNetworkInterface(this, 'DutDataEni', {
      subnetId: vpc.privateSubnets[0].subnetId,
      groupSet: [dataSecurityGroup.securityGroupId],
      description: 'DUT data plane interface',
      ipv6AddressCount: 1,
    });

    // NOTE: ENA Express (SRD) requires MTU ≤ 8900 and c6in.8xlarge+.
    // Not enabled here — our jumbo MTU 9001 exceeds the ENA Express limit.
    // Future work: either cap MTU at 8900 or use multi-flow to reach 25+ Gbps.
    // See: https://github.com/amzn/amzn-ec2-ena-utilities/blob/main/ena-express/check-ena-express-settings.sh

    new ec2.CfnNetworkInterfaceAttachment(this, 'TrexDataAttachment', {
      instanceId: trexInstance.instanceId,
      networkInterfaceId: trexDataEniTx.ref,
      deviceIndex: '1',
    });

    new ec2.CfnNetworkInterfaceAttachment(this, 'TrexDataRxAttachment', {
      instanceId: trexInstance.instanceId,
      networkInterfaceId: trexDataEniRx.ref,
      deviceIndex: '2',
    });

    new ec2.CfnNetworkInterfaceAttachment(this, 'DutDataAttachment', {
      instanceId: dutInstance.instanceId,
      networkInterfaceId: dutDataEni.ref,
      deviceIndex: '1',
    });

    // ── Outputs ──────────────────────────────────────────────────────────────

    new cdk.CfnOutput(this, 'TrexInstanceId', {
      value: trexInstance.instanceId,
      description: 'TRex generator instance ID',
    });

    new cdk.CfnOutput(this, 'DutInstanceId', {
      value: dutInstance.instanceId,
      description: 'DUT instance ID',
    });

    new cdk.CfnOutput(this, 'TrexSSMCommand', {
      value: `aws ssm start-session --target ${trexInstance.instanceId}`,
    });

    new cdk.CfnOutput(this, 'DutSSMCommand', {
      value: `aws ssm start-session --target ${dutInstance.instanceId}`,
    });

    new cdk.CfnOutput(this, 'TrexDataEniId', {
      value: trexDataEniTx.ref,
      description: 'TRex data plane TX ENI ID',
    });

    new cdk.CfnOutput(this, 'TrexDataEniRxId', {
      value: trexDataEniRx.ref,
      description: 'TRex data plane RX ENI ID',
    });

    new cdk.CfnOutput(this, 'DutDataEniId', {
      value: dutDataEni.ref,
      description: 'DUT data plane ENI ID',
    });

    new cdk.CfnOutput(this, 'TrexDataEniPrivateIp', {
      value: trexDataEniTx.attrPrimaryPrivateIpAddress,
      description: 'TRex data plane TX ENI private IP',
    });

    new cdk.CfnOutput(this, 'TrexDataEniRxPrivateIp', {
      value: trexDataEniRx.attrPrimaryPrivateIpAddress,
      description: 'TRex data plane RX ENI private IP',
    });

    new cdk.CfnOutput(this, 'DutDataEniPrivateIp', {
      value: dutDataEni.attrPrimaryPrivateIpAddress,
      description: 'DUT data plane ENI private IP',
    });

    if (usePrebuiltDpdkAmi) {
      new cdk.CfnOutput(this, 'DpdkAmiId', {
        value: dpdkAmiId,
        description: 'Pre-built DPDK AMI ID used for DUT',
      });
    }

    if (usePrebuiltTrexAmi) {
      new cdk.CfnOutput(this, 'TrexAmiId', {
        value: trexAmiId,
        description: 'Pre-built TRex AMI ID used for generator',
      });
    }
  }
}
