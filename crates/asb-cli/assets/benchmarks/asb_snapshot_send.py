"""Bounded-process Linux Btrfs send for a supervisor without guest procfs.

The caller owns the subprocess timeout and output file.  A failed send leaves
its partial stream for diagnosis and must never publish a complete archive.
"""
import argparse
import fcntl
import os
import platform
import struct
import sys

_SEND = 0x40489426
_GET_SUBVOL_INFO = 0x81f8943c
_INFO_SIZE = 504
_SEND_ARGS = '=qQQQQI28s'


def send(snapshot, output_fd, parent=None):
    if sys.platform != 'linux' or platform.machine() not in ('x86_64', 'aarch64') or struct.calcsize('P') != 8:
        raise RuntimeError('direct Btrfs send requires 64-bit Linux x86_64 or aarch64')
    parent_id = 0
    if parent is not None:
        descriptor = os.open(parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
        try:
            info = bytearray(_INFO_SIZE)
            fcntl.ioctl(descriptor, _GET_SUBVOL_INFO, info, True)
            parent_id = struct.unpack_from('=Q', info)[0]
            if parent_id == 0:
                raise RuntimeError('incremental Btrfs parent has no subvolume identity')
        finally:
            os.close(descriptor)
    descriptor = os.open(snapshot, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        args = struct.pack(_SEND_ARGS, output_fd, 0, 0, parent_id, 0, 0, b'')
        fcntl.ioctl(descriptor, _SEND, args)
    finally:
        os.close(descriptor)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('snapshot')
    parser.add_argument('--parent')
    args = parser.parse_args()
    send(args.snapshot, sys.stdout.fileno(), args.parent)


if __name__ == '__main__':
    main()
