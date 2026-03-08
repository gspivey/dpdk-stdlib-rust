#!/usr/bin/env node
import 'source-map-support/register';
import * as cdk from 'aws-cdk-lib';
import { DpdkTestStack } from './lib/dpdk-test-stack';
import { PerfTestStack } from './lib/perf-test-stack';

const app = new cdk.App();
new DpdkTestStack(app, 'DpdkTestStack', {
  env: {
    account: process.env.CDK_DEFAULT_ACCOUNT,
    region: process.env.CDK_DEFAULT_REGION || 'us-east-1',
  },
});

new PerfTestStack(app, 'PerfTestStack', {
  env: {
    account: process.env.CDK_DEFAULT_ACCOUNT,
    region: process.env.CDK_DEFAULT_REGION || 'us-east-1',
  },
});
