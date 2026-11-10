"""Exercise wrap and scrolling keys in a real narrow terminal."""
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import termios
import time

binary, cassette = sys.argv[1:]
pid, master = pty.fork()
if pid == 0:
    os.environ['TERM'] = 'xterm-256color'
    os.execv(binary, [binary, '--cassette', cassette])
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 50, 0, 0))
output = bytearray()
reaped = False
try:
    deadline = time.monotonic() + 15
    while b'Tracks' not in output and time.monotonic() < deadline:
        if select.select([master], [], [], 0.2)[0]:
            output.extend(os.read(master, 65536))
    assert b'Tracks' in output, bytes(output[-2000:])
    # Disable wrapping, scroll right/left, enable wrapping, then scroll vertically.
    for key in [b'w', b'\x1b[C', b'\x1b[D', b'w', b'\x1b[6~', b'\x1b[5~', b'q']:
        os.write(master, key)
        time.sleep(0.08)
        if select.select([master], [], [], 0.05)[0]:
            try:
                output.extend(os.read(master, 65536))
            except OSError:
                pass
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        finished, status = os.waitpid(pid, os.WNOHANG)
        if finished:
            reaped = True
            assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, status
            while select.select([master], [], [], 0.05)[0]:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
            break
        time.sleep(0.05)
    assert reaped, 'debugger did not exit'
    assert b'\x1b[?1049l' in output, 'alternate screen was not restored'
finally:
    if not reaped:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    os.close(master)
