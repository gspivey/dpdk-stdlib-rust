#!/usr/bin/env python3
"""check-cfn-signals.py - Verify cfn-signal resource names match CloudFormation logical IDs.

WHY THIS EXISTS
---------------
CloudFormation's CreationPolicy.ResourceSignal requires cfn-signal to send a signal
to the *exact* CloudFormation logical ID of the resource being created.

CDK generates logical IDs with hash suffixes by default (e.g., DpdkSender0BAA6CA3).
If user-data calls `cfn-signal --resource DpdkSender` but the logical ID is
`DpdkSender0BAA6CA3`, CloudFormation waits the full creation timeout (PT10M-PT35M)
before failing — an expensive, opaque failure mode that produces only:

    Failed to receive 1 resource signal(s) within the specified duration

The fix is `cfnInstance.overrideLogicalId('DpdkSender')` in dpdk-test-stack.ts,
which forces CDK to use the predictable name. This script verifies the contract
is correct after every CDK change, at synth time (seconds) rather than deploy time.

WHAT IT CHECKS
--------------
For each resource in the synthesized template that has a CreationPolicy.ResourceSignal,
it verifies that the resource's UserData contains:

    cfn-signal --resource <logical-id>

where <logical-id> is the CloudFormation resource name (the JSON key, not the CDK
construct ID).

HOW IT HANDLES CDK'S COMPLEX USERDDATA
---------------------------------------
CDK serializes UserData as Fn::Base64 { Fn::Join ["", [...]] } where the array
may contain a mix of plain strings and CloudFormation intrinsic functions
(e.g., Fn::Sub for S3 asset URLs). This script recursively extracts all plain
string values and searches them, ignoring unresolvable intrinsics.

If no cfn-signal call is found in the resolvable strings, a warning is printed
(not an error) because the call may be inside an intrinsic we can't resolve at
synth time. An error is only raised if we can clearly see a WRONG resource name.

KNOWN LIMITATIONS
-----------------
- Only validates static strings. If cfn-signal is inside a Fn::Sub that contains
  both literal text and CloudFormation references, we only see the literal parts.
- Does not validate the --stack or --region arguments.
- The check is specific to the pattern `cfn-signal --resource <name>`.

Usage: check-cfn-signals.py <synthesized-template.json>
"""

import json
import re
import sys
from typing import Any


def extract_strings(obj: Any) -> list[str]:
    """Recursively extract all plain string values from a nested JSON structure.

    This handles CDK's Fn::Join / Fn::Base64 / Fn::Sub structures by collecting
    every string leaf, regardless of nesting depth.
    """
    strings = []
    if isinstance(obj, str):
        strings.append(obj)
    elif isinstance(obj, dict):
        for v in obj.values():
            strings.extend(extract_strings(v))
    elif isinstance(obj, list):
        for item in obj:
            strings.extend(extract_strings(item))
    return strings


def check_cfn_signals(template_path: str) -> bool:
    """Return True if all checks pass, False if any error is found."""
    with open(template_path) as f:
        template = json.load(f)

    resources = template.get("Resources", {})
    errors: list[str] = []
    warnings: list[str] = []
    checked = 0

    for logical_id, resource in resources.items():
        creation_policy = resource.get("CreationPolicy", {})
        if "ResourceSignal" not in creation_policy:
            continue

        checked += 1
        print(f"  Checking: {logical_id}")

        user_data = resource.get("Properties", {}).get("UserData", {})
        if not user_data:
            warnings.append(
                f"{logical_id}: has ResourceSignal but no UserData found in template"
            )
            continue

        # Collect all resolvable string values from UserData
        all_strings = extract_strings(user_data)
        combined = "\n".join(all_strings)

        # Look for the exact expected pattern
        expected_pattern = rf"cfn-signal[^\n]*--resource\s+{re.escape(logical_id)}(\s|$)"
        if re.search(expected_pattern, combined):
            print(f"    ✓ cfn-signal correctly targets '{logical_id}'")
            continue

        # Not found with correct name — check what name IS used (better error message)
        wrong_resource_match = re.search(
            r"cfn-signal[^\n]*--resource\s+(\S+)", combined
        )
        if wrong_resource_match:
            actual = wrong_resource_match.group(1)
            errors.append(
                f"MISMATCH: Resource '{logical_id}' has ResourceSignal, but cfn-signal\n"
                f"  uses '--resource {actual}' instead of '--resource {logical_id}'.\n"
                f"\n"
                f"  Root cause: CDK generates logical IDs with hash suffixes unless\n"
                f"  overrideLogicalId() is called. The cfn-signal call targets the\n"
                f"  CDK construct name ('{actual}') but CloudFormation expects the\n"
                f"  actual logical ID ('{logical_id}').\n"
                f"\n"
                f"  Fix: In dpdk-test-stack.ts, after getting the CfnInstance:\n"
                f"    cfnInstance.overrideLogicalId('{actual}')\n"
                f"  This makes the logical ID '{actual}' match the cfn-signal target."
            )
        elif "cfn-signal" not in combined:
            # No cfn-signal at all in resolvable strings — may be in an intrinsic
            warnings.append(
                f"{logical_id}: no cfn-signal call found in resolvable UserData strings.\n"
                f"  This may be inside a CloudFormation intrinsic (Fn::Sub, etc.) that\n"
                f"  this validator cannot resolve at synth time. Verify manually or\n"
                f"  ensure cfn-signal uses literal string arguments."
            )
        else:
            # cfn-signal is present but --resource arg is unclear
            warnings.append(
                f"{logical_id}: cfn-signal found but could not parse --resource argument.\n"
                f"  Inspect the synthesized template manually: {template_path}"
            )

    if checked == 0:
        print("  No resources with ResourceSignal found — nothing to validate.")
        return True

    if warnings:
        print("\nWARNINGS (may be validator limitations, not necessarily bugs):")
        for w in warnings:
            for line in w.splitlines():
                print(f"  ⚠  {line}")

    if errors:
        print("\nERRORS:")
        for e in errors:
            for line in e.splitlines():
                print(f"  ✗ {line}")
        print(
            f"\n{len(errors)} cfn-signal validation error(s) found.\n"
            f"See scripts/integration-tests/DEBUGGING.md for context."
        )
        return False

    print(f"\nAll {checked} ResourceSignal resource(s) have correct cfn-signal targets. ✓")
    return True


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"Usage: {sys.argv[0]} <synthesized-template.json>", file=sys.stderr)
        sys.exit(1)
    ok = check_cfn_signals(sys.argv[1])
    sys.exit(0 if ok else 1)
