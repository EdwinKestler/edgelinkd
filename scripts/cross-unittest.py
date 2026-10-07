#!/usr/bin/env python3

import subprocess
import sys
import json
import os
import argparse

parser = argparse.ArgumentParser(description="Run test binaries using qemu-*.")
parser.add_argument("qemu", help="The QEMU command (qemu-arm)")
parser.add_argument("toolchain_prefix", help="The toolchain prefix (e.g., arm-linux-gnueabihf)")
parser.add_argument("cargo_output", help="The path to the cargo-output.json file")
args = parser.parse_args()

qemu_cmd = args.qemu
toolchain_prefix = args.toolchain_prefix
cargo_output_path = args.cargo_output

try:
    with open(cargo_output_path, 'r') as f:
        cargo_output = [json.loads(line) for line in f]
except FileNotFoundError:
    print(f"Error: {cargo_output_path} not found. Please run cargo test first.")
    sys.exit(1)

def resolve_exe(exe):
    """cross emits container paths with CARGO_TARGET_DIR=/target."""
    candidates = [exe]
    norm = exe.replace('\\', '/')
    if norm.startswith('/target/'):
        candidates.append(os.path.join('target', *norm[len('/target/'):].split('/')))
    for path in candidates:
        if os.path.isfile(path):
            return path
    return None

# Host-only artifacts (proc-macro / build-script tests) land in target/ci, not
# target/<triple>/ci. QEMU cannot run them.
test_binaries = []
for entry in cargo_output:
    if entry.get('profile', {}).get('test') is not True:
        continue
    exe = entry.get('executable')
    if not exe:
        continue
    norm = exe.replace('\\', '/')
    if '/target/ci/' in norm or '/target/debug/' in norm or '/target/release/' in norm:
        print(f"Skipping host test binary: {exe}")
        continue
    test_binaries.append(exe)

if not test_binaries:
    print("No test binaries found.")
    sys.exit(0)

exit_code = 0

for test_binary in test_binaries:
    host_binary = resolve_exe(test_binary)
    if host_binary is None:
        print(f"Test binary not on the host: {test_binary}")
        exit_code = 1
        continue
    print(f"Running test binary: {host_binary}")
    result = subprocess.run(
        [qemu_cmd, "-L", f"/usr/{toolchain_prefix}", host_binary],
        capture_output=True,
        text=True,
    )
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n")
    if result.returncode != 0:
        print(f"Test failed: {host_binary} (exit {result.returncode})")
        if result.stderr:
            print(result.stderr, end="" if result.stderr.endswith("\n") else "\n")
        elif result.stdout == "":
            print("qemu produced no output (missing interpreter or sysroot is a common cause)")
        exit_code = 1

sys.exit(exit_code)

