"""Compare two Rule Core builds with isolated, offline copies of an inline rule config."""
import argparse
import ctypes
from ctypes import wintypes
import json
import os
from pathlib import Path
import re
import statistics
import subprocess
import tempfile
import time
import tomllib


class Counters(ctypes.Structure):
    _fields_ = [("cb", wintypes.DWORD), ("faults", wintypes.DWORD)] + [
        (name, ctypes.c_size_t)
        for name in (
            "peak_working", "working", "peak_paged", "paged", "peak_nonpaged",
            "nonpaged", "pagefile", "peak_pagefile", "private",
        )
    ]


get_memory = ctypes.WinDLL("psapi", use_last_error=True).GetProcessMemoryInfo
get_memory.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
get_memory.restype = wintypes.BOOL


def memory(process):
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    if not get_memory(int(process._handle), ctypes.byref(counters), counters.cb):
        raise ctypes.WinError(ctypes.get_last_error())
    return {
        "private_mib": counters.private / 2**20,
        "working_mib": counters.working / 2**20,
        "peak_private_mib": counters.peak_pagefile / 2**20,
        "peak_working_mib": counters.peak_working / 2**20,
    }


def run(binary, config, directory):
    log = directory / "run.log"
    control = directory / "control.sock"
    env = dict(os.environ, ASTRA_CONTROL_SOCKET=str(control), ASTRA_LOG_LEVEL="info")
    env.pop("RUST_LOG", None)
    with log.open("wb") as output:
        started = time.perf_counter()
        process = subprocess.Popen(
            [str(binary), str(config)], stdout=output, stderr=subprocess.STDOUT,
            env=env, creationflags=subprocess.CREATE_NO_WINDOW,
        )
        try:
            while "Rule Core ready" not in log.read_text(encoding="utf-8", errors="replace"):
                if process.poll() is not None or time.perf_counter() - started > 60:
                    raise RuntimeError(log.read_text(encoding="utf-8", errors="replace"))
                time.sleep(0.01)
            ready_ms = (time.perf_counter() - started) * 1000
            time.sleep(1)
            result = memory(process)
            result["ready_ms"] = ready_ms
        finally:
            try:
                if process.poll() is None:
                    subprocess.run(
                        [str(binary), "--stop", str(control)], env=env,
                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10,
                        creationflags=subprocess.CREATE_NO_WINDOW,
                    )
                    process.wait(timeout=5)
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("This benchmark uses Windows process memory counters")
    text = args.config.read_text(encoding="utf-8")
    parsed = tomllib.loads(text)
    # This diagnostic fixture uses the current configuration's local databases.
    if parsed.get("rule_sets"):
        parser.error("This comparison fixture expects inline rules, without rule_sets")
    for key in ("geoip_database", "geosite_database"):
        source = Path(parsed[key])
        if not source.is_absolute():
            source = (args.config.parent / source).resolve()
        text = re.sub(rf"(?m)^{key}\s*=.*$", f"{key} = {json.dumps(source.as_posix())}", text)
    text = re.sub(r"(?m)^(geoip_url|geosite_url|watch_config)\s*=.*\r?\n?", "", text)
    geoip_rules = [rule for rule in parsed["rules"] if rule["kind"] == "geoip"]
    ip_only = (
        'default_action = "direct"\ndirect_local = false\n'
        + next(line for line in text.splitlines() if line.startswith("geoip_database"))
        + "\n"
        + "\n".join(
            '[[rules]]\n' + "\n".join(f"{key} = {json.dumps(value)}" for key, value in rule.items())
            for rule in geoip_rules
        )
    )
    runs = []
    with tempfile.TemporaryDirectory(prefix="astra-rule-memory-") as temporary:
        root = Path(temporary)
        for scenario, source in (("geoip_only", ip_only), ("full_rules", text)):
            source = re.sub(r"(?m)^socket\s*=.*\r?\n?", "", source)
            for repeat in range(3):
                for label, binary in (("before", args.before), ("after", args.after)):
                    directory = root / f"{scenario}-{repeat}-{label}"
                    directory.mkdir()
                    config = directory / "rules.toml"
                    config.write_text(
                        f"socket = {json.dumps((directory / 'route.sock').as_posix())}\n"
                        "watch_config = false\n" + source, encoding="utf-8",
                    )
                    sample = dict(scenario=scenario, version=label, repeat=repeat, **run(binary.resolve(), config, directory))
                    runs.append(sample)
                    print(json.dumps(sample), flush=True)
    summaries = []
    for scenario in ("geoip_only", "full_rules"):
        for label in ("before", "after"):
            selected = [r for r in runs if r["scenario"] == scenario and r["version"] == label]
            summaries.append(dict(scenario=scenario, version=label, **{
                key: round(statistics.median(r[key] for r in selected), 3)
                for key in ("ready_ms", "private_mib", "working_mib", "peak_private_mib", "peak_working_mib")
            }))
    args.output.write_text(json.dumps(dict(runs=runs, medians=summaries), indent=2), encoding="utf-8")
    print(json.dumps(summaries, indent=2))


if __name__ == "__main__":
    main()
