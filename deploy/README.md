# DPDK-STDLIB Deployment

AWS CDK infrastructure for testing DPDK-STDLIB with sender/receiver instances.

## Prerequisites

1. **AWS CLI** configured with appropriate credentials
2. **Node.js and npm** installed  
3. **AWS CDK CLI** installed: `npm install -g aws-cdk`
4. **Session Manager plugin** for AWS CLI:

### Installing Session Manager Plugin

**macOS:**
```bash
curl "https://s3.amazonaws.com/session-manager-downloads/plugin/latest/mac/sessionmanager-bundle.zip" -o "sessionmanager-bundle.zip"
unzip sessionmanager-bundle.zip
sudo ./sessionmanager-bundle/install -i /usr/local/sessionmanagerplugin -b /usr/local/bin/session-manager-plugin
rm -rf sessionmanager-bundle.zip sessionmanager-bundle/
```

**Linux:**
```bash
curl "https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_64bit/session-manager-plugin.rpm" -o "session-manager-plugin.rpm"
sudo yum install -y session-manager-plugin.rpm
rm session-manager-plugin.rpm
```

**Windows:**
Download and run the installer from: https://s3.amazonaws.com/session-manager-downloads/plugin/latest/windows/SessionManagerPluginSetup.exe

## Quick Start

```bash
cd deploy/cdk
npm install
cdk bootstrap  # First time only
cdk deploy --profile your-aws-profile
```

## Architecture

- **2 EC2 Instances**: Sender and Receiver (c6gn.large)
- **Dual ENIs**: Primary for SSM access, secondary for DPDK
- **Private Subnets**: No public IPs, access via SSM only
- **Security Groups**: Separate for management and DPDK traffic

## Testing

After deployment:

1. **Connect to receiver**:
   ```bash
   aws ssm start-session --target <RECEIVER_INSTANCE_ID> --profile your-aws-profile
   ```

2. **Start echo server** (on receiver):
   ```bash
   sudo su -
   cd /opt/dpdk-stdlib
   # Bind secondary ENI to DPDK
   ./scripts/bind_eni.sh <RECEIVER_DPDK_ENI_ID>
   # Start echo server with DPDK
   cargo run -p echo -- --dpdk-args="-l 0-1 -n 4"
   ```

3. **Connect to sender** (new terminal):
   ```bash
   aws ssm start-session --target <SENDER_INSTANCE_ID> --profile your-aws-profile
   ```

4. **Send test traffic** (on sender):
   ```bash
   sudo su -
   cd /opt/dpdk-stdlib
   # Bind secondary ENI to DPDK  
   ./scripts/bind_eni.sh <SENDER_DPDK_ENI_ID>
   # Send test packets
   echo "hello dpdk" | nc -u <RECEIVER_DPDK_IP> 9000
   ```

## ENI Management

Each instance has:
- **eth0**: Primary ENI for SSM (stays with kernel)
- **eth1**: Secondary ENI for DPDK (bind to vfio-pci)

The CDK outputs provide ENI IDs for binding scripts.

## Troubleshooting

### User Data Script Issues
If deployment fails, check console output:
```bash
aws ec2 get-console-output --instance-id <INSTANCE_ID> --profile your-aws-profile
```

### SSM Connection Issues
- Ensure Session Manager plugin is installed
- Verify AWS profile has SSM permissions
- Check instance is in private subnet with NAT gateway

## Cleanup

```bash
cdk destroy --profile your-aws-profile
```

## Cost Estimate

- 2x c6gn.large: ~$0.30/hour
- NAT Gateway: ~$0.045/hour  
- **Total**: ~$8.28/day

## Security

- No public IPs or SSH keys
- Access only via SSM (IAM controlled)
- DPDK traffic isolated to private subnet
- Management traffic separate from test traffic
