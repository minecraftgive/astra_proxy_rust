#!/usr/bin/env python3
"""Isolate server half-close behavior with a minimal VLESS client and no Astra."""
import contextlib
import pathlib
import socket
import tempfile
import threading
import uuid
from protocol_matrix import ROOT, USER, TRAILER, EchoServer, Echo, exact, free_port, process, wait_port, write_json


def main():
    with tempfile.TemporaryDirectory(prefix='astra-halfclose-') as td, EchoServer(('127.0.0.1', 0), Echo) as echo:
        directory = pathlib.Path(td)
        echo.send_trailer = True
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        results = {}
        for engine in ('xray', 'sing-box'):
            port = free_port()
            if engine == 'xray':
                config = {'log': {'loglevel': 'warning'}, 'inbounds': [{'listen':'127.0.0.1','port':port,'protocol':'vless','settings':{'clients':[{'id':USER}],'decryption':'none'}}], 'outbounds':[{'protocol':'freedom'}]}
            else:
                config = {'log': {'level':'warn'}, 'inbounds':[{'type':'vless','listen':'127.0.0.1','listen_port':port,'users':[{'uuid':USER}]}], 'outbounds':[{'type':'direct'}]}
            path = write_json(directory/(engine+'.json'), config)
            with process([str(ROOT/'.test-tools/bin'/engine),'run','-config' if engine=='xray' else '-c',str(path)], directory, engine) as (server, log):
                wait_port(port, server, log)
                with socket.create_connection(('127.0.0.1', port), timeout=5) as client:
                    # Version, UUID, empty addons, TCP, port, IPv4 address.
                    request = b'\x00'+uuid.UUID(USER).bytes+b'\x00\x01'+echo.server_address[1].to_bytes(2,'big')+b'\x01\x7f\x00\x00\x01'
                    payload = bytes(range(256))*256
                    client.sendall(request+payload)
                    version, addon_length = exact(client, 2)
                    assert version == 0
                    exact(client, addon_length)
                    assert exact(client, len(payload)) == payload
                    client.shutdown(socket.SHUT_WR)
                    remainder = b''
                    while chunk := client.recv(4096):
                        remainder += chunk
                    results[engine] = remainder
                    print(f'{engine}: 65536-byte echo verified; after TCP write EOF received {len(remainder)} of {len(TRAILER)} trailer bytes (Astra absent)', flush=True)
        echo.shutdown()
        assert results['xray'] == b'', results
        assert results['sing-box'] == TRAILER, results
        print('CONFIRMED reference-server difference: Xray closes without trailing response; sing-box preserves it')


if __name__ == '__main__':
    main()
