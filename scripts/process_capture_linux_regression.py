#!/usr/bin/env python3
"""Run the native executable-routing regression in a private Linux network namespace."""
import argparse
import json
import os
from pathlib import Path
import platform
import shlex
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def executable(*cargo_args):
    output = subprocess.check_output(["cargo", *cargo_args, "--locked", "--message-format=json"],
                                     cwd=ROOT, text=True)
    paths = [artifact["executable"] for line in output.splitlines()
             if (artifact := json.loads(line)).get("executable")]
    if len(paths) != 1:
        raise RuntimeError(f"Expected one Cargo binary, found {paths}")
    return paths[0]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bpf-cflags", default="", help="additional installed libbpf/kernel include paths")
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("run inside a disposable Linux host/VM with cgroup v2 and kernel BTF")
    with tempfile.TemporaryDirectory() as directory:
        obj = Path(directory) / "capture.bpf.o"
        multiarch = subprocess.check_output(["cc", "-print-multiarch"], text=True).strip()
        subprocess.run(["clang", "-target", "bpf", "-O2", "-g", f"-I/usr/include/{multiarch}",
                        *shlex.split(args.bpf_cflags), "-c",
                        ROOT / "crates/astra-process-routing/native/linux/capture.bpf.c", "-o", obj], check=True)
        core = executable("build", "-p", "astra-proxy", "--bin", "astra-proxy")
        tests = executable("test", "-p", "astra-process-routing", "--lib", "--no-run")
        subprocess.run([*(["sudo"] if os.geteuid() else []), "unshare", "--net", "env",
                        f"ASTRA_CAPTURE_OBJECT={obj}", f"ASTRA_CAPTURE_CORE={core}", tests,
                        "--ignored", "--exact", "linux::tests::isolated_capture", "--nocapture"], check=True)


if __name__ == "__main__":
    main()
