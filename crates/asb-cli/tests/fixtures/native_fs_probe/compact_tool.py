"""Run a real child read inside a tool marker for the container fixture."""
import os
import socket
import subprocess
import threading

path = os.environ['ASB_PROBE_CONTROL_SOCKET']
def marker(action):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
        client.connect(path)
        client.sendall(f'tool-{action}:compact:{os.getpid()}:{threading.get_native_id()}\n'.encode())
        with client.makefile('rb') as response:
            assert response.readline().strip().isdigit()
marker('begin')
try:
    subprocess.run(['cat', '/work/mount/source'], stdout=subprocess.DEVNULL, check=True)
finally:
    marker('end')
