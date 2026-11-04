"""Credential-free content/consistency/resource tests of the fingerprint layer."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks/asb_fingerprint.py"
spec = importlib.util.spec_from_file_location("fingerprint", SOURCE)
fingerprint = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fingerprint)
KEY = bytes(range(32))


class FingerprintTests(unittest.TestCase):
    def test_full_manifest_root_population_fits_without_unbounding_the_helper(self):
        with tempfile.TemporaryDirectory() as temporary:
            missing = str(Path(temporary) / "not-installed")
            request = {"key":KEY.hex(), "context":None,
                "roots":[{"name":f"source-{index:06}", "path":missing} for index in range(65536)]}
            result = fingerprint.fingerprint(request, {})
            self.assertEqual(result["nodes"],65536)
            self.assertEqual(result["bytes"],0)
            request["roots"] = [{"name":"unused", "path":missing}] * (fingerprint.MAX_ROOTS+1)
            with self.assertRaisesRegex(fingerprint.FingerprintError,"population"):
                fingerprint.fingerprint(request,{})

    def test_parent_components_follow_actual_symlink_resolution(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "actual/subdirectory").mkdir(parents=True)
            (root / "link").symlink_to(root / "actual/subdirectory", target_is_directory=True)
            actual, lexical = root / "actual/input", root / "input"
            actual.write_text("actual input")
            lexical.write_text("not the actual input")
            def seal():
                return fingerprint.Snapshot(KEY).seal([{
                    "name":"override", "path":str(root / "link/../input"),
                }], None)["digest"]
            before = seal()
            lexical.write_text("irrelevant change")
            self.assertEqual(before, seal())
            actual.write_text("relevant change")
            self.assertNotEqual(before, seal())

    def test_explicit_file_roots_detect_symlink_retargeting(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first, second, link = root / "first", root / "second", root / "override"
            first.write_text("original input")
            second.write_text("replacement input")
            link.symlink_to(first)
            def seal():
                return fingerprint.Snapshot(KEY).seal([{"name":"override", "path":str(link)}], None)["digest"]
            before = seal()
            link.unlink()
            link.symlink_to(second)
            self.assertNotEqual(before, seal())
            self.assertEqual(first.read_text(), "original input")

    def test_context_and_root_boundaries_cannot_alias(self):
        with tempfile.TemporaryDirectory() as temporary:
            file = Path(temporary) / "input"
            file.write_text("same data")
            first = fingerprint.Snapshot(KEY).seal([{"name":"23", "path":str(file)}], 1)
            second = fingerprint.Snapshot(KEY).seal([{"name":"3", "path":str(file)}], 12)
            self.assertNotEqual(first["digest"], second["digest"])

    def test_repeated_direct_input_cannot_change_between_roots(self):
        with tempfile.TemporaryDirectory() as temporary:
            file = Path(temporary) / "input"
            file.write_text("original")
            snapshot = fingerprint.Snapshot(KEY)
            original = snapshot.file_contents
            def read_then_change(path):
                value = original(path)
                file.write_text("changed")
                return value
            with patch.object(snapshot, "file_contents", read_then_change):
                with self.assertRaisesRegex(fingerprint.FingerprintError, "changed"):
                    snapshot.seal([
                        {"name":name, "path":str(file), "direct_file":True}
                        for name in ["first", "second"]
                    ], None)

    def test_direct_file_identity_does_not_depend_on_process_or_pathname(self):
        with tempfile.TemporaryDirectory() as temporary:
            first = Path(temporary) / "first"
            second = Path(temporary) / "second"
            first.write_bytes(b"same executable contents")
            second.write_bytes(first.read_bytes())
            def seal(path):
                return fingerprint.Snapshot(KEY).seal([{
                    "name":"executable", "path":str(path),
                    "bind_location":False, "direct_file":True,
                }], None)["digest"]
            self.assertEqual(seal(first), seal(second))
            if sys.platform == "linux":
                with first.open("rb") as source:
                    handle = Path(f"/proc/{os.getpid()}/fd/{source.fileno()}")
                    expected = seal(handle)
                    first.unlink()
                    self.assertEqual(expected, seal(handle))
                    self.assertEqual(expected, seal(second))
            second.write_bytes(b"replacement executable")
            self.assertNotEqual(expected if sys.platform == "linux" else seal(first), seal(second))

    @unittest.skipUnless(hasattr(os, "mkfifo"), "requires FIFO support")
    def test_file_replaced_with_fifo_before_open_does_not_block(self):
        with tempfile.TemporaryDirectory() as temporary:
            file = Path(temporary) / "input"
            file.write_text("original")
            snapshot = fingerprint.Snapshot(KEY)
            original = snapshot.observe
            def replace(path):
                info = original(path)
                file.unlink()
                os.mkfifo(file)
                return info
            with patch.object(snapshot, "observe", replace):
                with self.assertRaisesRegex(fingerprint.FingerprintError, "replaced"):
                    snapshot.seal([{"name":"input", "path":str(file)}], None)

    def test_content_modes_missing_inputs_and_symlink_targets_are_bound(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            file = root / "input"; file.write_text("one")
            roots = [{"name":"inputs", "path":str(root)}]
            seal = lambda: fingerprint.Snapshot(KEY).seal(roots, {"dataset":"pinned"})["digest"]
            original = seal()
            self.assertEqual(original, seal())
            os.utime(file, None)
            self.assertEqual(original, seal(), "mtime alone is not content")
            file.write_text("two")
            self.assertNotEqual(original, seal())
            file.write_text("one")
            self.assertEqual(original, seal())
            file.chmod(0o700)
            self.assertNotEqual(original, seal())
            link = root / "link"; link.symlink_to(file)
            linked = seal(); file.write_text("different")
            self.assertNotEqual(linked, seal())
            absent = root / "absent"
            missing = fingerprint.Snapshot(KEY).seal([{"name":"missing","path":str(absent)}], None)["digest"]
            absent.write_text("now present")
            self.assertNotEqual(missing, fingerprint.Snapshot(KEY).seal([{"name":"missing","path":str(absent)}], None)["digest"])

    def test_key_environment_context_and_evidence_changes_cannot_share_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); (root / "evidence").write_text("resolved false")
            request = {"key":KEY.hex(), "roots":[{"name":"evidence","path":str(root)}],
                       "context":{"model":"same-model"}, "environment":True}
            environment = {"SECRET_TEST_TOKEN":"never publish this value", "MODEL_ENDPOINT":"http://127.0.0.1:1"}
            original = fingerprint.fingerprint(request, environment)
            response = json.dumps(original)
            self.assertNotIn("SECRET_TEST_TOKEN", response)
            self.assertNotIn("never publish", response)
            self.assertNotIn(KEY.hex(), response)
            self.assertEqual(original["digest"], fingerprint.fingerprint(request, dict(environment, PWD="different", SHLVL="2"))["digest"])
            for changed in [dict(request, key=(b"b"*32).hex()), dict(request, context={"model":"changed"})]:
                self.assertNotEqual(original["digest"], fingerprint.fingerprint(changed, environment)["digest"])
            self.assertNotEqual(original["digest"], fingerprint.fingerprint(request, dict(environment, SECRET_TEST_TOKEN="changed"))["digest"])
            (root / "evidence").write_text("resolved true")
            self.assertNotEqual(original["digest"], fingerprint.fingerprint(request, environment)["digest"])

    def test_only_explicit_housekeeping_files_are_ignored(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); (root / "grade.json").write_text("false")
            roots = [{"name":"job","path":str(root)}]
            seal = lambda: fingerprint.Snapshot(KEY).seal(roots, None, (".terminal-output.tmp",))["digest"]
            original = seal(); (root / ".terminal-output.tmp").write_text("transient")
            self.assertEqual(original, seal())
            (root / "grade.json").write_text("true")
            self.assertNotEqual(original, seal())

    def test_cycles_special_files_size_time_and_population_are_bounded(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); file = root / "input"; file.write_bytes(b"abcd")
            for options in [{"max_bytes":3}, {"max_nodes":0}, {"timeout":-1}]:
                with self.assertRaises(fingerprint.FingerprintError):
                    fingerprint.Snapshot(KEY, **options).seal([{"name":"input","path":str(file)}], None)
            link = root / "loop"; link.symlink_to(link)
            with self.assertRaisesRegex(fingerprint.FingerprintError, "cycle"):
                fingerprint.Snapshot(KEY).seal([{"name":"link","path":str(link)}], None)
            if hasattr(os,"mkfifo"):
                fifo=root/"fifo";os.mkfifo(fifo)
                with self.assertRaisesRegex(fingerprint.FingerprintError, "regular"):
                    fingerprint.Snapshot(KEY).seal([{"name":"fifo","path":str(fifo)}], None)

    def test_replacements_during_hashing_are_not_silently_accepted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary); file=root/"input"; file.write_text("original")
            snapshot=fingerprint.Snapshot(KEY)
            original=snapshot.path
            def changed(path, *args, **kwargs):
                result=original(path, *args, **kwargs)
                if Path(path)==file:
                    file.write_text("changed after reading")
                return result
            with patch.object(snapshot,"path",changed), self.assertRaisesRegex(fingerprint.FingerprintError,"changed"):
                snapshot.seal([{"name":"input","path":str(file)}], None)

    def test_process_errors_never_print_secret_values_or_input_paths(self):
        request={"key":KEY.hex(),"roots":[{"name":"SECRET_PATH","path":"\x00SECRET_VALUE"}],"context":"SECRET_CONTEXT"}
        result=subprocess.run([sys.executable,"-I","-S",str(SOURCE)],input=json.dumps(request).encode(),capture_output=True,timeout=5)
        self.assertEqual(result.returncode,0)
        self.assertEqual(result.stderr,b"")
        self.assertNotIn(b"SECRET",result.stdout)
        self.assertNotIn(KEY.hex().encode(),result.stdout)
        self.assertEqual(json.loads(result.stdout)["error_code"],"verification_failed")

    @unittest.skipUnless(hasattr(os, 'mkfifo'), 'requires FIFO support')
    def test_process_classifies_unsupported_input_without_exposing_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path=Path(directory)/'SECRET_FIFO';os.mkfifo(path)
            request={'key':KEY.hex(),'roots':[{'name':'fixture','path':str(path)}]}
            result=subprocess.run([sys.executable,'-I','-S',str(SOURCE)],input=json.dumps(request).encode(),capture_output=True,timeout=5)
            self.assertEqual(result.returncode,0)
            self.assertEqual(json.loads(result.stdout),{'schema_version':1,'error_code':'unsupported_input'})
            self.assertNotIn(b'SECRET_FIFO',result.stdout)


if __name__=="__main__":
    unittest.main()
