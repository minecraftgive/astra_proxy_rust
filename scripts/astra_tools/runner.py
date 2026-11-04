"""Suite command execution and machine-readable results, independent of CLI parsing."""
from collections import deque
from dataclasses import dataclass
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
from .workspace import ROOT

# Lines worth surfacing when a step fails: Rust test failures, panics, Python errors.
_FAILURE_LINE = re.compile(r'panicked at|^---- |^test .* FAILED|^failures:|^error(\[|:)|Error:|Traceback|assert')


def timestamped_report(name):
    """Allocate a new report pathname without overwriting a historical result."""
    stamp = datetime.now(timezone.utc).strftime('%Y-%m-%d_%H-%M-%S_%f')
    return ROOT/'.test-tools'/f'{name}-{stamp}.json'


@dataclass(frozen=True)
class Step:
    name: str
    command: list
    timeout: float | None = None
    cwd: Path = ROOT


def _run_annotated(command, *, cwd, env, timeout):
    """Stream output like subprocess.run, keeping failure context for a GitHub annotation."""
    tail, interesting = deque(maxlen=25), deque(maxlen=40)
    # Bytes pass through unchanged: the child's encoding (cp1252 on Windows runners) is
    # unknown here, and re-encoding text into this process's stdout can fail.
    child = subprocess.Popen(command, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    output = getattr(sys.stdout, 'buffer', None)
    killed = []

    def kill():
        killed.append(True)
        child.kill()

    timer = threading.Timer(timeout, kill) if timeout else None
    if timer:
        timer.start()
    try:
        for raw in child.stdout:
            line = raw.decode('utf-8', errors='replace')
            if output is not None:
                sys.stdout.flush()
                output.write(raw)
                output.flush()
            else:
                sys.stdout.write(line)
            line = line.rstrip()
            tail.append(line)
            if _FAILURE_LINE.search(line):
                interesting.append(line)
        code = child.wait()
    finally:
        if timer:
            timer.cancel()
    return (124 if killed else code), [*interesting, '--- last output ---', *tail]


def _annotate_failure(name, lines):
    """Logs need a GitHub login; annotations are public, so failures stay diagnosable."""
    message = '\n'.join(lines)[-6000:]
    escaped = message.replace('%', '%25').replace('\r', '%0D').replace('\n', '%0A')
    print(f'::error title=Step {name} failed::{escaped}', flush=True)


def run_steps(steps, *, env, keep_going=False, report=None):
    results = []
    annotate = os.environ.get('GITHUB_ACTIONS') == 'true'
    for step in steps:
        command = list(map(str, step.command))
        print(f'\n== {step.name}', flush=True)
        start = time.monotonic()
        try:
            if annotate:
                code, context = _run_annotated(command, cwd=step.cwd, env=env, timeout=step.timeout)
                if code:
                    _annotate_failure(step.name, context)
            else:
                code = subprocess.run(command, cwd=step.cwd, env=env, timeout=step.timeout).returncode
        except subprocess.TimeoutExpired:
            code = 124
        except OSError as error:
            print(str(error), flush=True)
            code = 127
        elapsed = round(time.monotonic() - start, 3)
        results.append({'name': step.name, 'command': command, 'cwd': str(step.cwd),
                        'exit_code': code, 'seconds': elapsed})
        print(f'== {"PASS" if code == 0 else "FAIL"} {step.name} · {elapsed:.1f}s', flush=True)
        if report:
            path = Path(report)
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(json.dumps({'steps': results, 'passed': False}, indent=2) + '\n', encoding='utf-8')
        if code and not keep_going:
            break
    passed = all(result['exit_code'] == 0 for result in results)
    if report:
        Path(report).write_text(json.dumps({'steps': results, 'passed': passed}, indent=2) + '\n', encoding='utf-8')
    return results
