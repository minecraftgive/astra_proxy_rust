"""Bounded process lifetime and readiness helpers with explicit stop policy."""
import contextlib
from dataclasses import dataclass
import os
from pathlib import Path
import socket
import json
import signal
import subprocess
import time
from .platform import interrupt, process_flags
from .workspace import ROOT


@dataclass(frozen=True)
class StopPolicy:
    mode: str = 'interrupt'
    timeout: float = 15
    command: tuple[str, ...] = ()

    def __post_init__(self):
        if self.mode not in ('interrupt', 'control', 'kill', 'terminate'):
            raise ValueError(f'Unknown stop policy: {self.mode}')
        if self.timeout <= 0 or (self.mode == 'control' and not self.command):
            raise ValueError('Stop timeout must be positive and control mode needs a command')


def stop_process(child, policy=StopPolicy()):
    if child.poll() is not None:
        return
    try:
        if policy.mode == 'kill':
            child.kill()
        elif policy.mode == 'terminate':
            child.terminate()
        elif policy.mode == 'control':
            subprocess.run(list(policy.command), check=True, capture_output=True, timeout=policy.timeout)
        else:
            interrupt(child)
        child.wait(timeout=policy.timeout)
    except (OSError, subprocess.SubprocessError):
        if child.poll() is None:
            child.kill()
        child.wait(timeout=max(policy.timeout, 5))


@contextlib.contextmanager
def process(command, directory, name, *, env=None, cwd=ROOT, stop=StopPolicy()):
    log_path = Path(directory) / (name + '.log')
    child_env = dict(os.environ, RUST_LOG=os.environ.get('RUST_LOG', 'info'))
    if env:
        child_env.update(env)
    with log_path.open('w', encoding='utf-8') as log:
        child = subprocess.Popen(list(map(str, command)), stdout=log, stderr=subprocess.STDOUT,
                                 cwd=cwd, env=child_env, **process_flags())
        try:
            yield child, log_path
        finally:
            stop_process(child, stop)


@contextlib.contextmanager
def running_core(core, config, control, directory):
    env = dict(os.environ, ASTRA_CONTROL_SOCKET=str(control))
    with process([core, config], directory, config.stem, env=env, stop=StopPolicy(timeout=5)) as (child, log):
        yield child, log, env


def _tail(log):
    if log is None:
        return ''
    try:
        with Path(log).open('rb') as stream:
            stream.seek(0, 2)
            stream.seek(max(0, stream.tell() - 5000))
            return stream.read().decode('utf-8', errors='replace')
    except OSError:
        return ''


def wait_ready(probe, child, log=None, *, timeout=20, interval=.05, description='listener'):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError(f'Process exited before {description} was ready ({child.returncode}): {_tail(log)}')
        try:
            if probe():
                return
        except OSError:
            pass
        time.sleep(min(interval, max(0, deadline - time.monotonic())))
    raise TimeoutError(f'{description} startup timeout: {_tail(log)}')


def wait_port(port, child, log=None, *, timeout=20):
    def probe():
        with socket.create_connection(('127.0.0.1', port), timeout=.1):
            return True
    wait_ready(probe, child, log, timeout=timeout)


def wait_socket(path, child, log, *, timeout=2):
    wait_ready(Path(path).exists, child, log, timeout=timeout, interval=.02, description='control endpoint')


def child_processes(parent_pid):
    """Inspect only direct children of one test-owned process, without arguments."""
    parent_pid = int(parent_pid)
    if parent_pid <= 0:
        raise ValueError('A positive parent process identity is required')
    if os.name == 'nt':
        command = f"Get-CimInstance Win32_Process -Filter 'ParentProcessId = {parent_pid}' | Select-Object ProcessId,Name | ConvertTo-Json -Compress"
        result = subprocess.run(['powershell.exe', '-NoProfile', '-NonInteractive', '-Command', command],
                                capture_output=True, text=True, check=True, timeout=10,
                                creationflags=subprocess.CREATE_NO_WINDOW)
        values = json.loads(result.stdout) if result.stdout.strip() else []
        values = values if isinstance(values, list) else [values]
        return [{'pid': int(value['ProcessId']), 'name': value['Name']} for value in values]
    result = subprocess.run(['ps', '-eo', 'pid=,ppid=,comm='], capture_output=True, text=True, check=True, timeout=10)
    values = []
    for line in result.stdout.splitlines():
        parts = line.strip().split(maxsplit=2)
        if len(parts) == 3 and int(parts[1]) == parent_pid:
            values.append({'pid': int(parts[0]), 'name': Path(parts[2]).name})
    return values


def kill_owned_child(parent, pid, expected_name):
    """Crash a known test child's child after revalidating parent and image name."""
    if parent.poll() is not None:
        raise RuntimeError('The owning test process has already exited')
    if not any(row['pid'] == pid and row['name'] == expected_name for row in child_processes(parent.pid)):
        raise RuntimeError('Refusing to stop a process outside the expected test process family')
    os.kill(pid, signal.SIGTERM if os.name == 'nt' else signal.SIGKILL)
