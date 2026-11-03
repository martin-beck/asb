"""Compatibility entry point for the experimental filesystem replay fixture."""
import importlib.util
from pathlib import Path
import sys

cursor = Path('/asb_fs_replay_cursor.py')
if not cursor.is_file():
    cursor = Path(__file__).resolve().parents[3] / 'assets/benchmarks/asb_fs_replay_cursor.py'
spec = importlib.util.spec_from_file_location('asb_fs_replay_cursor', cursor)
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
Replay = module.Replay
Coordinator = module.Coordinator

if __name__ == '__main__':
    replay = Replay(sys.argv[1], sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else None)
    try:
        replay.finish()
    finally:
        replay.close()
