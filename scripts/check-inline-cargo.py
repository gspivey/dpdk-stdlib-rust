#!/usr/bin/env python3
"""check-inline-cargo.py - Verify inline Cargo.toml files in CDK UserData match real crates.

WHY THIS EXISTS
---------------
The CDK stack writes inline Cargo.toml overrides into EC2 user-data via heredocs
(e.g., `cat > dpdk-udp/Cargo.toml << 'EOF' ... EOF`). These inline files can
drift from the real Cargo.toml files in the repository, causing compilation
failures in integration tests.

Example bugs this catches:
  - Missing `libc` dep: dpdk-udp uses libc::socklen_t, libc::c_void, etc.
    57 unresolved symbol errors at runtime, but not visible until EC2 build.
  - `dpdk = optional` when real crate requires it: 20+ import errors at runtime.

WHAT IT CHECKS
--------------
For each `cat > <path>/Cargo.toml << 'EOF' ... EOF` block found in the
synthesized UserData:

  1. If a real Cargo.toml exists at the same path in the project:
       a. Every [dependencies] entry in the REAL file must be present in
          the inline file (no missing packages).
       b. If a dep is non-optional in the real file, it must also be
          non-optional in the inline file (catches optional-vs-required drift).

Inline Cargo.toml files for new crates with no real counterpart (e.g.,
apps/peer-app) are skipped — they can't drift from a file that doesn't exist.

HOW IT WORKS
------------
CDK serializes UserData as either a plain string or a Fn::Base64{Fn::Join[...]}
structure. This script handles both by recursively extracting all plain string
values from the JSON, then joining and scanning for heredoc patterns.

KNOWN LIMITATIONS
-----------------
- Only validates [dependencies] section (not [dev-dependencies], [features], etc.)
- Does not validate workspace-level Cargo.toml since it intentionally differs
  (inline workspace omits dpdk-tokio and adds peer-app).
- TOML parsing is line-oriented regex, not a full TOML parser. Complex multi-line
  dependency specs are not supported (none exist in this project currently).

Usage: check-inline-cargo.py <synthesized-template.json> <project-root>
"""

import json
import re
import sys
from pathlib import Path
from typing import Any


def extract_strings(obj: Any) -> list[str]:
    """Recursively extract all plain string values from a nested JSON structure.

    Handles CDK's Fn::Join / Fn::Base64 / Fn::Sub structures by collecting
    every string leaf.
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


def get_user_data_text(resource: dict) -> str:
    """Extract all string content from a resource's UserData."""
    user_data = resource.get("Properties", {}).get("UserData", {})
    if not user_data:
        return ""
    strings = extract_strings(user_data)
    return "\n".join(strings)


def parse_deps(toml_text: str) -> dict[str, dict]:
    """Parse [dependencies] from TOML text.

    Returns {package_name: {optional: bool, raw: str}} for each entry.
    Only handles single-line dependency entries.
    """
    deps: dict[str, dict] = {}
    in_deps = False
    for line in toml_text.splitlines():
        stripped = line.strip()
        # Section header
        if stripped.startswith("["):
            in_deps = stripped == "[dependencies]"
            continue
        if not in_deps or not stripped or stripped.startswith("#"):
            continue
        if "=" not in stripped:
            continue
        name, _, value = stripped.partition("=")
        name = name.strip()
        if not name or "." in name:  # skip dotted keys like package.x = ...
            continue
        is_optional = "optional = true" in value
        deps[name] = {"optional": is_optional, "raw": stripped}
    return deps


def extract_inline_cargo_tomls(user_data_text: str) -> dict[str, str]:
    """Find all `cat > <path>/Cargo.toml << 'EOF' ... EOF` heredoc blocks.

    Returns {relative_path: toml_content}.
    """
    # Match heredoc: cat > <path> << 'EOF'\\n<content>\\nEOF
    # Single-quoted EOF prevents variable expansion in bash
    pattern = r"cat > ([\w./\-]+/Cargo\.toml) << 'EOF'\n(.*?)\nEOF"
    results = {}
    for m in re.finditer(pattern, user_data_text, re.DOTALL):
        rel_path = m.group(1)
        content = m.group(2)
        results[rel_path] = content
    return results


def extract_inline_source_crates(user_data_text: str) -> set[str]:
    """Find all crate directories whose source files are also replaced inline.

    When both Cargo.toml AND src/*.rs are replaced inline, the crate is a
    completely different app from the real one — dep comparison doesn't apply.

    Returns a set of crate directory prefixes (e.g., 'apps/echo').
    """
    src_pattern = r"cat > ([\w./\-]+)/src/\w+\.rs << 'EOF'"
    crates: set[str] = set()
    for m in re.finditer(src_pattern, user_data_text):
        crates.add(m.group(1))
    return crates


def check_cargo_deps(template_path: str, project_root: str) -> bool:
    """Return True if all checks pass, False if any error found."""
    with open(template_path) as f:
        template = json.load(f)

    resources = template.get("Resources", {})
    errors: list[str] = []
    checked: list[str] = []

    # Process each EC2 instance; deduplicate by inline path (sender/receiver
    # have identical user-data except for the cfn-signal resource name).
    seen_paths: set[str] = set()

    for logical_id, resource in resources.items():
        if resource.get("Type") != "AWS::EC2::Instance":
            continue

        user_data_text = get_user_data_text(resource)
        inline_tomls = extract_inline_cargo_tomls(user_data_text)
        # Crates where source is also replaced inline are completely different
        # apps — their Cargo.toml deps won't match the real crate's.
        source_replaced_crates = extract_inline_source_crates(user_data_text)

        for inline_path, inline_content in inline_tomls.items():
            if inline_path in seen_paths:
                continue
            seen_paths.add(inline_path)

            # Skip workspace root Cargo.toml — intentionally differs
            # (inline workspace has different members list)
            if inline_path == "Cargo.toml":
                continue

            real_path = Path(project_root) / inline_path
            if not real_path.exists():
                # New crate with no real counterpart (e.g., apps/peer-app)
                continue

            # Skip crates whose source is also replaced inline — they are
            # entirely different apps and dep comparison would be meaningless.
            crate_dir = str(Path(inline_path).parent)
            if crate_dir in source_replaced_crates:
                continue

            real_content = real_path.read_text()
            real_deps = parse_deps(real_content)
            inline_deps = parse_deps(inline_content)

            crate_errors: list[str] = []

            for name, real_info in real_deps.items():
                if name not in inline_deps:
                    crate_errors.append(
                        f"    MISSING: '{name}' is in real {inline_path} "
                        f"but absent from inline"
                    )
                elif not real_info["optional"] and inline_deps[name]["optional"]:
                    crate_errors.append(
                        f"    WRONG:   '{name}' is required in real {inline_path} "
                        f"but marked `optional = true` in inline"
                    )

            if crate_errors:
                errors.append(
                    f"Inline '{inline_path}' is missing or has wrong deps:\n"
                    + "\n".join(crate_errors)
                    + f"\n\n  Real deps:   {sorted(real_deps)}"
                    + f"\n  Inline deps: {sorted(inline_deps)}"
                    + f"\n\n  Fix: update the inline Cargo.toml in dpdk-test-stack.ts"
                )
            else:
                ok_msg = f"  ✓ {inline_path}: deps match real crate {sorted(inline_deps)}"
                print(ok_msg)
                checked.append(inline_path)

    if not seen_paths - {"Cargo.toml"}:
        print("  No inline crate Cargo.toml files found in UserData.")
        return True

    if errors:
        print("\nERRORS:")
        for e in errors:
            for line in e.splitlines():
                print(f"  ✗ {line}")
        print(
            f"\n{len(errors)} inline Cargo.toml validation error(s) found.\n"
            f"The inline Cargo.toml in deploy/cdk/lib/dpdk-test-stack.ts has "
            f"drifted from the real crate's Cargo.toml."
        )
        return False

    print(f"\nAll {len(checked)} inline Cargo.toml(s) validated. ✓")
    return True


if __name__ == "__main__":
    if len(sys.argv) != 3:
        print(
            f"Usage: {sys.argv[0]} <synthesized-template.json> <project-root>",
            file=sys.stderr,
        )
        sys.exit(1)
    ok = check_cargo_deps(sys.argv[1], sys.argv[2])
    sys.exit(0 if ok else 1)
