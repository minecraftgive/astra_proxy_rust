#!/usr/bin/env python3
"""Transactional startup/reload and independent server control checks."""
import argparse
import contextlib
import pathlib
import socket
import socketserver
import subprocess
import tempfile
import threading
import time
from platform_support import executable
from protocol_matrix import Echo, exact, free_port, process
from server_regression import ROOT, command

def configuration(port,password,extra=''):
    return f'allow_private_targets=true\nshutdown_timeout_secs=0\n{extra}\n[[listeners]]\ntag="socks"\nprotocol="socks5"\nlisten="127.0.0.1:{port}"\n[[listeners.users]]\nname="test"\npassword="{password}"\n'

def authenticated(port,password,target):
    s=socket.create_connection(('127.0.0.1',port),timeout=3)
    s.sendall(b'\x05\x01\x02');assert exact(s,2)==b'\x05\x02'
    encoded=password.encode();s.sendall(b'\x01\x04test'+bytes([len(encoded)])+encoded)
    result=exact(s,2)
    if result!=b'\x01\x00':s.close();return None
    s.sendall(b'\x05\x01\x00\x01\x7f\x00\x00\x01'+target.to_bytes(2,'big'))
    assert exact(s,10)[1]==0
    return s

def main():
    parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--bin-dir',type=pathlib.Path,default=ROOT/'target/debug');args=parser.parse_args();binary=args.bin_dir.resolve()/executable('astra-server-core')
    with tempfile.TemporaryDirectory(prefix='astra-server-life-') as temp,contextlib.ExitStack() as stack:
        d=pathlib.Path(temp);path=d/'server.toml';control=d/'control.sock';port=free_port()
        # A failed second bind must release the already prepared first listener.
        with socket.socket() as occupied:
            occupied.bind(('127.0.0.1',0));occupied.listen()
            good=configuration(port,'old-password')
            duplicate=f'\n[[listeners]]\ntag="occupied"\nprotocol="socks5"\nlisten="127.0.0.1:{occupied.getsockname()[1]}"\n[[listeners.users]]\nname="test"\npassword="other-password"\n'
            path.write_text(good+duplicate)
            assert subprocess.run([str(binary),'--config',str(path),'--check'],capture_output=True,timeout=5).returncode==0
            failed=subprocess.run([str(binary),'--config',str(path),'--control',str(control)],capture_output=True,timeout=5)
            assert failed.returncode!=0 and not control.exists()
            with socket.socket() as released:released.bind(('127.0.0.1',port))
        print('PASS startup rollback releases sockets and control endpoint',flush=True)
        path.write_text(good)
        core,log=stack.enter_context(process([str(binary),'--config',str(path),'--control',str(control)],d,'server'))
        for _ in range(100):
            if control.exists() and command(binary,control,'--status').returncode==0:break
            assert core.poll() is None,log.read_text();time.sleep(.02)
        echo=socketserver.ThreadingTCPServer(('127.0.0.1',0),Echo);echo.daemon_threads=True;stack.callback(echo.server_close);stack.callback(echo.shutdown);threading.Thread(target=echo.serve_forever,daemon=True).start()
        target=echo.server_address[1]
        old=stack.enter_context(authenticated(port,'old-password',target));old.sendall(b'old');assert exact(old,3)==b'old'
        path.write_text(configuration(port,'new-password'))
        assert command(binary,control,'--reload').returncode==0
        old.sendall(b'kept');assert exact(old,4)==b'kept'
        assert authenticated(port,'old-password',target) is None
        with authenticated(port,'new-password',target) as new:new.sendall(b'new');assert exact(new,3)==b'new'
        path.write_text(configuration(port,'unapplied-password',extra='max_connections=1'))
        assert command(binary,control,'--reload').returncode!=0
        with authenticated(port,'new-password',target) as new:new.sendall(b'kept');assert exact(new,4)==b'kept'
        print('PASS reload changes authentication, preserves existing traffic, rejects invalid changes',flush=True)
        assert command(binary,control,'--stop').returncode==0;core.wait(timeout=5);assert not control.exists()
        print('PASS stop closes active sessions and removes control endpoint',flush=True)
if __name__=='__main__':main()
