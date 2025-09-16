# DPDK-STDLIB Deployment

AWS CDK infrastructure for testing DPDK-STDLIB with sender/receiver instances.

## Prerequisites

1. AWS CLI configured with appropriate credentials
2. Node.js and npm installed  
3. AWS CDK CLI installed: `npm install -g aws-cdk`
4. Session Manager plugin: `aws ssm install-session-manager-plugin`

## Quick Start

```bash
cd deploy/cdk
npm install
cdk bootstrap  # First time only
cdk deploy
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
   aws ssm start-session --target <RECEIVER_INSTANCE_ID>
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
   aws ssm start-session --target <SENDER_INSTANCE_ID>
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

## Cleanup

```bash
cdk destroy
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
