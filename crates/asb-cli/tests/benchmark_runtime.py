"""Offline runtime-bundle contracts; no Docker daemon or provider required."""
import os
import json
import hashlib
import io
from pathlib import Path
import subprocess
import select
import signal
import shutil
import struct
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
from test_environment import coverage_env

sys.dont_write_bytecode = True
sys.path.insert(0,str(Path(__file__).parents[1]/"assets/benchmarks"))
import asb_runtime as subject
from asb_execution import ExecutionError


def elf_fixture(interpreter=None):
    """Small structural ELF fixture, never a runnable benchmark submission."""
    data = bytearray(256)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHI", data, 16, 3, 62, 1)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HHH", data, 52, 64, 56, 2)
    if interpreter is not None:
        raw = interpreter + b"\0"
        struct.pack_into("<I", data, 64, 3)
        struct.pack_into("<Q", data, 72, 192)
        struct.pack_into("<Q", data, 96, len(raw))
        data[192:192 + len(raw)] = raw
    return data


class RuntimeTests(unittest.TestCase):
    def test_large_elf_metadata_does_not_admit_or_allocate_a_guest_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "metadata-only"
            original = elf_fixture()
            path.write_bytes(original)
            self.assertEqual(subject.elf_identity(path), subject.elf(path)[1:])
            self.assertEqual(subject.elf(path)[0], original)
            with path.open("r+b") as output:
                output.truncate(subject.MAX_FILE + 1)
            # Demand-sized header reads, not a full-file read or a higher cap.
            real_open = subject.os.fdopen
            reads = []
            modes = []
            class Observed:
                def __init__(self, source): self.source = source
                def __enter__(self): return self
                def __exit__(self, *args): self.source.close()
                def fileno(self): return self.source.fileno()
                def seek(self, offset): return self.source.seek(offset)
                def read(self, size):
                    reads.append(size)
                    self.assert_bounded(size)
                    return self.source.read(size)
                @staticmethod
                def assert_bounded(size):
                    if not 0 <= size <= 4096: raise AssertionError("unbounded metadata read")
            def opened(*args, **kwargs):
                mode = kwargs.get("buffering", -1)
                modes.append(mode)
                source = real_open(*args, **kwargs)
                return Observed(source) if mode == 0 else source
            with patch.object(subject.os, "fdopen", side_effect=opened):
                self.assertEqual(subject.elf_identity(path), (62, None))
                self.assertEqual(reads, [64, 56, 56])
                for operation in [subject.elf, subject.bundle]:
                    with self.assertRaisesRegex(ExecutionError, "exceeds 256 MiB"):
                        operation(path)
            self.assertEqual(modes, [0, -1, -1])

    def test_elf_metadata_wide_sparse_headers_have_a_fixed_read_budget(self):
        header = elf_fixture()[:64]
        struct.pack_into("<HH", header, 54, 65535, 2048)
        interpreter = b"/fixture-loader\0"
        end = 64 + 65535 * 2048
        first = bytearray(56)
        struct.pack_into("<I", first, 0, 3)
        struct.pack_into("<Q", first, 8, end)
        struct.pack_into("<Q", first, 32, len(interpreter))
        class Sparse:
            def __init__(self): self.position, self.reads = 0, []
            def seek(self, position): self.position = position
            def read(self, size):
                self.reads.append(size)
                if size > 4096: raise AssertionError("stride became an allocation")
                data = {0: header, 64: first, end: interpreter}.get(self.position, bytes(size))
                self.position += size
                return data
        source = Sparse()
        self.assertEqual(subject._elf_metadata(source, end + len(interpreter)), (62, "/fixture-loader"))
        self.assertEqual(len(source.reads), 2050)
        self.assertEqual(sum(source.reads), 64 + 2048 * 56 + len(interpreter))

    def test_elf_metadata_rejects_malformed_ranges_headers_and_interpreters(self):
        cases = []
        def changed(offset, format_, value):
            data = elf_fixture(b"/fixture-loader")
            struct.pack_into(format_, data, offset, value)
            cases.append(data)
        for offset, format_, value in [
            (4, "<B", 1), (5, "<B", 2), (6, "<B", 0),
            (16, "<H", 1), (18, "<H", 0), (20, "<I", 0),
            (32, "<Q", 2**64 - 1), (52, "<H", 63),
            (54, "<H", 55), (56, "<H", 0), (56, "<H", 2049),
            (56, "<H", 4), (72, "<Q", 2**64 - 1),
            (96, "<Q", 1), (96, "<Q", 4097),
        ]:
            changed(offset, format_, value)
        for raw in [b"relative\0", b"/embedded\0nul\0", b"/nonascii-\xff\0", b"/unterminated"]:
            data = elf_fixture()
            struct.pack_into("<I", data, 64, 3)
            struct.pack_into("<Q", data, 72, 192)
            struct.pack_into("<Q", data, 96, len(raw))
            data[192:192 + len(raw)] = raw
            cases.append(data)
        duplicate = elf_fixture(b"/fixture-loader")
        duplicate[120:176] = duplicate[64:120]
        cases.extend([duplicate, bytearray(b"not an ELF")])
        for index, data in enumerate(cases):
            with self.subTest(index=index), self.assertRaises(ExecutionError):
                subject._elf_metadata(io.BytesIO(data), len(data))
        class Short(io.BytesIO):
            def read(self, count): return super().read(count)[:-1]
        with self.assertRaisesRegex(ExecutionError, "changed during"):
            subject._elf_metadata(Short(elf_fixture()), 256)

    def test_elf_metadata_pins_the_open_file_and_rejects_changes_during_inspection(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "entry"
            original = elf_fixture()
            path.write_bytes(original)
            parse = subject._elf_metadata
            def replaced(source, length):
                path.rename(Path(directory) / "retained")
                path.write_bytes(b"foreign non-ELF replacement")
                return parse(source, length)
            with patch.object(subject, "_elf_metadata", side_effect=replaced):
                # Renaming the pinned inode changes its ctime. Reject that
                # observation; never reopen/parse the foreign replacement.
                with self.assertRaisesRegex(ExecutionError, "changed during"):
                    subject.elf_identity(path)
            self.assertEqual(path.read_bytes(), b"foreign non-ELF replacement")
            path.write_bytes(original)
            def changed(source, length):
                result = parse(source, length)
                with path.open("ab") as output: output.write(b"changed")
                return result
            with patch.object(subject, "_elf_metadata", side_effect=changed):
                with self.assertRaisesRegex(ExecutionError, "changed during"):
                    subject.elf_identity(path)

    @unittest.skipUnless(sys.platform == "linux" and os.environ.get("ASB_TEST_HOST_ASB"),
                         "needs the just-built Linux ASB executable")
    def test_kernel_reference_bundles_the_running_binary_after_path_replacement(self):
        original = Path(os.environ["ASB_TEST_HOST_ASB"]).absolute()
        digest = hashlib.sha256(original.read_bytes()).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            executable = Path(directory) / "asb"
            shutil.copy2(original, executable)
            process = subprocess.Popen([executable, "__asb-gate", "/bin/true"],
                                       env=coverage_env({"PATH": os.defpath}), stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                deadline = time.monotonic() + 5
                while not any(line.startswith("State:") and "T" in line
                              for line in Path(f"/proc/{process.pid}/status").read_text().splitlines()):
                    self.assertLess(time.monotonic(), deadline, "gate never stopped")
                    self.assertIsNone(process.poll())
                    time.sleep(.005)
                executable.unlink()  # Only the test-created executable copy.
                files, receipt = subject.bundle(f"/proc/{process.pid}/exe")
                image = next(content for name, content, _ in files if name in ["asb", "bin/asb"] and content.startswith(b"\x7fELF"))
                self.assertEqual(hashlib.sha256(image).hexdigest(), digest)
                self.assertNotIn(str(directory), str(receipt))
                os.kill(process.pid, signal.SIGCONT)
                _, stderr = process.communicate(timeout=5)
                self.assertEqual(process.returncode, 0, stderr.decode())
            finally:
                if process.poll() is None:
                    process.kill()
                process.communicate(timeout=5)

    def test_loader_dependency_parser_is_complete_and_rejects_unresolved_or_ambiguous_libraries(self):
        text = "linux-vdso.so.1 (0x123)\nlibc.so.6 => /lib/libc.so.6 (0x456)\n/lib64/ld-linux.so.2 (0x789)\n"
        self.assertEqual(subject.library_paths(text),{"libc.so.6":"/lib/libc.so.6","ld-linux.so.2":"/lib64/ld-linux.so.2"})
        for invalid in ["libc.so.6 => not found", "./bad => /lib/x (0x123)",text+"libc.so.6 => /other/libc.so.6 (0x124)"]:
            with self.subTest(invalid=invalid),self.assertRaises(ExecutionError):
                subject.library_paths(invalid)

    def test_captured_ubuntu26_main_mapping_is_not_a_library_identity(self):
        captured = (Path(__file__).parent/"fixtures/loader-ubuntu26-asb.txt").read_text()
        self.assertEqual(hashlib.sha256(captured.encode()).hexdigest(),
                         "2b5ef0db4ee89223c334d3eab68a7a1dbe8c6f503d3426844484153fce59847d")
        expected = {"libgcc_s.so.1":"/usr/lib/x86_64-linux-gnu/libgcc_s.so.1",
                    "libc.so.6":"/usr/lib/x86_64-linux-gnu/libc.so.6",
                    "ld-linux-x86-64.so.2":"/lib64/ld-linux-x86-64.so.2"}
        self.assertEqual(subject.library_paths(captured), expected)
        tail = "\n".join(captured.splitlines()[1:])+"\n"
        for address in ["0", "1", "ABCDEF", "0123456789abcdef"]:
            with self.subTest(address=address):
                self.assertEqual(subject.library_paths(f"\n\t (0x{address})\n"+tail),expected)

    def test_anonymous_mapping_never_hides_unknown_or_unresolved_dependencies(self):
        header = "\t (0x000072fb98000000)\n"
        library = "libc.so.6 => /lib/libc.so.6 (0x456)\n"
        invalid = [
            header, header+"linux-vdso.so.1 (0x123)\n",
            header+header+library, library+header,
            "linux-vdso.so.1 (0x123)\n"+header+library,
            "(0x)\n"+library, "(0x123456789abcdef01)\n"+library,
            "(0xNOTHEX)\n"+library, "(0x123) extra\n"+library,
            "missing => (0x123)\n"+library,
            header+"libmissing.so => not found\n",
            header+library+"libmissing.so => not found\n",
            header+library+"libc.so.6 => /other/libc.so.6 (0x457)\n",
            header+"unexpected loader text\n"+library,
            header+"".join(f"lib{i}.so => /lib/lib{i}.so (0x123)\n" for i in range(65)),
        ]
        for text in invalid:
            with self.subTest(text=text),self.assertRaises(ExecutionError):
                subject.library_paths(text)

    @unittest.skipUnless(os.environ.get("ASB_TEST_HOST_ASB"),"needs the just-built ASB executable")
    def test_auxiliary_runtime_uses_separate_private_loader(self):
        path = Path(os.environ['ASB_TEST_HOST_ASB']).absolute()
        files,receipt = subject.bundle_tool(path,'fixture')
        self.assertTrue(all(name.startswith('tools/fixture/') for name,_,_ in files))
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory)
            for name,content,mode in files:
                target=root/name;target.parent.mkdir(parents=True,exist_ok=True)
                if name=='tools/fixture/fixture' and receipt['kind']=='bundled-loader':
                    self.assertNotIn(b'ASB_RUNTIME_EXECUTABLE',content)
                    content=content.replace(b'/opt/asb-runtime',str(root).encode())
                target.write_bytes(content);target.chmod(mode)
            output=subprocess.run([root/'tools/fixture/fixture','--version'],capture_output=True,timeout=10)
            self.assertEqual(output.returncode,0,output.stderr.decode())
        for invalid in ('../bad','', 'CAPS'):
            with self.subTest(invalid=invalid),self.assertRaises(ExecutionError):subject.bundle_tool(path,invalid)

    @unittest.skipUnless(os.environ.get("ASB_TEST_HOST_ASB"),"needs the just-built ASB executable")
    def test_real_asb_bundle_executes_without_injecting_library_paths_into_children(self):
        path = Path(os.environ["ASB_TEST_HOST_ASB"]).absolute()
        files,receipt = subject.bundle(path)
        self.assertIn(receipt["kind"],["static","bundled-loader"])
        self.assertNotIn(str(path),str(receipt))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name,content,mode in files:
                target = root/name
                target.parent.mkdir(parents=True,exist_ok=True)
                # Fixed deployment root is changed only in this local test
                # launcher; the ELF and libraries remain byte-for-byte equal.
                if name == "asb" and receipt["kind"] == "bundled-loader":
                    self.assertNotIn(b"LD_LIBRARY_PATH",content)
                    content = content.replace(subject.ROOT.encode(),str(root).encode())
                target.write_bytes(content)
                target.chmod(mode)
            result = subprocess.run([root/"asb","--version"],env=coverage_env({"PATH":os.defpath}),capture_output=True,text=True,timeout=10)
            self.assertEqual(result.returncode,0,result.stderr)
            self.assertTrue(result.stdout.startswith("asb "),result.stdout)
            result = subprocess.run([root/"asb","__asb-agent"],env=coverage_env({"PATH":os.defpath,"ASB_GUEST_AGENT_REQUEST":"invalid"}),
                                    capture_output=True,text=True,timeout=10)
            self.assertNotEqual(result.returncode,0)
            self.assertIn("invalid guest execution request",result.stderr)
            project = root/"project"
            for name in [".asb",".asb-tools/pi/bin","workspace"]:
                (project/name).mkdir(parents=True)
            (project/".asb/project.toml").write_text("tools_dir='.asb-tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n")
            client = project/".asb-tools/pi/bin/pi"
            client.write_text("#!/bin/sh\ntest -z \"$LD_LIBRARY_PATH\" || exit 90\n"
                "printf '%s\\n' '{\"type\":\"turn_end\",\"usage\":{\"input\":2,\"output\":3}}'\n"
                "printf '%s\\n' '{\"type\":\"result\",\"result\":\"offline fixture\"}'\n")
            client.chmod(0o755)
            request = {"config":{"agent":"pi","provider":"openrouter","model":"fixture","workload":"fixture",
                "workspace":str(project/"workspace")}, "connection":{"interface":"open_ai_chat","authentication":"bearer",
                "base_url":"http://127.0.0.1:1/v1","credential":"fixture"}, "prompt":"offline fixture",
                "project_root":str(project),"runtime_dir":str(project/"private"),"run_dir":str(project/"private/attempt")}
            process = subprocess.Popen([root/"asb","__asb-agent"],env=coverage_env({"PATH":os.defpath,"HOME":str(project),
                "ASB_GUEST_AGENT_REQUEST":json.dumps(request)}),stdout=subprocess.PIPE,stderr=subprocess.PIPE)
            try:
                self.assertTrue(select.select([process.stdout],[],[],5)[0],"guest did not become ready")
                ready = process.stdout.readline().strip()
                prefix = b"__ASB_GATE__:"
                self.assertTrue(ready.startswith(prefix) and ready[len(prefix):].isdigit(), ready)
                gated_pid = int(ready[len(prefix):])
                ancestor = gated_pid
                while ancestor != process.pid:
                    self.assertGreater(ancestor, 1, "gate must belong to the fixture process tree")
                    status = Path(f"/proc/{ancestor}/status").read_text()
                    ancestor = int(next(line.split()[1] for line in status.splitlines() if line.startswith("PPid:")))
                deadline = time.monotonic()+5
                while not any(line.startswith("State:") and "T" in line for line in Path(f"/proc/{gated_pid}/status").read_text().splitlines()):
                    self.assertLess(time.monotonic(),deadline)
                    time.sleep(0.005)
                os.kill(gated_pid,signal.SIGCONT)
                _,stderr = process.communicate(timeout=5)
                self.assertEqual(process.returncode,0,stderr.decode())
                metadata = json.loads((project/"private/attempt/agent-result.json").read_text())
                # Pi token accounting is returned through its sealed deferred
                # summary; this guest-protocol boundary does not aggregate it.
                self.assertIsNone(metadata["token_usage"]["total_tokens"])
                self.assertEqual(len(metadata["exported_artifacts"]),1)
                self.assertEqual(metadata["exported_artifacts"][0]["kind"],"asb-deferred-adapter-collection")
            finally:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
        with self.assertRaisesRegex(ExecutionError,"architecture"):
            subject.bundle(path,"a-deliberately-incompatible-image")


if __name__ == "__main__":
    unittest.main()
