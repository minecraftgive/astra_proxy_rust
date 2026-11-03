"""Native server control and client fixtures."""
import socket
import subprocess
from astra_tools.net import exact

USER = '52ded580-44fd-4674-8fe3-957b28d66143'

PASSWORD = 'astra-native-server-test'

def command(binary, control, action):
    return subprocess.run([str(binary), '--control', str(control), action], capture_output=True, text=True, timeout=25)

def connect(proxy_port, echo_port):
    stream=socket.create_connection(('127.0.0.1',proxy_port),timeout=15)
    stream.sendall(b'\x05\x01\x00')
    assert exact(stream,2)==b'\x05\x00'
    stream.sendall(b'\x05\x01\x00\x03\x09localhost'+echo_port.to_bytes(2,'big'))
    response=exact(stream,10)
    return stream,response[1]
