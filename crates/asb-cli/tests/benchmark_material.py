"""Pure, provider/Docker-free material reader qualification."""
import copy
import importlib.util
import io
from pathlib import Path
import sys
import tarfile
import threading
import time
import unittest
from types import SimpleNamespace

sys.dont_write_bytecode = True
SOURCE = Path(__file__).parents[1] / "assets/benchmarks/asb_material.py"
SPEC = importlib.util.spec_from_file_location("asb_material", SOURCE)
MATERIAL = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MATERIAL)
sys.path.insert(0,str(SOURCE.parent))


class Chunks:
    def __init__(self, data, chunk=91):
        self.data, self.chunk, self.closed, self.reads = data, chunk, False, 0
    def __iter__(self):
        return self
    def __next__(self):
        self.reads += 1
        if not self.data:
            raise StopIteration
        result, self.data = self.data[:self.chunk], self.data[self.chunk:]
        return result
    def close(self):
        self.closed = True


def archive(items, prefix="root", mode=tarfile.PAX_FORMAT):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w", format=mode) as result:
        directory = tarfile.TarInfo(prefix)
        directory.type, directory.mode = tarfile.DIRTYPE, 0o755
        result.addfile(directory)
        directories = {""}
        for name, body, kind, link in items:
            parts = name.split("/")[:-1]
            for index in range(1,len(parts)+1):
                parent = "/".join(parts[:index])
                if parent not in directories:
                    directory = tarfile.TarInfo(prefix+"/"+parent)
                    directory.type,directory.mode = tarfile.DIRTYPE,0o755
                    result.addfile(directory)
                    directories.add(parent)
            item = tarfile.TarInfo(prefix + "/" + name)
            item.type, item.mode = kind, 0o644
            item.linkname = link
            if kind in [tarfile.REGTYPE, tarfile.AREGTYPE]:
                item.size = len(body)
                result.addfile(item, io.BytesIO(body))
            else:
                result.addfile(item)
    return output.getvalue()


def regular(name, content=b"content"):
    return name, content, tarfile.REGTYPE, ""


class MaterialTests(unittest.TestCase):
    def workspace(self, items, mounts=(), workdir="/task"):
        attrs = {"Os":"linux","Architecture":"amd64","RootFS":{"Layers":["sha256:"+"a"*64]}}
        image = SimpleNamespace(attrs=attrs,reload=lambda:None)
        labels = {"asb.owner":"asb","asb.attempt":"asb-"+"a"*32,"asb.recovery":"token"}
        container = SimpleNamespace(id="container-one",name="asb-"+"a"*32,image=image,attrs={"Config":{"Labels":labels},"Mounts":list(mounts)},reload=lambda:None,diff=lambda:[])
        container.get_archive = lambda path,**kwargs:(Chunks(archive(items,prefix=workdir.rsplit("/",1)[-1])),{"name":workdir.rsplit("/",1)[-1]})
        return SimpleNamespace(container=container,workdir=workdir,
                               image_identity={"image_id":"sha256:" + "b" * 64})

    def test_workspace_capture_binds_actual_files_definition_and_explicit_coverage(self):
        definition = {"task":"fixture","base":"pinned"}
        workspace = self.workspace([regular("input",b"prepared-one")])
        before = MATERIAL.capture_workdir(workspace,definition)
        self.assertEqual(before["scope"],"prepared-workload-material-v1")
        changed = MATERIAL.capture_workdir(self.workspace([regular("input",b"prepared-two")]),definition)
        self.assertNotEqual(before["sha256"],changed["sha256"])
        self.assertNotEqual(before,MATERIAL.capture_workdir(workspace,dict(definition,base="different")))
        for unsupported in [self.workspace([],mounts=[{"Destination":"/task/input"}]),self.workspace([],workdir="/")]:
            with self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.capture_workdir(unsupported,definition)
        workspace.container.diff = lambda:[{"Path":"/outside/task-input","Kind":1}]
        with self.assertRaises(MATERIAL.MaterialError):MATERIAL.capture_workdir(workspace,definition)
        workspace.container.diff = lambda:[{"Path":"/etc/hostname","Kind":0},{"Path":"/task/input","Kind":0}]
        self.assertEqual(before,MATERIAL.capture_workdir(workspace,definition))
        nested = self.workspace([regular("file")],workdir="/parent/task")
        MATERIAL.capture_workdir(nested,definition)
        nested.container.diff = lambda:[{"Path":"/parent","Kind":0}]
        with self.assertRaisesRegex(MATERIAL.MaterialError,"outside declared roots"):
            MATERIAL.capture_workdir(nested,definition)

    def test_swe_capture_normalizes_only_proven_reset_artifacts_and_binds_git_config(self):
        from asb_swebench_task import SweBenchTask
        task = object.__new__(SweBenchTask)
        task.row = {"base_commit":"a"*40}
        task.invocation = {"baseline":{"kind":"upstream-task-definition","base_commit":"a"*40},
            "identity":{"dataset":{"source":"fixture","revision":"pinned","split":"test"}},"sdk":{"revision":"pinned","version":"fixture"}}
        task.cancelled = threading.Event()
        labels = {"asb.owner":"asb","asb.attempt":"asb-"+"a"*32,"asb.recovery":"token"}
        task.journal = SimpleNamespace(lock=object(),labels=labels,record={"daemon":"fixture-daemon","resources":[{"kind":"containers","name":"asb-"+"a"*32}]})
        task.client = SimpleNamespace(info=lambda:{"ID":"fixture-daemon"},containers=SimpleNamespace(get=lambda _:task.workspace.container))
        semantic = {"index":b"100644 object src/file\0","flags":b"H src/file\0","reflog":b"HEAD@{0}\0commit\0reset\0ASB\0asb@localhost\0","config":b"core.bare=false\0","dirty":False}
        def checked(arguments,*_):
            if "rev-parse" in arguments:return b"a"*40+b"\n"
            if "status" in arguments:return b" M src/file\n" if semantic["dirty"] else b""
            if "reflog" in arguments:return semantic["reflog"]
            if "config" in arguments:return semantic["config"]
            return semantic["index"] if "--stage" in arguments else semantic["flags"]
        task.checked = checked
        def tree(index=b"timestamp-one",config=b"safe git settings"):
            return self.workspace([regular("src/file"),regular(".git/index",index),regular(".git/logs/HEAD",index),regular(".git/config",config)],workdir="/task")
        task.workspace = tree()
        before = task.initial_material()
        task.workspace = tree(index=b"different reset timestamps")
        self.assertEqual(before,task.initial_material())
        task.workspace = tree(config=b"changed hooks/config")
        semantic["config"] = b"core.bare=true\0"
        self.assertNotEqual(before,task.initial_material())
        semantic["flags"] = b"H src/file\0"
        semantic["reflog"] = b"HEAD@{0}\0different-commit\0reset\0ASB\0asb@localhost\0"
        self.assertNotEqual(before,task.initial_material())
        task.workspace = tree()
        semantic["flags"] = b"h src/file\0"
        self.assertNotEqual(before,task.initial_material())
        semantic["dirty"] = True
        from asb_execution import ExecutionError
        with self.assertRaises(ExecutionError):task.initial_material()

    def test_both_terminal_adapters_use_same_protocol_and_refuse_uncovered_services(self):
        from asb_terminal_bench_1 import TerminalBenchTask
        from asb_harbor import HarborTask
        from asb_execution import ExecutionError
        for task_class in [TerminalBenchTask,HarborTask]:
            task = object.__new__(task_class)
            task.invocation = {"baseline":{"kind":"upstream-environment-definition","source_revision":"pinned","definition_sha256":"a"*64},
                "identity":{"dataset":{"source":"fixture","revision":"pinned","split":"test"}},"sdk":{"revision":"pinned","version":"fixture"}}
            task.cancelled = threading.Event()
            task.workspace = self.workspace([regular("task-input")])
            labels = task.workspace.container.attrs["Config"]["Labels"]
            task.journal = SimpleNamespace(lock=object(),labels=labels,record={"daemon":"fixture-daemon","resources":[{"kind":"containers","name":"asb-"+"a"*32}]})
            task.client = SimpleNamespace(info=lambda:{"ID":"fixture-daemon"},containers=SimpleNamespace(get=lambda _:task.workspace.container))
            material = task.initial_material()
            self.assertEqual(material["schema_version"],1)
            self.assertEqual(material["scope"],"prepared-workload-material-v1")
            self.assertEqual(len(material["sha256"]),64)
            task.journal.record["resources"].append({"kind":"containers","name":"asb-"+"a"*32+"-sidecar"})
            with self.assertRaisesRegex(ExecutionError,"multiple declared services"):
                task.initial_material()
            task.journal.record["resources"].pop()
            task.client.info = lambda:{"ID":"different-daemon"}
            with self.assertRaisesRegex(ExecutionError,"different resource daemon"):
                task.initial_material()

    def digest(self, data, prefix="root", **kwargs):
        chunks = Chunks(data)
        try:
            return MATERIAL.tar_digest(chunks, prefix, **kwargs)
        finally:
            self.assertTrue(chunks.closed)

    def test_image_labels_ids_and_timestamps_do_not_replace_filesystem_identity(self):
        attrs = {"Os":"linux", "Architecture":"amd64", "RootFS":{"Layers":["sha256:"+"a"*64]},
                 "Id":"first", "Created":"now", "Config":{"Labels":{"asb.attempt":"first"}}}
        config = {"Env":["FIXTURE_KEY=not-a-real-credential"], "Cmd":["server"], "Labels":{"asb.attempt":"first"}}
        original = MATERIAL.image_digest(attrs, config)
        other = copy.deepcopy(attrs)
        other.update(Id="second", Created="later")
        other["Config"]["Labels"]["asb.attempt"] = "second"
        self.assertEqual(original, MATERIAL.image_digest(other,dict(config,Labels={"asb.attempt":"second"})))
        other["RootFS"]["Layers"] = ["sha256:"+"b"*64]
        self.assertNotEqual(original,MATERIAL.image_digest(other, config))
        self.assertNotEqual(original,MATERIAL.image_digest(attrs,dict(config,Env=["FIXTURE_KEY=other"])))
        self.assertEqual(len(original),64)
        for invalid in [{}, dict(attrs, RootFS={"Layers":["unverified"]}), dict(attrs, Os="windows")]:
            with self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.image_digest(invalid,config)

    def test_tree_is_order_root_mtime_and_tar_format_independent_but_binds_files(self):
        items = [regular("a"),regular("b",b"other")]
        original = self.digest(archive(items))
        self.assertEqual(original,self.digest(archive(list(reversed(items)),prefix="different"),prefix="different"))
        self.assertEqual(original,self.digest(archive(items,mode=tarfile.USTAR_FORMAT)))
        self.assertNotEqual(original,self.digest(archive([regular("a",b"changed"),items[1]])))
        self.assertNotEqual(original,self.digest(archive([regular("renamed"),items[1]])))
        # Same image metadata cannot conceal entrypoint-created file changes.
        self.assertNotEqual(self.digest(archive([regular("ready",b"one")])),self.digest(archive([regular("ready",b"two")])) )

    def test_hardlink_topology_is_canonical_and_distinct_from_independent_files(self):
        a_first = archive([regular("a"),("b",b"",tarfile.LNKTYPE,"root/a")])
        b_first = archive([regular("b"),("a",b"",tarfile.LNKTYPE,"root/b")])
        original = self.digest(a_first)
        self.assertEqual(original,self.digest(b_first))
        self.assertNotEqual(original,self.digest(archive([regular("a"),regular("b")])))
        with self.assertRaises(MATERIAL.MaterialError):
            self.digest(archive([("b",b"",tarfile.LNKTYPE,"root/a"),regular("a")]))
        links = archive([("link",b"",tarfile.SYMTYPE,"/outside")])
        self.assertNotEqual(self.digest(links),self.digest(archive([("link",b"",tarfile.SYMTYPE,"/elsewhere")])))

    def test_hardlinks_cannot_discard_contradictory_xattrs(self):
        def content(attribute):
            output = io.BytesIO()
            with tarfile.open(fileobj=output,mode="w",format=tarfile.PAX_FORMAT) as target:
                directory = tarfile.TarInfo("root"); directory.type = tarfile.DIRTYPE
                target.addfile(directory)
                first = tarfile.TarInfo("root/a"); first.size = 1
                first.pax_headers = {"SCHILY.xattr.user.test":"first"}
                target.addfile(first,io.BytesIO(b"x"))
                link = tarfile.TarInfo("root/b"); link.type = tarfile.LNKTYPE; link.linkname = "root/a"
                link.pax_headers = {"SCHILY.xattr.user.test":attribute}
                target.addfile(link)
            return output.getvalue()
        self.assertEqual(len(self.digest(content("first"))),64)
        with self.assertRaisesRegex(MATERIAL.MaterialError,"contradictory inode attributes"):
            self.digest(content("second"))

    def test_inode_metadata_and_empty_directories_are_material_not_tar_timestamps(self):
        def content(mode=0o644, uid=0, gid=0, timestamp=1, empty=True):
            output = io.BytesIO()
            with tarfile.open(fileobj=output,mode="w",format=tarfile.PAX_FORMAT) as target:
                root = tarfile.TarInfo("root")
                root.type,root.mode = tarfile.DIRTYPE,0o755
                target.addfile(root)
                item = tarfile.TarInfo("root/input")
                item.mode,item.uid,item.gid,item.mtime,item.size = mode,uid,gid,timestamp,1
                target.addfile(item,io.BytesIO(b"x"))
                if empty:
                    directory = tarfile.TarInfo("root/empty")
                    directory.type,directory.mode = tarfile.DIRTYPE,0o755
                    target.addfile(directory)
            return output.getvalue()
        original = self.digest(content())
        self.assertEqual(original,self.digest(content(timestamp=999999)))
        for change in [dict(mode=0o755),dict(uid=1000),dict(gid=1000),dict(empty=False)]:
            with self.subTest(change=change):
                self.assertNotEqual(original,self.digest(content(**change)))

    def test_directory_metadata_capture_closes_before_reading_descendants_including_root(self):
        class HeaderOnly:
            def __init__(self, header):
                self.header,self.closed,self.reads = header,False,0
            def __iter__(self):return self
            def __next__(self):
                self.reads += 1
                if self.reads > 1:
                    raise AssertionError("directory metadata capture read a descendant")
                return self.header
            def close(self):self.closed = True
        def digest(mode=0o755, uid=0, root="root", kind=tarfile.DIRTYPE):
            item = tarfile.TarInfo(root)
            item.type,item.mode,item.uid = kind,mode,uid
            chunks = HeaderOnly(item.tobuf(format=tarfile.USTAR_FORMAT))
            try:
                return MATERIAL.tar_digest(chunks,root,root_metadata_only=True)
            finally:
                self.assertTrue(chunks.closed)
                self.assertEqual(chunks.reads,1)
        original = digest()
        self.assertEqual(original,digest(root="/"))
        self.assertNotEqual(original,digest(mode=0o700))
        self.assertNotEqual(original,digest(uid=1000))
        with self.assertRaisesRegex(MATERIAL.MaterialError,"wrong kind"):
            digest(kind=tarfile.REGTYPE)
        with self.assertRaises(MATERIAL.MaterialError):
            self.digest(archive([]),root_metadata_only=True,exclusions=["hidden"])

    def test_writable_inventory_is_ordered_retains_deletions_and_rejects_ambiguity(self):
        changes = [{"Path":"/data/gone","Kind":2},{"Path":"/","Kind":0},
                   {"Path":"/data/new","Kind":1},{"Path":"/etc","Kind":0},
                   {"Path":"/etc/hostname","Kind":0},{"Path":"/proc/temporary","Kind":1}]
        expected = [("/",0),("/data/gone",2),("/data/new",1),("/etc",0)]
        self.assertEqual(MATERIAL.change_inventory(changes),expected)
        self.assertEqual(MATERIAL.change_inventory(list(reversed(changes))),expected)
        for invalid in [changes + [changes[0]], [{"Path":"/a//b","Kind":1}],
                        [{"Path":"/a/../b","Kind":1}],[{"Path":"/a","Kind":True}],
                        [{"Path":"relative","Kind":0}], [{"Path":"/a","Kind":3}]]:
            with self.subTest(invalid=invalid),self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.change_inventory(invalid)

    def test_delta_root_type_cannot_silently_dereference_a_symlink(self):
        # The path requested is reported as a symlink, but the archive contains
        # a dereferenced regular file. That must not establish link identity.
        target = io.BytesIO()
        with tarfile.open(fileobj=target,mode="w") as output:
            entry = tarfile.TarInfo("link"); entry.size = 1
            output.addfile(entry,io.BytesIO(b"x"))
        stream = Chunks(target.getvalue())
        with self.assertRaisesRegex(MATERIAL.MaterialError,"requested root kind"):
            MATERIAL.tar_digest(stream,"link",expected_root_kind=b"2")
        self.assertTrue(stream.closed)

    def test_long_utf8_names_are_bounded_and_supported_without_extraction(self):
        name = "long/" + "ü"*120
        self.assertEqual(len(self.digest(archive([regular(name)]))),64)
        self.assertEqual(len(self.digest(archive([regular("x"*200)],mode=tarfile.GNU_FORMAT))),64)
        for limits in [dict(max_entries=1),dict(max_bytes=2),dict(max_file_bytes=2),dict(max_names=2),dict(max_wire=100)]:
            with self.subTest(limits=limits),self.assertRaises(MATERIAL.MaterialError):
                self.digest(archive([regular("file")]),**limits)

    def test_hostile_extensions_are_refused_before_reading_advertised_payload(self):
        for kind in [tarfile.XHDTYPE,tarfile.GNUTYPE_LONGNAME,tarfile.GNUTYPE_LONGLINK,tarfile.XGLTYPE]:
            header = tarfile.TarInfo("extension")
            header.type,header.size = kind,1024**3
            chunks = Chunks(header.tobuf(format=tarfile.USTAR_FORMAT),chunk=512)
            with self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.tar_digest(chunks,"root")
            self.assertEqual(chunks.reads,1)
            self.assertTrue(chunks.closed)

    def test_duplicates_collisions_escape_truncation_checksum_and_trailing_data(self):
        invalid = [archive([regular("same"),regular("same")]),
                   archive([regular("a"),regular("a/child")]),
                   archive([regular("a/child"),regular("a")]),
                   archive([regular("../escape")]),
                   archive([("device",b"",tarfile.CHRTYPE,"")]),
                   archive([regular("ok")])[:700]]
        changed = bytearray(archive([regular("ok")]))
        changed[0] ^= 1
        invalid.extend([bytes(changed),archive([regular("ok")])+b"unexpected"])
        for data in invalid:
            with self.subTest(length=len(data)),self.assertRaises(MATERIAL.MaterialError):
                self.digest(data)

    def test_exclusions_are_exact_and_bound_not_broad_path_patterns(self):
        first = archive([regular("owned/runtime",b"first"),regular("owned-other",b"task")])
        changed = archive([regular("owned/runtime",b"later"),regular("owned-other",b"task")])
        self.assertEqual(self.digest(first,exclusions=["owned"]),self.digest(changed,exclusions=["owned"]))
        self.assertNotEqual(self.digest(first),self.digest(changed))
        self.assertNotEqual(self.digest(first,exclusions=["owned"]),self.digest(first,exclusions=["owned/runtime"]))
        changed_task = archive([regular("owned/runtime",b"later"),regular("owned-other",b"changed")])
        self.assertNotEqual(self.digest(first,exclusions=["owned"]),self.digest(changed_task,exclusions=["owned"]))

    def test_cancellation_deadlines_and_chunk_limits_close_streams(self):
        cancelled = threading.Event(); cancelled.set()
        for options in [dict(cancelled=cancelled),dict(deadline=0)]:
            with self.assertRaises(MATERIAL.MaterialError):
                self.digest(archive([regular("file")]),**options)
        class Slow(Chunks):
            def __next__(self):
                time.sleep(.01)
                return super().__next__()
        slow = Slow(archive([regular("file")]))
        with self.assertRaises(MATERIAL.MaterialError):
            MATERIAL.tar_digest(slow,"root",deadline=time.monotonic()+.005)
        self.assertTrue(slow.closed)
        oversized = Chunks(b"x"*(1024*1024+1),chunk=1024*1024+1)
        with self.assertRaises(MATERIAL.MaterialError):
            MATERIAL.tar_digest(oversized,"root")
        self.assertTrue(oversized.closed)
        class MustNotRead:
            def __iter__(self):return self
            def __next__(self):raise AssertionError("cancelled trailing read must not start")
        reader = MATERIAL.Reader(MustNotRead(),1024,time.monotonic()+10,cancelled)
        with self.assertRaises(MATERIAL.MaterialError):reader.finish()
        for prefix,exclusions in [("../bad",[]),("root",["../bad"]),("root",["a"]*129)]:
            chunks = Chunks(archive([regular("file")]))
            with self.assertRaises(MATERIAL.MaterialError):MATERIAL.tar_digest(chunks,prefix,exclusions)
            self.assertTrue(chunks.closed)

    def test_metadata_structures_and_pax_fields_are_bounded(self):
        for value in [{"x":"a"*(1024*1024+1)},[0]*10001,{"x":2**128},{"x":1.5}]:
            with self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.canonical_digest(value,"fixture")
        for payload in [b"999 path=short\n",b"18 GNU.sparse.x=1\n",b"invalid"]:
            with self.assertRaises(MATERIAL.MaterialError):
                MATERIAL.pax_fields(payload)


if __name__ == "__main__":
    unittest.main()
