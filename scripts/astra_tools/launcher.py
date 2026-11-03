"""Bounded launcher cleanup shared by the CLI and isolated regression tests."""
import os
import subprocess

def stop_children(children, commands, controls, *, windows=None):
    """Drain proxy before rule, but never let one failed stop skip another child."""
    windows = os.name == 'nt' if windows is None else windows
    errors = []
    for child, command, control in reversed(list(zip(children, commands, controls))):
        label = f'{command[0]} (pid {child.pid})'
        try:
            if child.poll() is not None:
                continue
            try:
                if windows:
                    try:
                        subprocess.run([command[0], '--stop', str(control)], check=True,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                    except (OSError, subprocess.SubprocessError):
                        child.terminate()
                else:
                    child.terminate()
            except ProcessLookupError:
                pass  # The child exited after poll; still reap it below.
            except OSError as error:
                errors.append(f'{label}: stop failed: {error}')
            try:
                child.wait(timeout=30)
            except subprocess.TimeoutExpired:
                try:
                    child.kill()
                except ProcessLookupError:
                    pass
                child.wait(timeout=5)
        except (OSError, subprocess.SubprocessError) as error:
            errors.append(f'{label}: cleanup failed: {error}')
    return errors


def finish_state(state):
    """Only remove an empty directory: recovery records must survive every failure."""
    try:
        state.rmdir()
        return None
    except FileNotFoundError:
        return None
    except OSError:
        return f'Launcher state retained at {state}; inspect pending recovery records before restarting.'

