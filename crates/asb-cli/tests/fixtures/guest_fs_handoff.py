"""Offline bridge fixture for actual Rust-exported guest filesystem receipts."""
import io
import json
from pathlib import Path
import sys
import tarfile
from types import SimpleNamespace

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'assets/benchmarks'))
from asb_guest import export_agent_artifacts

guest = Path(sys.argv[1])
host = Path(sys.argv[2])
receipts = json.loads(Path(sys.argv[3]).read_text())

class Container:
    def get_archive(self, source, chunk_size=65536, encode_stream=False):
        name = source.rsplit('/', 1)[-1]
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive, mode='w') as outgoing:
            outgoing.add(guest / 'exports' / name, arcname=name)
        archive.seek(0)
        def chunks():
            while data := archive.read(65536):
                yield data
        return chunks(), {'name':name, 'linkTarget':''}

export_agent_artifacts(SimpleNamespace(workspace=SimpleNamespace(container=Container())), str(host), receipts)
