"""Suite command execution and machine-readable results, independent of CLI parsing."""
from dataclasses import dataclass
from datetime import datetime, timezone
import json
from pathlib import Path
import subprocess
import time
from .workspace import ROOT


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


def run_steps(steps, *, env, keep_going=False, report=None):
    results = []
    for step in steps:
        command = list(map(str, step.command))
        print(f'\n== {step.name}', flush=True)
        start = time.monotonic()
        try:
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
