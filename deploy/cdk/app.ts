#!/usr/bin/env node
import 'source-map-support/register';
import * as cdk from 'aws-cdk-lib';
import * as ec2 from 'aws-cdk-lib/aws-ec2';
import { DpdkTestStack } from './lib/dpdk-test-stack';
import { PerfTestStack } from './lib/perf-test-stack';

const app = new cdk.App();

const env = {
  account: process.env.CDK_DEFAULT_ACCOUNT,
  region: process.env.CDK_DEFAULT_REGION || 'us-east-1',
};

// ── x86_64 stacks (default) ──────────────────────────────────────────────────
new DpdkTestStack(app, 'DpdkTestStack', { env });
new PerfTestStack(app, 'PerfTestStack', { env });

// ── Graviton (arm64) stacks ──────────────────────────────────────────────────
// TRex stays on x86_64 in PerfTestStackGraviton — TRex does not support ARM.
new DpdkTestStack(app, 'DpdkTestStackGraviton', {
  env,
  instanceClass:    ec2.InstanceClass.C7G,
  cpuType:          ec2.AmazonLinuxCpuType.ARM_64,
  amiContextKey:    'gravitonAmiId',
  ssmAgentRpmArch:  'linux_arm64',
});

new PerfTestStack(app, 'PerfTestStackGraviton', {
  env,
  dutInstanceClass:   ec2.InstanceClass.C7G,
  dutCpuType:         ec2.AmazonLinuxCpuType.ARM_64,
  dutAmiContextKey:   'gravitonDpdkAmiId',
  dutSsmAgentRpmArch: 'linux_arm64',
});
