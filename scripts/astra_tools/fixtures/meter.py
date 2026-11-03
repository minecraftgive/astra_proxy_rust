"""TCP dial-counting and half-close relay fixture."""
import socket
import socketserver
import select

class Meter(socketserver.BaseRequestHandler):
    def handle(self):
        with self.server.lock:
            self.server.connections += 1
        try:
            with socket.create_connection(('127.0.0.1', self.server.destination), timeout=15) as upstream:
                self.request.settimeout(15)
                readers = [self.request, upstream]
                while readers:
                    ready, _, _ = select.select(readers, [], [], 1)
                    for source in ready:
                        target = upstream if source is self.request else self.request
                        data = source.recv(65536)
                        if data:
                            target.sendall(data)
                        else:
                            readers.remove(source)
                            target.shutdown(socket.SHUT_WR)
        except (OSError, ValueError):
            pass
