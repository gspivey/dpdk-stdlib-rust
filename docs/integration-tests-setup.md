# EC2 Integration Tests Setup Guide

This guide covers how to run the automated EC2 integration tests for dpdk-stdlib-rust locally from your machine and automatically on GitHub Actions PRs.

## Overview

The integration test pipeline:
1. Deploys two c5n.large EC2 instances via CDK (dual ENIs each)
2. Waits for SSM readiness and build verification
3. Runs test tiers (DPDK-to-DPDK echo, DPDK-to-iperf3 interop)
4. Collects JUnit XML results and prints a summary
5. Optionally tears down infrastructure

**Estimated cost**: ~$0.35/hour (~$8.28/day) while instances are running.

---

## 1. Local Developer Setup

### Prerequisites

| Tool | Install |
|------|---------|
| AWS CLI v2 | [Install guide](https://docs.aws.amazon.com/cli/latest/userguide/getting-started-install.html) |
| Node.js 20+ | `brew install node` or [nodejs.org](https://nodejs.org) |
| AWS CDK CLI | `npm install -g aws-cdk` |
| Session Manager plugin | See [below](#session-manager-plugin) |
| Python 3 | Required by the orchestrator for JSON summary generation |

#### Session Manager Plugin

**macOS:**
```bash
curl "https://s3.amazonaws.com/session-manager-downloads/plugin/latest/mac/sessionmanager-bundle.zip" -o "sessionmanager-bundle.zip"
unzip sessionmanager-bundle.zip
sudo ./sessionmanager-bundle/install -i /usr/local/sessionmanagerplugin -b /usr/local/bin/session-manager-plugin
rm -rf sessionmanager-bundle.zip sessionmanager-bundle/
```

**Linux (Debian/Ubuntu):**
```bash
curl "https://s3.amazonaws.com/session-manager-downloads/plugin/latest/ubuntu_64bit/session-manager-plugin.deb" -o "session-manager-plugin.deb"
sudo dpkg -i session-manager-plugin.deb
```

**Linux (Amazon Linux/RHEL):**
```bash
curl "https://s3.amazonaws.com/session-manager-downloads/plugin/latest/linux_64bit/session-manager-plugin.rpm" -o "session-manager-plugin.rpm"
sudo yum install -y session-manager-plugin.rpm
```

### AWS Credentials Setup

Create (or add to) `~/.aws/config`:

```ini
[profile dpdk-test]
region = us-east-1
output = json
```

And `~/.aws/credentials`:

```ini
[dpdk-test]
aws_access_key_id = AKIA...
aws_secret_access_key = ...
```

Or if your org uses SSO:

```ini
[profile dpdk-test]
sso_start_url = https://your-org.awsapps.com/start
sso_region = us-east-1
sso_account_id = 123456789012
sso_role_name = YourRoleName
region = us-east-1
```

Then login: `aws sso login --profile dpdk-test`

### Required IAM Permissions

The AWS profile needs these permissions. A minimal IAM policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "CDKDeploy",
      "Effect": "Allow",
      "Action": [
        "cloudformation:*",
        "ec2:*",
        "iam:CreateRole",
        "iam:DeleteRole",
        "iam:AttachRolePolicy",
        "iam:DetachRolePolicy",
        "iam:PutRolePolicy",
        "iam:DeleteRolePolicy",
        "iam:GetRole",
        "iam:PassRole",
        "iam:CreateInstanceProfile",
        "iam:DeleteInstanceProfile",
        "iam:AddRoleToInstanceProfile",
        "iam:RemoveRoleFromInstanceProfile",
        "iam:GetInstanceProfile",
        "iam:TagRole",
        "iam:TagInstanceProfile"
      ],
      "Resource": "*"
    },
    {
      "Sid": "CDKAssets",
      "Effect": "Allow",
      "Action": [
        "s3:CreateBucket",
        "s3:PutObject",
        "s3:GetObject",
        "s3:ListBucket",
        "s3:DeleteObject",
        "s3:DeleteBucket",
        "s3:GetBucketLocation",
        "s3:PutBucketPolicy",
        "s3:GetBucketPolicy"
      ],
      "Resource": "arn:aws:s3:::cdk-*"
    },
    {
      "Sid": "SSMExecution",
      "Effect": "Allow",
      "Action": [
        "ssm:SendCommand",
        "ssm:GetCommandInvocation",
        "ssm:DescribeInstanceInformation",
        "ssm:StartSession",
        "ssm:TerminateSession"
      ],
      "Resource": "*"
    }
  ]
}
```

> **Note**: For a quick start in a personal account, the `AdministratorAccess` managed policy works. Narrow permissions before sharing the profile or using in production.

### Bootstrap CDK (First Time Only)

```bash
cd deploy/cdk
npm install
npx cdk bootstrap --profile dpdk-test
```

### Running Integration Tests Locally

**Run all tiers with teardown:**
```bash
./scripts/run-integration-tests.sh dpdk-test --teardown
```

**Run only Tier 1 (DPDK-to-DPDK echo):**
```bash
./scripts/run-integration-tests.sh dpdk-test --tier 1 --teardown
```

**Run only Tier 3 (DPDK-to-iperf3 interop):**
```bash
./scripts/run-integration-tests.sh dpdk-test --tier 3 --teardown
```

**Keep infrastructure alive for debugging (no teardown):**
```bash
./scripts/run-integration-tests.sh dpdk-test
# ... run tests, debug via SSM ...
# When done, tear down manually:
cd deploy/cdk && npx cdk destroy DpdkTestStack --profile dpdk-test
```

**Re-run on existing infrastructure (skip deploy):**
```bash
./scripts/run-integration-tests.sh dpdk-test --skip-deploy --tier 1
```

**Generate JSON summary for agent consumption:**
```bash
./scripts/run-integration-tests.sh dpdk-test --teardown --json-summary
cat test-results/summary.json
```

### Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All tests passed |
| 1 | One or more tests failed |
| 2 | Infrastructure/setup failure (CDK deploy, SSM timeout, etc.) |

### Test Results

Results are collected into `test-results/` at the repo root:

```
test-results/
  tier1-dpdk-echo.xml          # Tier 1 JUnit XML
  tier3-our-app-sends.xml      # Tier 3 direction 1
  tier3-iperf-sends.xml        # Tier 3 direction 2
  summary.json                 # (if --json-summary)
```

---

## 2. GitHub Actions Setup (PR Integration Tests)

The workflow at `.github/workflows/integration-tests.yml` runs integration tests on PRs to main and on manual dispatch. It uses OIDC (OpenID Connect) for keyless AWS authentication.

### Step 1: Create an OIDC Identity Provider in AWS

In the AWS Console (IAM > Identity providers > Add provider):

- **Provider type**: OpenID Connect
- **Provider URL**: `https://token.actions.githubusercontent.com`
- **Audience**: `sts.amazonaws.com`

Or via CLI:
```bash
aws iam create-open-id-connect-provider \
  --url https://token.actions.githubusercontent.com \
  --client-id-list sts.amazonaws.com \
  --thumbprint-list 6938fd4d98bab03faadb97b34396831e3780aea1
```

### Step 2: Create an IAM Role for GitHub Actions

Create a role with a trust policy that restricts to your repo:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::ACCOUNT_ID:oidc-provider/token.actions.githubusercontent.com"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "token.actions.githubusercontent.com:aud": "sts.amazonaws.com"
        },
        "StringLike": {
          "token.actions.githubusercontent.com:sub": "repo:YOUR_ORG/dpdk-stdlib-rust:*"
        }
      }
    }
  ]
}
```

Replace:
- `ACCOUNT_ID` with your AWS account ID
- `YOUR_ORG/dpdk-stdlib-rust` with your GitHub org/repo

Attach the same IAM permissions policy from the [Required IAM Permissions](#required-iam-permissions) section above.

Via CLI:
```bash
# Save the trust policy to a file
cat > /tmp/trust-policy.json << 'EOF'
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": {
        "Federated": "arn:aws:iam::ACCOUNT_ID:oidc-provider/token.actions.githubusercontent.com"
      },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "token.actions.githubusercontent.com:aud": "sts.amazonaws.com"
        },
        "StringLike": {
          "token.actions.githubusercontent.com:sub": "repo:YOUR_ORG/dpdk-stdlib-rust:*"
        }
      }
    }
  ]
}
EOF

# Create the role
aws iam create-role \
  --role-name dpdk-stdlib-integration-tests \
  --assume-role-policy-document file:///tmp/trust-policy.json

# Attach permissions (use the managed policy or your custom one)
aws iam attach-role-policy \
  --role-name dpdk-stdlib-integration-tests \
  --policy-arn arn:aws:iam::ACCOUNT_ID:policy/DpdkIntegrationTestPolicy
```

### Step 3: Bootstrap CDK in the Target Account

The GitHub Actions runner needs a bootstrapped CDK environment:

```bash
npx cdk bootstrap aws://ACCOUNT_ID/us-east-1 --profile dpdk-test
```

### Step 4: Add the Repository Secret

In your GitHub repo: **Settings > Secrets and variables > Actions > New repository secret**

- **Name**: `AWS_ROLE_ARN`
- **Value**: `arn:aws:iam::ACCOUNT_ID:role/dpdk-stdlib-integration-tests`

### Step 5: Verify

Create a PR or trigger the workflow manually:

- Go to **Actions > Integration Tests > Run workflow**
- Select the branch and click **Run workflow**

The workflow will:
1. Check out code
2. Assume the OIDC role (no stored AWS keys)
3. Install CDK, Node.js, Session Manager plugin
4. Run `./scripts/run-integration-tests.sh default --teardown --json-summary`
5. Upload JUnit XML artifacts
6. Publish results in the PR checks UI via `dorny/test-reporter`
7. Run safety-net teardown if the orchestrator crashes

### Workflow Triggers

The current triggers are:

```yaml
on:
  pull_request:
    branches: [main]       # Runs on every PR to main
  workflow_dispatch: {}     # Manual trigger from Actions tab
```

Since all changes go through PRs, there is no mainline push trigger. This avoids redundant runs (the PR already validated the code). If you need to run on demand, use the manual `workflow_dispatch` trigger from the Actions tab.

### Viewing Results

1. Go to the PR **Checks** tab
2. View the **EC2 Integration Tests** check for JUnit results
3. Download the `integration-test-results` artifact for XML/JSON details

---

## Troubleshooting

### "CDK bootstrap required"

```
Error: This stack uses assets, so the toolkit stack must be deployed
```

Run: `npx cdk bootstrap aws://ACCOUNT_ID/REGION --profile dpdk-test`

### "SSM readiness timeout"

The instances take 15-20 minutes to install Rust, DPDK, and build the project. If SSM readiness times out:

- Check the user data log: `aws ec2 get-console-output --instance-id <ID> --profile dpdk-test`
- Increase `SSM_READINESS_TIMEOUT` in `scripts/run-integration-tests.sh` (default: 600s)

### "OIDC: Not authorized to perform sts:AssumeRoleWithWebIdentity"

- Verify the trust policy `sub` condition matches your repo: `repo:YOUR_ORG/dpdk-stdlib-rust:*`
- Verify the OIDC provider thumbprint is correct
- Verify the `AWS_ROLE_ARN` secret is set correctly in repo settings

### "Build not found on instance"

The CDK user data compiles the project on the instance. If the build fails:

```bash
# Connect via SSM and check logs
aws ssm start-session --target <INSTANCE_ID> --profile dpdk-test
sudo cat /var/log/user-data.log
```

### Orphaned infrastructure (costs money!)

If the orchestrator or workflow crashes without teardown:

```bash
cd deploy/cdk
npx cdk destroy DpdkTestStack --profile dpdk-test --force
```

The GitHub Actions workflow has a safety-net teardown step that runs even on failure, but always verify in the AWS Console (EC2 > Instances) that no instances are left running.

---

## Quick Reference

| Action | Command |
|--------|---------|
| Run all tests locally | `./scripts/run-integration-tests.sh dpdk-test --teardown` |
| Run Tier 1 only | `./scripts/run-integration-tests.sh dpdk-test --tier 1 --teardown` |
| Run Tier 3 only | `./scripts/run-integration-tests.sh dpdk-test --tier 3 --teardown` |
| Re-run on existing infra | `./scripts/run-integration-tests.sh dpdk-test --skip-deploy` |
| Get JSON summary | `./scripts/run-integration-tests.sh dpdk-test --teardown --json-summary` |
| Manual teardown | `cd deploy/cdk && npx cdk destroy DpdkTestStack --profile dpdk-test` |
| Trigger CI manually | GitHub Actions tab > Integration Tests > Run workflow |
| View CI results | PR checks > "EC2 Integration Tests" |
