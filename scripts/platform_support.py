"""Platform helpers for launch, packaging and local regression scripts."""
import os
import signal
import subprocess


def executable(name):
    return name + ('.exe' if os.name == 'nt' else '')


def process_flags():
    return {'creationflags': subprocess.CREATE_NEW_PROCESS_GROUP} if os.name == 'nt' else {}


def interrupt(child):
    """Request a graceful console shutdown; terminate() is a forced kill on Windows."""
    child.send_signal(signal.CTRL_BREAK_EVENT if os.name == 'nt' else signal.SIGTERM)
