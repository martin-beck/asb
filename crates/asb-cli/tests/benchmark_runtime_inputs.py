"""Provider-free native runtime closure and non-execution regressions."""
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ASSETS = Path(__file__).parents[1] / "assets/benchmarks"
sys.path.insert(0,str(ASSETS))
import asb_runtime_inputs as subject
import asb_fingerprint as fingerprints


def native_image(name):
    text = b"\0"+name.encode()+b"\0"
    start = 64+3*64
    symbols = start+len(text)
    data = bytearray(symbols+48)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<Q",data,40,64)
    struct.pack_into("<HH",data,58,64,3)
    struct.pack_into("<I",data,64+64+4,3)
    struct.pack_into("<QQ",data,64+64+24,start,len(text))
    struct.pack_into("<I",data,64+128+4,11)
    struct.pack_into("<QQ",data,64+128+24,symbols,48)
    struct.pack_into("<I",data,64+128+40,1)
    struct.pack_into("<Q",data,64+128+56,24)
    data[start:symbols] = text
    struct.pack_into("<I",data,symbols+24,1)
    struct.pack_into("<H",data,symbols+24+6,1)
    return data


class RuntimeInputTests(unittest.TestCase):
    def test_response_budget_matches_existing_json_including_escaping(self):
        discovery = subject.Discovery()
        for path in ['/ascii', '/quotes"\\\t\n', '/\x7f\u0080\u2603\U0001f600']:
            discovery.file(path)
            encoded = json.dumps({"schema_version":1,"paths":sorted(discovery.paths)}).encode()+b"\n"
            self.assertEqual(discovery.response_bytes,len(encoded))
        for excess in [0,1]:
            discovery = subject.Discovery()
            length = 4*1024**2-discovery.response_bytes-2
            path = "/"+"a"*(length-1+excess)
            if excess:
                with self.assertRaises(subject.InputError):discovery.file(path)
                self.assertEqual(discovery.paths,set())
            else:
                discovery.file(path)
                self.assertEqual(len(json.dumps({"schema_version":1,"paths":[path]}).encode())+1,4*1024**2)
                discovery.file(path)  # Repeated names do not consume the budget twice.

    @unittest.skipUnless(os.name=="posix","nonblocking regular-file inspection")
    def test_elf_fifo_and_changed_file_do_not_become_stable_inputs(self):
        runtime = subject.asb_runtime
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory)/"input"
            with self.assertRaisesRegex(runtime.ExecutionError,"missing"):
                runtime.elf(path)
            os.mkfifo(path)
            started = time.monotonic()
            with self.assertRaises(runtime.ExecutionError):runtime.elf(path)
            self.assertLess(time.monotonic()-started,.5)
            path.unlink()
            path.write_bytes(b"not-an-ELF")
            original = runtime.os.fstat
            calls = [0]
            def changed(fd):
                calls[0] += 1
                if calls[0]==2:
                    with path.open("ab") as output:output.write(b"changed")
                return original(fd)
            with patch.object(runtime.os,"fstat",side_effect=changed):
                with self.assertRaisesRegex(runtime.ExecutionError,"changed during"):
                    runtime.elf(path)

    @unittest.skipUnless(os.name=="posix","nonblocking owned pipe implementation")
    def test_real_probe_stream_limits_and_remaining_deadline(self):
        runtime = subject.asb_runtime
        def probe(code, budget=2):
            return runtime.bounded_probe([sys.executable,"-I","-S","-B","-c",code],deadline=time.monotonic()+budget)
        self.assertEqual(len(probe("import os;os.write(1,b'x'*1048576);os.write(2,b'e'*65536)")),1048576)
        for code in ["import os;os.write(1,b'x'*1048577)",
                     "import os;os.write(2,b'e'*65537)","raise SystemExit(7)"]:
            with self.assertRaises(runtime.ExecutionError):probe(code)
        start = time.monotonic()
        with self.assertRaisesRegex(runtime.ExecutionError,"deadline"):
            probe("import time;time.sleep(30)",.1)
        self.assertLess(time.monotonic()-start,1.5)
        # A permanent read fault is not accepted as EOF; Interrupted is retried.
        original = runtime.os.read
        interrupted = [False]
        def retry(fd,count):
            if count<=16384 and not interrupted[0]:
                interrupted[0] = True
                raise InterruptedError()
            return original(fd,count)
        with patch.object(runtime.os,"read",side_effect=retry):
            self.assertEqual(probe("print('complete')"),b"complete\n")
        def fail(fd,count):
            if count<=16384:raise OSError("SECRET_READ_VALUE")
            return original(fd,count)
        with patch.object(runtime.os,"read",side_effect=fail):
            with self.assertRaisesRegex(runtime.ExecutionError,"transport failed") as caught:
                probe("print('not EOF')")
        self.assertNotIn("SECRET",str(caught.exception))
        real = runtime.subprocess.Popen
        children = []
        close_error = [OSError]
        def close_fault(*args,**kwargs):
            child = real(*args,**kwargs)
            children.append(child)
            close = child.stdout.close
            def fail_close():
                close()
                raise close_error[0]("SECRET_CLOSE_VALUE")
            child.stdout.close = fail_close
            return child
        with patch.object(runtime.subprocess,"Popen",side_effect=close_fault):
            with self.assertRaisesRegex(runtime.ExecutionError,"deadline.*pipe cleanup failed") as caught:
                probe("import time;time.sleep(30)",.05)
        self.assertNotIn("SECRET",str(caught.exception))
        self.assertIsNotNone(children[0].poll())
        close_error[0] = KeyboardInterrupt
        with patch.object(runtime.subprocess,"Popen",side_effect=close_fault):
            with self.assertRaises(KeyboardInterrupt):probe("import time;time.sleep(30)",.05)
        self.assertIsNotNone(children[-1].poll())
        children[-1].stderr.close()
        self.assertEqual(probe("print('cleanup released')"),b"cleanup released\n")

    @unittest.skipUnless(os.name=="posix","owned probe keeper")
    def test_probe_keeper_failure_precedes_spawn_and_uncertainty_closes_admission(self):
        # Isolate the intentionally poisoned singleton from other healthy tests.
        script = '''
import sys,time,subprocess
from unittest.mock import patch
sys.path.insert(0,sys.argv[1])
import asb_runtime as runtime
with patch.object(runtime.threading.Thread,"start",side_effect=RuntimeError("keeper unavailable")):
    with patch.object(runtime.subprocess,"Popen",side_effect=AssertionError("must not spawn")):
        try:runtime.bounded_probe([sys.executable,"-c","pass"])
        except runtime.ExecutionError as error:assert "custody unavailable before spawn" in str(error)
        else:raise AssertionError("missing keeper failure")
        try:runtime.bounded_probe([sys.executable,"-c","pass"])
        except runtime.ExecutionError as error:assert "custody unavailable" in str(error)
        else:raise AssertionError("failed keeper admitted new work")
# This test's mocked start never created a worker; reset only this isolated
# subprocess's singleton to exercise independent real-child handoff below.
runtime._PROBE_CUSTODY = None
start = runtime.threading.Thread.start
starts = []
def interrupted_start(worker):
    starts.append(worker)
    start(worker)
    raise KeyboardInterrupt()
with patch.object(runtime.threading.Thread,"start",interrupted_start):
    with patch.object(runtime.subprocess,"Popen",side_effect=AssertionError("must not spawn")):
        try:runtime.bounded_probe([sys.executable,"-c","pass"])
        except KeyboardInterrupt:pass
        else:raise AssertionError("missing startup unwind")
        try:runtime.bounded_probe([sys.executable,"-c","pass"])
        except runtime.ExecutionError:pass
        else:raise AssertionError("startup unwind admitted work")
assert len(starts)==1
runtime._PROBE_CUSTODY.ready.set()
starts[0].join(2)
assert not starts[0].is_alive()
# The sole real idle keeper is confirmed finished before this fixture reset.
runtime._PROBE_CUSTODY = None
real = runtime.subprocess.Popen
children = []
interrupt_wait = False
def spawn(*args,**kwargs):
    child = real(*args,**kwargs)
    children.append(child)
    wait = child.wait
    def uncertain(timeout=None):
        if timeout is not None:
            if interrupt_wait:raise KeyboardInterrupt()
            raise subprocess.TimeoutExpired("private fixture",timeout)
        return wait()
    child.wait = uncertain
    return child
with patch.object(runtime.subprocess,"Popen",side_effect=spawn):
    try:runtime.bounded_probe([sys.executable,"-c","print('valid')"])
    except runtime.ExecutionError as error:assert "cleanup unconfirmed" in str(error)
    else:raise AssertionError("uncertainty accepted")
    try:runtime.bounded_probe([sys.executable,"-c","pass"])
    except runtime.ExecutionError as error:assert "unconfirmed" in str(error)
    else:raise AssertionError("new admission accepted")
assert len(children)==1
runtime._PROBE_CUSTODY.worker.join(2)
assert not runtime._PROBE_CUSTODY.worker.is_alive()
assert children[0].poll() is not None
runtime._PROBE_CUSTODY = None  # Prior fixture keeper is confirmed finished.
children.clear()
interrupt_wait = True
with patch.object(runtime.subprocess,"Popen",side_effect=spawn):
    try:runtime.bounded_probe([sys.executable,"-c","print('valid')"])
    except KeyboardInterrupt:pass
    else:raise AssertionError("missing cleanup unwind")
    try:runtime.bounded_probe([sys.executable,"-c","pass"])
    except runtime.ExecutionError as error:assert "unconfirmed" in str(error)
    else:raise AssertionError("cleanup unwind admitted new work")
assert len(children)==1
runtime._PROBE_CUSTODY.worker.join(2)
assert not runtime._PROBE_CUSTODY.worker.is_alive()
assert children[0].poll() is not None
'''
        result = subprocess.run([sys.executable,"-I","-S","-B","-c",script,str(ASSETS)],capture_output=True,timeout=5)
        self.assertEqual(result.returncode,0,result.stderr.decode())

    def test_python_entries_are_distinct_from_bundled_dependent_libraries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module,library = root/"module.abi3.so",root/"libdependent.so"
            module.write_bytes(native_image("PyInit_module"))
            library.write_bytes(native_image("library_function"))
            self.assertTrue(subject.python_extension(module))
            self.assertFalse(subject.python_extension(library))
            self.assertEqual(subject.asb_runtime.library_paths("\tstatically linked\n"),{})
            with self.assertRaises(subject.asb_runtime.ExecutionError):
                subject.asb_runtime.library_paths("statically linked\nunexpected text")
            with patch.object(subject.asb_runtime,"elf_identity",return_value=(62,None)), \
                 patch.object(subject.asb_runtime,"resolve_dependencies",return_value={"dependent":str(library)}) as resolve:
                discovery = subject.Discovery()
                discovery.python_extensions(root,"/fixture-loader")
                self.assertEqual(resolve.call_count,1)
                self.assertEqual(resolve.call_args.args[1],module)
                self.assertIn(str(library),discovery.paths)

    def test_native_symbol_tables_are_bounded(self):
        with tempfile.TemporaryDirectory() as directory:
            file = Path(directory)/"module.so"
            for change in ["sections","width","strings","symbols"]:
                image = native_image("PyInit_module")
                if change=="sections": struct.pack_into("<Q",image,40,len(image)+1)
                elif change=="width": struct.pack_into("<H",image,58,1)
                elif change=="strings": struct.pack_into("<Q",image,64+64+32,17*1024**2)
                else: struct.pack_into("<Q",image,64+128+32,25*1024**2)
                file.write_bytes(image)
                with self.assertRaises(subject.InputError,msg=change): subject.python_extension(file)

    @unittest.skipUnless(sys.platform=="linux" and shutil.which("cc"),"needs the local Rust linker")
    def test_real_loader_binds_private_library_without_running_application(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root/"must-not-run"
            (root/"library.c").write_text("int fixture_value(void) { return 0; }\n")
            (root/"application.c").write_text('#include <stdio.h>\nextern int fixture_value(void);\nint main(void) { FILE *f=fopen("'+str(marker)+'","w"); if(f) fclose(f); return fixture_value(); }\n')
            subprocess.run(["cc","-shared","-fPIC",root/"library.c","-o",root/"libfixture.so"],check=True,capture_output=True,timeout=15)
            subprocess.run(["cc",root/"application.c","-L"+str(root),"-lfixture","-Wl,-rpath,"+str(root),"-o",root/"program"],check=True,capture_output=True,timeout=15)
            paths = subject.discover([{"kind":"program","path":str(root/"program"),"search":None}])["paths"]
            self.assertIn(str(root/"libfixture.so"),paths)
            self.assertFalse(marker.exists())
            roots = [{"name":str(index),"path":path} for index,path in enumerate(paths)]
            before = fingerprints.Snapshot(b"k"*32).seal(roots,None)["digest"]
            library = root/"libfixture.so"
            library.write_bytes(library.read_bytes()+b"changed runtime bytes")
            self.assertNotEqual(before,fingerprints.Snapshot(b"k"*32).seal(roots,None)["digest"])
            self.assertFalse(marker.exists())

    @unittest.skipUnless(sys.platform=="linux","uses native ELF interpreters")
    def test_new_earlier_path_candidate_changes_declared_interpreter(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            first,second = root/"first",root/"second"
            first.mkdir(); second.mkdir()
            shutil.copy2("/bin/true",second/"fixture-interpreter")
            script = root/"application"
            script.write_text("#!/usr/bin/env fixture-interpreter\nnot an executable application body\n")
            script.chmod(0o700)
            request = [{"kind":"program","path":str(script),"search":os.pathsep.join([str(first),str(second)])}]
            before = subject.discover(request)["paths"]
            self.assertIn(str(second/"fixture-interpreter"),before)
            shutil.copy2("/bin/true",first/"fixture-interpreter")
            after = subject.discover(request)["paths"]
            self.assertIn(str(first/"fixture-interpreter"),after)
            self.assertNotEqual(before,after)

    def test_bounds_and_errors_do_not_expose_private_values(self):
        with self.assertRaises(subject.InputError): subject.discover([{}]*4097)
        with self.assertRaises(subject.InputError): subject.discover([{"kind":"unknown"}])
        with self.assertRaises(subject.InputError): subject.Discovery(timeout=-1).file("/private-value")
        request = [{"kind":"program","path":"/missing/SECRET_RUNTIME_VALUE","search":None}]
        script = "import sys;sys.path.insert(0,"+repr(str(ASSETS))+ ");import asb_runtime_inputs as m;raise SystemExit(m.main())"
        result = subprocess.run([sys.executable,"-I","-S","-B","-c",script],
            input=json.dumps(request).encode(),capture_output=True,timeout=5)
        # Runtime discovery publishes a sanitized machine-readable outcome;
        # expected validation failures are not helper crashes.
        self.assertEqual(result.returncode,0)
        self.assertEqual(result.stderr,b"")
        self.assertNotIn(b"SECRET",result.stdout)
        self.assertEqual(json.loads(result.stdout),{
            "schema_version":1,"error_code":"required_runtime_unavailable","paths":[]})


if __name__=="__main__": unittest.main()
