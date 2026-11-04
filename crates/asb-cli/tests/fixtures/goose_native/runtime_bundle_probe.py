"""Stage a hooked Goose with ASB's runtime bundler in an offline task image."""
import os
from pathlib import Path
import subprocess
import sys

asb = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(asb / 'assets/benchmarks'))
from asb_runtime import bundle_tool

if len(sys.argv) != 2 or not os.environ.get('ASB_GOOSE_BINARY') or not os.environ.get('ASB_TEST_IMAGE'):
    raise SystemExit('Set ASB_GOOSE_BINARY and ASB_TEST_IMAGE and provide a new output directory')
root = Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=False)
files, receipt = bundle_tool(os.environ['ASB_GOOSE_BINARY'], 'goose')
for name, content, mode in files:
    destination = root / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(content)
    destination.chmod(mode)
result = subprocess.run([
    'docker', 'run', '--rm', '--network', 'none',
    '--entrypoint', '/opt/asb-runtime/tools/goose/goose',
    '-v', str(root) + ':/opt/asb-runtime:ro', os.environ['ASB_TEST_IMAGE'], '--version',
], capture_output=True, text=True, check=True, timeout=60)
print('bundle kind:', receipt['kind'], 'version:', result.stdout.strip())
