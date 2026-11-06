"""Offline bootstrap contracts; no Docker daemon, installed client or provider."""
import io
import hashlib
import json
import os
from pathlib import Path
import struct
import sys
import tarfile
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).parents[1] / "assets/benchmarks"))
from asb_execution import ExecutionError
from asb_guest import (execute_agent, export_agent_artifacts, stage_npm_package_tree, stage_python_package_tree, stage_private_inputs, stage_auxiliary_tools, prepare_agent, propagate_guest_timeout,
                       stage_public_cache, stage_bootstrap, PreparationBudget)
from asb_runtime import elf as inspect_elf
from asb_guest import experimental_snapshot_roots, isolate_snapshot_replay_network


def elf(interpreter=False):
    data = bytearray(120)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<H",data,16,2)
    struct.pack_into("<H",data,18,62)
    struct.pack_into("<I",data,20,1)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<HHH", data, 52, 64, 56, 1)
    struct.pack_into("<I", data, 64, 3 if interpreter else 1)
    return data


class Container:
    def __init__(self):
        self.files = []
        self.id = "guest-bootstrap-fixture"
        self.attrs = {"Config":{"User":""}}

    def put_archive(self, destination, source):
        if isinstance(source, bytes):
            source = io.BytesIO(source)
        with tarfile.open(fileobj=source, mode="r") as archive:
            self.files.extend((destination, item.name, archive.extractfile(item).read() if item.isfile() else None)
                              for item in archive.getmembers())
        return True


class ArchiveContainer:
    def __init__(self, name, payload):
        self.name, self.payload = name, payload

    def get_archive(self, source, chunk_size=65536, encode_stream=False):
        if source.rsplit("/",1)[-1] != self.name:
            raise AssertionError("unexpected guest artifact source")
        archive = io.BytesIO()
        with tarfile.open(fileobj=archive,mode="w") as outgoing:
            member = tarfile.TarInfo(self.name)
            member.size = len(self.payload)
            outgoing.addfile(member,io.BytesIO(self.payload))
        class Chunks:
            def __init__(self, bytes):self.stream=io.BytesIO(bytes)
            def __iter__(self):
                while data := self.stream.read(65536):yield data
            def close(self):self.stream.close()
        return Chunks(archive.getvalue()), {"name":self.name,"linkTarget":""}


class GuestBootstrapTests(unittest.TestCase):
    def test_shared_launch_preparation_budget_accepts_long_setup_but_is_bounded(self):
        with patch("asb_guest.time.monotonic", return_value=100):
            self.assertEqual(PreparationBudget(7200).deadline, 7300)
        with self.assertRaises(ExecutionError):
            PreparationBudget(7201)

    def test_snapshot_replay_disconnects_only_owned_routed_networks(self):
        class Network:
            def __init__(self, name): self.name = name
            def disconnect(self, container):
                calls.append(self.name)
                del container.attrs['NetworkSettings']['Networks'][self.name]
        calls = []
        container = SimpleNamespace(attrs={
            'Config':{'Labels':{'asb.owner':'asb','asb.attempt':'attempt'}},
            'HostConfig':{'NetworkMode':'bridge'},
            'NetworkSettings':{'Networks':{'bridge':{'IPAddress':'172.17.0.2'},'extra':{}}},
        }, reload=lambda:None, client=SimpleNamespace(networks=SimpleNamespace(get=Network)))
        task = SimpleNamespace(workspace=SimpleNamespace(container=container,attempt='attempt'))
        isolate_snapshot_replay_network(task)
        self.assertEqual(calls,['bridge','extra'])
        self.assertEqual(container.attrs['NetworkSettings']['Networks'],{})

        for mode in ('host','container:other',None):
            container.attrs['HostConfig']['NetworkMode'] = mode
            with self.subTest(mode=mode), self.assertRaises(ExecutionError):
                isolate_snapshot_replay_network(task)
        container.attrs['HostConfig']['NetworkMode'] = 'bridge'
        container.attrs['Config']['Labels']['asb.attempt'] = 'different'
        with self.assertRaises(ExecutionError): isolate_snapshot_replay_network(task)

    def test_snapshot_replay_rejects_incomplete_network_disconnection(self):
        container = SimpleNamespace(attrs={
            'Config':{'Labels':{'asb.owner':'asb','asb.attempt':'attempt'}},
            'HostConfig':{'NetworkMode':'bridge'},
            'NetworkSettings':{'Networks':{'bridge':{'IPAddress':'172.17.0.2'}}},
        }, reload=lambda:None, client=SimpleNamespace(networks=SimpleNamespace(
            get=lambda _:SimpleNamespace(disconnect=lambda _:None))))
        task = SimpleNamespace(workspace=SimpleNamespace(container=container,attempt='attempt'))
        with self.assertRaises(ExecutionError): isolate_snapshot_replay_network(task)

    def test_snapshot_replay_network_failure_stops_before_agent_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            def forbidden(*_args, **_kwargs):
                raise AssertionError('agent started without network isolation')
            task = SimpleNamespace(snapshot=True, tool_mode='replay', artifacts=Path(directory),
                workspace=SimpleNamespace(container=Container(),execute=forbidden))
            with patch('asb_guest.prepare_agent',return_value=({},{})), \
                 patch('asb_guest.isolate_snapshot_replay_network',side_effect=ExecutionError('network unavailable')):
                with self.assertRaisesRegex(ExecutionError,'network unavailable'):
                    execute_agent(task,{},None,None,None)

    def test_experimental_snapshot_roots_reject_invalid_configuration(self):
        self.assertEqual(experimental_snapshot_roots(None),[])
        self.assertEqual(experimental_snapshot_roots('["/tmp"]'),['/tmp'])
        for value in ('invalid','{}','["relative"]','[null]','["/a\\u0000b"]'):
            with self.subTest(value=value), self.assertRaises(ExecutionError):
                experimental_snapshot_roots(value)

    def test_preinstalled_native_agent_does_not_run_guest_install(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'.asb').mkdir();(root/'.asb/project.toml').write_text('')
            calls=[]
            def execute(*args,**kwargs):
                calls.append(args)
                return {'stdout':b'','stderr':b'','exit_code':0,'timed_out':False,'cancelled':False}
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={},execute=execute),
                                 artifacts=root,checked=lambda _:b'')
            request={'project_root':str(root),'tools_root':str(root),'binary':'fixture',
                     'request':{'config':{'agent':'fixture'}},'bootstrap':{},'auxiliary_tools':{}}
            with patch('asb_guest.bundle',return_value=([],{})), patch('asb_guest.stage_runtime',return_value=(0,0)), \
                 patch('asb_guest.upload'), patch('asb_guest.stage_private_inputs'), \
                 patch('asb_guest.stage_bootstrap',return_value=([],['fixture'])), \
                 patch('asb_guest.stage_auxiliary_tools',return_value={}):
                prepare_agent(task,request,None)
            self.assertEqual(calls,[])

    def test_guest_preparation_forwards_staged_auxiliary_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'.asb').mkdir();(root/'.asb/project.toml').write_text('')
            execute=lambda *args,**kwargs:{'stdout':b'','stderr':b'','exit_code':0,'timed_out':False,'cancelled':False}
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={},execute=execute),
                                 artifacts=root,checked=lambda _:b'')
            request={'project_root':str(root),'tools_root':str(root),'binary':'fixture',
                     'request':{'config':{'agent':'fixture'}},'bootstrap':{},
                     'auxiliary_tools':{'btrfs':'/some/tool'}}
            with patch('asb_guest.bundle',return_value=([],{})), \
                 patch('asb_guest.stage_runtime',return_value=(0,0)), \
                 patch('asb_guest.upload'), patch('asb_guest.stage_private_inputs'), \
                 patch('asb_guest.stage_bootstrap',return_value=([],[])), \
                 patch('asb_guest.stage_auxiliary_tools',return_value={'btrfs':'/opt/asb-private/tools/btrfs/btrfs'}) as stage:
                guest, environment=prepare_agent(task,request,None)
            stage.assert_called_once_with(task,{'btrfs':'/some/tool'},(0,0))
            self.assertNotIn('snapshot_private_mount', guest)
            self.assertEqual(json.loads(environment['ASB_AUXILIARY_TOOLS']),
                             {'btrfs':'/opt/asb-private/tools/btrfs/btrfs'})

    def test_snapshot_guest_requests_private_mount_namespace(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'.asb').mkdir();(root/'.asb/project.toml').write_text('')
            execute=lambda *args,**kwargs:{'stdout':b'','stderr':b'','exit_code':0,'timed_out':False,'cancelled':False}
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={},execute=execute),
                                 artifacts=root,checked=lambda _:b'',snapshot=True,tool_mode='record')
            request={'project_root':str(root),'tools_root':str(root),'binary':'fixture',
                     'request':{'config':{'agent':'fixture'}},'bootstrap':{},
                     'auxiliary_tools':{'btrfs':'a','mkfs.btrfs':'b','rsync':'c'}}
            resources=SimpleNamespace(push=lambda _:None)
            with patch('asb_guest.bundle',return_value=([],{})), \
                 patch('asb_guest.stage_runtime',return_value=(0,0)), \
                 patch('asb_guest.upload'), patch('asb_guest.stage_private_inputs'), \
                 patch('asb_guest.stage_bootstrap',return_value=([],[])), \
                 patch('asb_guest.stage_auxiliary_tools',return_value={'btrfs':'a','mkfs.btrfs':'b','rsync':'c'}), \
                 patch('asb_docker.docker_command',return_value=['docker']), \
                 patch('asb_snapshot_control.GuestSnapshot'):
                guest, environment=prepare_agent(task,request,None,resources=resources)
            self.assertTrue(guest['snapshot_private_mount'])
            self.assertEqual(environment['ASB_FS_MARKER_SOCKET'],'/opt/asb-private/snapshot.sock')

    def test_snapshot_guest_accepts_optional_qcow_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'.asb').mkdir();(root/'.asb/project.toml').write_text('')
            execute=lambda *args,**kwargs:{'stdout':b'','stderr':b'','exit_code':0,'timed_out':False,'cancelled':False}
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={},execute=execute),
                                 artifacts=root,checked=lambda _:b'',snapshot=True,tool_mode='record')
            tools={'btrfs':'a','mkfs.btrfs':'b','rsync':'c','qemu-img':'d','qemu-storage-daemon':'e'}
            request={'project_root':str(root),'tools_root':str(root),'binary':'fixture',
                     'request':{'config':{'agent':'fixture'}},'bootstrap':{},'auxiliary_tools':tools}
            resources=SimpleNamespace(push=lambda _:None)
            with patch('asb_guest.bundle',return_value=([],{})), \
                 patch('asb_guest.stage_runtime',return_value=(0,0)), \
                 patch('asb_guest.upload'), patch('asb_guest.stage_private_inputs'), \
                 patch('asb_guest.stage_bootstrap',return_value=([],[])), \
                 patch('asb_guest.stage_auxiliary_tools',return_value=tools), \
                 patch('asb_docker.docker_command',return_value=['docker']), \
                 patch('asb_snapshot_control.GuestSnapshot') as supervisor, \
                 patch.dict(os.environ,{'ASB_EXPERIMENTAL_SNAPSHOT_ROOTS':'["/tmp"]'}):
                guest,_=prepare_agent(task,request,None,resources=resources)
            self.assertTrue(guest['snapshot_private_mount'])
            self.assertEqual(supervisor.call_args.args[1],tools)
            self.assertEqual(supervisor.call_args.kwargs['extra_roots'],['/tmp'])

    def test_snapshot_replay_forwards_the_verified_archive_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'.asb').mkdir();(root/'.asb/project.toml').write_text('')
            execute=lambda *args,**kwargs:{'stdout':b'','stderr':b'','exit_code':0,'timed_out':False,'cancelled':False}
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={},execute=execute),
                                 artifacts=root,checked=lambda _:b'',snapshot=True,tool_mode='replay')
            request={'project_root':str(root),'tools_root':str(root),'binary':'fixture',
                     'request':{'config':{'agent':'fixture'}},'bootstrap':{},
                     'private_inputs':[{'target':'workspace-snapshots.tar','sha256':'a'*64}],
                     'auxiliary_tools':{'btrfs':'a','mkfs.btrfs':'b','rsync':'c'}}
            with patch('asb_guest.bundle',return_value=([],{})), patch('asb_guest.stage_runtime',return_value=(0,0)), \
                 patch('asb_guest.upload'), patch('asb_guest.stage_private_inputs'), patch('asb_guest.stage_bootstrap',return_value=([],[])), \
                 patch('asb_guest.stage_auxiliary_tools',return_value={'btrfs':'a','mkfs.btrfs':'b','rsync':'c'}), \
                 patch('asb_docker.docker_command',return_value=['docker']), patch('asb_snapshot_control.GuestSnapshot'):
                guest,environment=prepare_agent(task,request,None,resources=SimpleNamespace(push=lambda _:None))
            self.assertEqual(guest['snapshot_archive_sha256'],'a'*64)
            self.assertEqual(environment['ASB_FS_REPLAY_SOCKET'],'/opt/asb-private/snapshot.sock')

    def test_incompatible_optional_qemu_does_not_block_required_tools(self):
        with tempfile.TemporaryDirectory() as directory:
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),image_identity={}),
                                 artifacts=Path(directory),checked=lambda command:None)
            def bundle(source,name,architecture,root):
                if name == 'qemu-storage-daemon':
                    raise ExecutionError('incompatible optional executable')
                return ([],None)
            with patch('asb_guest.bundle_tool',side_effect=bundle), patch('asb_guest.upload'):
                paths=stage_auxiliary_tools(task,{'btrfs':'a','qemu-img':'b','qemu-storage-daemon':'c'},(1,2))
            self.assertEqual(paths,{'btrfs':'/opt/asb-private/tools/btrfs/btrfs'})

    @unittest.skipUnless(os.environ.get('ASB_TEST_HOST_ASB'), 'needs just-built ASB')
    def test_auxiliary_tool_staging_uses_private_prefix(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);container=Container();commands=[]
            task=SimpleNamespace(workspace=SimpleNamespace(container=container,image_identity={}),artifacts=root,
                                 checked=lambda command:commands.append(command))
            paths=stage_auxiliary_tools(task,{'fixture':os.environ['ASB_TEST_HOST_ASB']},(1,2))
            self.assertEqual(paths,{'fixture':'/opt/asb-private/tools/fixture/fixture'})
            self.assertTrue(any(item[1]=='tools/fixture/fixture' for item in container.files))
            self.assertTrue(commands and commands[0][:2]==['mkdir','-p'])
            with self.assertRaises(ExecutionError):stage_auxiliary_tools(task,{'../invalid':'unused'},(1,2))

    def test_private_inputs_stream_and_reject_changed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);source=root/'source';payload=b'abc123'*20000;source.write_bytes(payload)
            container=Container();task=SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root)
            item={'source':str(source),'target':'replay.jsonl','bytes':len(payload),
                  'sha256':hashlib.sha256(payload).hexdigest()}
            with patch.object(Path,'read_bytes',side_effect=AssertionError('whole file read')):
                stage_private_inputs(task,[item],owner=(123,456))
            self.assertEqual(container.files,[('/opt/asb-private','replay.jsonl',payload)])
            container.files.clear();source.write_bytes(payload+b'changed')
            with self.assertRaises(ExecutionError):stage_private_inputs(task,[item])
            self.assertEqual(container.files,[])

    def test_large_snapshot_archive_uses_streaming_stager(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).resolve().parents[3]/'target') as directory:
            root=Path(directory);source=root/'snapshot.tar';storage=root/'storage';storage.mkdir()
            with source.open('wb') as file:file.truncate(64*1024**2+1)
            task=SimpleNamespace(workspace=SimpleNamespace(container=Container(),snapshot_storage=storage),artifacts=root)
            digest=hashlib.sha256()
            with source.open('rb') as file:
                for chunk in iter(lambda:file.read(1024*1024),b''):digest.update(chunk)
            item={'source':str(source),'target':'workspace-snapshots.tar','bytes':source.stat().st_size,
                  'sha256':digest.hexdigest()}
            with patch('asb_guest.upload_files') as upload:
                stage_private_inputs(task,[item],owner=(123,456))
            upload.assert_not_called()
            self.assertEqual((storage/'workspace-snapshots.tar').stat().st_size,item['bytes'])
            with self.assertRaises(FileExistsError):stage_private_inputs(task,[item])
            self.assertTrue((storage/'workspace-snapshots.tar').exists())
            item['target']='ordinary.jsonl'
            with self.assertRaises(ExecutionError):stage_private_inputs(task,[item])
    def test_guest_timeout_propagates_without_inventing_it_from_other_values(self):
        for value, expected in [(True, True), (False, False), ("true", False), (None, False)]:
            result = {"metadata": {"timed_out": value}, "timed_out": False}
            observation = {"timed_out": False}
            propagate_guest_timeout(result, observation)
            self.assertEqual(result["timed_out"], expected)
            self.assertEqual(observation["timed_out"], expected)

    def setUp(self):
        # These fixtures exercise bootstrap selection, not the separately
        # tested ownership transport or actual Docker filesystem staging.
        for name, options in [
            ("asb_staging.configured_owner",dict(return_value=(0,0))),
            ("asb_staging.verify_runtime",dict(return_value=(0,0))),
            ("asb_guest.root_execute",dict(return_value=b"")),
            ("asb_guest.prepare_guest_roots",dict(return_value=({},None))),
        ]:
            mock = patch(name,**options)
            mock.start()
            self.addCleanup(mock.stop)

    def test_guest_artifact_export_copies_only_receipt_bound_archive_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = b"sealed MCP relay transcript"
            digest = hashlib.sha256(payload).hexdigest()
            task = SimpleNamespace(workspace=SimpleNamespace(container=ArchiveContainer("artifact-0",payload)))
            export_agent_artifacts(task,directory,[{"kind":"mcp-relay-filesystem","filename":"artifact-0",
                "bytes":len(payload),"sha256":digest}])
            exported = Path(directory) / "artifact-0"
            self.assertEqual(exported.read_bytes(),payload)
            self.assertEqual(exported.stat().st_mode & 0o777,0o600)

    def test_guest_artifact_export_streams_fragmented_archive_without_spooling(self):
        payload = b'\x00\xffpayload' * 10000
        class FragmentedContainer:
            def get_archive(self, source, chunk_size=65536, encode_stream=False):
                chunks, metadata = ArchiveContainer('artifact-0', payload).get_archive(source)
                def fragments():
                    for chunk in chunks:
                        for offset in range(0, len(chunk), 97):
                            yield chunk[offset:offset + 97]
                return fragments(), metadata
        with tempfile.TemporaryDirectory() as directory:
            task = SimpleNamespace(workspace=SimpleNamespace(container=FragmentedContainer()))
            receipt = {'kind':'fixture','filename':'artifact-0','bytes':len(payload),
                       'sha256':hashlib.sha256(payload).hexdigest()}
            with patch('asb_guest.tempfile.TemporaryFile', side_effect=AssertionError('archive was spooled')):
                export_agent_artifacts(task, directory, [receipt])
            self.assertEqual((Path(directory) / 'artifact-0').read_bytes(), payload)

    def test_guest_artifact_export_rejects_extra_tar_entry_without_publishing(self):
        payload = b'fixture'
        class ExtraContainer:
            def get_archive(self, source, chunk_size=65536, encode_stream=False):
                data = io.BytesIO()
                with tarfile.open(fileobj=data, mode='w') as archive:
                    for name in ('artifact-0', 'extra'):
                        member = tarfile.TarInfo(name)
                        member.size = len(payload)
                        archive.addfile(member, io.BytesIO(payload))
                return iter([data.getvalue()]), {'name':'artifact-0','linkTarget':''}
        with tempfile.TemporaryDirectory() as directory:
            task = SimpleNamespace(workspace=SimpleNamespace(container=ExtraContainer()))
            receipt = {'kind':'fixture','filename':'artifact-0','bytes':len(payload),
                       'sha256':hashlib.sha256(payload).hexdigest()}
            with self.assertRaises(ExecutionError):
                export_agent_artifacts(task, directory, [receipt])
            self.assertFalse((Path(directory) / 'artifact-0').exists())
            self.assertFalse((Path(directory) / '.artifact-0.part').exists())

    def test_guest_artifact_export_checks_trailing_wire_bytes(self):
        payload = b'x'
        class ExcessContainer:
            def get_archive(self, source, chunk_size=65536, encode_stream=False):
                chunks, metadata = ArchiveContainer('artifact-0', payload).get_archive(source)
                def extended():
                    yield from chunks
                    for _ in range(18):
                        yield b'\0' * 65536
                return extended(), metadata
        with tempfile.TemporaryDirectory() as directory:
            task = SimpleNamespace(workspace=SimpleNamespace(container=ExcessContainer()))
            receipt = {'kind':'fixture','filename':'artifact-0','bytes':1,
                       'sha256':hashlib.sha256(payload).hexdigest()}
            with patch('asb_guest.MAX_EXPORTED_ARTIFACT_BYTES', 1024):
                with self.assertRaisesRegex(ExecutionError, 'archive exceeds'):
                    export_agent_artifacts(task, directory, [receipt])
            self.assertFalse((Path(directory) / 'artifact-0').exists())

    def test_guest_artifact_export_rejects_changed_or_oversized_receipts(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = b"x"
            task = SimpleNamespace(workspace=SimpleNamespace(container=ArchiveContainer("artifact-0",payload)))
            for receipt in [
                {"kind":"mcp-relay-filesystem","filename":"artifact-0","bytes":1,"sha256":"a"*64},
                {"kind":"mcp-relay-filesystem","filename":"../artifact-0","bytes":1,"sha256":"a"*64},
                {"kind":"mcp-relay-filesystem","filename":"artifact-0","bytes":16*1024*1024+1,"sha256":"a"*64},
            ]:
                with self.subTest(receipt=receipt), self.assertRaises(ExecutionError):
                    export_agent_artifacts(task,directory,[receipt])
                self.assertFalse(any(path.name.endswith(".part") for path in Path(directory).iterdir()))

    def test_guest_artifact_export_accepts_all_eight_driver_slots(self):
        class MultiContainer:
            def get_archive(self, source, chunk_size=65536, encode_stream=False):
                name = source.rsplit('/', 1)[-1]
                return ArchiveContainer(name, name.encode()).get_archive(source, chunk_size, encode_stream)
        with tempfile.TemporaryDirectory() as directory:
            task = SimpleNamespace(workspace=SimpleNamespace(container=MultiContainer()))
            receipts = []
            for index in range(8):
                name = f'artifact-{index}'
                data = name.encode()
                receipts.append({'kind':f'fixture-{index}', 'filename':name, 'bytes':len(data),
                                 'sha256':hashlib.sha256(data).hexdigest()})
            export_agent_artifacts(task, directory, receipts)
            for receipt in receipts:
                self.assertEqual((Path(directory) / receipt['filename']).read_bytes(), receipt['filename'].encode())

    def test_guest_artifact_export_rejects_ninth_slot(self):
        with tempfile.TemporaryDirectory() as directory:
            task = SimpleNamespace(workspace=SimpleNamespace(container=ArchiveContainer('artifact-8',b'x')))
            receipt = {'kind':'fixture', 'filename':'artifact-8', 'bytes':1, 'sha256':hashlib.sha256(b'x').hexdigest()}
            with self.assertRaises(ExecutionError):
                export_agent_artifacts(task, directory, [receipt])
            with self.assertRaises(ExecutionError):
                export_agent_artifacts(task, directory, [receipt] * 9)
    def test_preparation_budget_is_bounded_anchored_and_not_restarted(self):
        for invalid in [True, False, 0, -1, 7201, float("nan"), float("inf"), "900"]:
            with self.subTest(invalid=invalid), self.assertRaises(ExecutionError):
                PreparationBudget(invalid)
        now = [100.0]
        with patch("asb_guest.time.monotonic", side_effect=lambda:now[0]):
            budget = PreparationBudget(1800)
            now[0] += 1000
            self.assertEqual(budget.remaining(),800)
            now[0] += 801
            with self.assertRaisesRegex(ExecutionError,"deadline exceeded"):budget.remaining()
            self.assertTrue(budget.expired())

    def test_slow_bootstrap_and_original_session_share_budget_without_extending_agent_runtime(self):
        now, calls, deadlines = [100.0], [], []
        with tempfile.TemporaryDirectory() as directory, patch("asb_guest.time.monotonic", side_effect=lambda:now[0]):
            root = Path(directory)
            (root/".asb").mkdir()
            (root/".asb/project.toml").write_text("tools_dir='tools'\n")
            binary = root/"asb"
            binary.write_bytes(elf())
            def execute(argv, **options):
                calls.append((argv,options))
                if argv[-1] == "__asb-agent":
                    self.assertEqual(options["preparation_timeout"],1000)
                    self.assertEqual(options["timeout"],12)
                    now[0] += 200  # Beyond both former 60/90-second waits.
                    options["before_resume"]({"namespace_pid":42})
                    now[0] += 2000
                    self.assertFalse(options["cancelled"].is_set(), "preparation budget must not become an agent runtime limit")
                else:
                    now[0] += 400
                return dict(stdout=b"",stderr=b"",exit_code=0,timed_out=False,cancelled=False)
            def transport(request):
                deadlines.append(request["preparation_deadline"])
                return SimpleNamespace(inherit_working_directory=True)
            task = SimpleNamespace(artifacts=root,agent_timeout=12,prepare_agent_transport=transport,
                workspace=SimpleNamespace(container=Container(),execute=execute),
                checked=lambda argv,**_:json.dumps(dict(schema_version=1,tree_quiesced=True,token_usage={})).encode() if argv[0] == "cat" else b"")
            request = dict(binary=str(binary),project_root=str(root),tools_root=str(root/"tools"),preparation_seconds=1800,
                bootstrap=dict(runtime_tools=["node"],assets=[]),request=dict(config=dict(agent="pi")))
            execute_agent(task,request,None,None,None)
            self.assertEqual([call[0][-1] for call in calls],["node","pi","__asb-agent"])
            self.assertEqual(deadlines,[1900])

    def test_expired_bootstrap_notifies_before_relay_teardown_and_never_launches_client(self):
        now, order = [0.0], []
        def expired(_task,_request,_cancelled,resources,_relays,budget):
            resources.callback(lambda:order.append("relay-close"))
            now[0] = 1900
            return {},{}
        task = SimpleNamespace(prepare_agent_transport=lambda _:self.fail("expired bootstrap reached client preparation"))
        with patch("asb_guest.time.monotonic",side_effect=lambda:now[0]), patch("asb_guest.prepare_agent",side_effect=expired):
            with self.assertRaisesRegex(ExecutionError,"deadline exceeded"):
                execute_agent(task,dict(preparation_seconds=1800),None,None,None,
                    on_preparation_failure=lambda:order.append("notify-peers"))
        self.assertEqual(order,["notify-peers","relay-close"])

    def test_workload_environment_hook_runs_after_bootstrap_before_client(self):
        with tempfile.TemporaryDirectory() as directory:
            order=[]
            def prepare(*_args):
                order.append('bootstrap')
                return {}, {'PATH':'/base', 'HOME':'/private', 'API_KEY':'fixture'}
            def environment(values, budget):
                order.append('workload-environment')
                self.assertIsNone(budget)
                self.assertEqual(values['API_KEY'], 'fixture')
                return dict(values, PATH='/task/bin:/base')
            def execute(_argv, **options):
                order.append('client')
                self.assertEqual(options['environment']['PATH'], '/task/bin:/base')
                self.assertEqual(options['environment']['HOME'], '/private')
                return {'stdout':b'', 'stderr':b'', 'exit_code':1}
            def unavailable(*_args, **_kwargs):
                raise ExecutionError('result unavailable')
            task=SimpleNamespace(artifacts=Path(directory), agent_timeout=15,
                checked=unavailable, agent_environment=environment,
                workspace=SimpleNamespace(container=Container(), execute=execute,
                    quiesce_unconfirmed_agent=lambda:None))
            with patch('asb_guest.prepare_agent',side_effect=prepare):
                execute_agent(task,{},None,None,None)
            self.assertEqual(order, ['bootstrap','workload-environment','client'])

    def test_owned_service_handoff_uses_guest_metadata_without_reset_or_duplicate_end(self):
        import os
        with tempfile.TemporaryDirectory() as directory:
            controller = os.stat("/proc/self/ns/pid").st_ino
            container = SimpleNamespace(
                attrs={"Config":{"Labels":{"asb.owner":"asb", "asb.attempt":"owned"}}, "HostConfig":{"PidMode":"private"}},
                reload=lambda:None,
                exec_run=lambda argv:SimpleNamespace(exit_code=0, output=str(controller+1).encode()))
            calls, ends = [], []
            def execute(_argv, **options):
                request = json.loads(options["environment"]["ASB_GUEST_AGENT_REQUEST"])
                self.assertEqual(request["workload_owner"], options["workload_owner"])
                self.assertNotEqual(request["workload_owner"]["guest_namespace"], controller)
                result = {"stdout":b"", "stderr":b"", "exit_code":0, "elapsed_seconds":.123,
                    "workload_processes_retained":True, "metadata":{
                        "workload_owner":request["workload_owner"], "tree_quiesced":False}}
                options["on_execution_end"](dict(result))
                calls.append(options)
                return result
            def forbidden(*_args, **_kwargs):
                raise AssertionError("confirmed service handoff must not reset the container or reread unbound metadata")
            task = SimpleNamespace(artifacts=Path(directory), agent_timeout=5, checked=forbidden,
                workspace=SimpleNamespace(attempt="owned", container=container, execute=execute, quiesce_unconfirmed_agent=forbidden))
            with patch("asb_guest.prepare_agent", return_value=({"process_lifetime":"through-evaluation"}, {})):
                result = execute_agent(task, {}, None, None, None, on_ended=ends.append)
            self.assertTrue(result["workload_processes_retained"])
            self.assertEqual(len(calls), 1)
            self.assertEqual(len(ends), 1)
            self.assertEqual(json.loads((task.artifacts / "execution.json").read_text())["agent_tree_cleanup"],
                             "delegated-owned-workload")

    def test_shared_or_unowned_namespace_cannot_retain_services(self):
        import os
        for mode, owner, namespace in [("host", "asb", 99), ("private", "foreign", 99),
                                        ("private", "asb", os.stat("/proc/self/ns/pid").st_ino)]:
            with self.subTest(mode=mode, owner=owner), tempfile.TemporaryDirectory() as directory:
                container = SimpleNamespace(
                    attrs={"Config":{"Labels":{"asb.owner":owner,"asb.attempt":"owned"}}, "HostConfig":{"PidMode":mode}},
                    reload=lambda:None,
                    exec_run=lambda argv:SimpleNamespace(exit_code=0, output=str(namespace).encode()))
                def forbidden(*_args, **_kwargs):
                    raise AssertionError("invalid lifetime launched a client")
                task = SimpleNamespace(artifacts=Path(directory), workspace=SimpleNamespace(
                    attempt="owned", container=container, execute=forbidden))
                with patch("asb_guest.prepare_agent", return_value=({"process_lifetime":"through-evaluation"}, {})):
                    with self.assertRaises(ExecutionError):
                        execute_agent(task, {}, None, None, None)
                self.assertFalse((task.artifacts / "execution.json").exists())

    def test_unconfirmed_tree_cannot_publish_a_result_when_container_reset_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            def unavailable(_argv, **_kwargs):
                raise ExecutionError("guest result is missing")
            def failed_reset():
                raise ExecutionError("could not confirm stopped agent tree")
            task = SimpleNamespace(artifacts=root, agent_timeout=15, checked=unavailable,
                workspace=SimpleNamespace(container=Container(), quiesce_unconfirmed_agent=failed_reset,
                    execute=lambda *_args, **_kwargs: {"stdout":b"", "stderr":b"", "exit_code":2}))
            with patch("asb_guest.prepare_agent", return_value=({}, {})):
                with self.assertRaisesRegex(ExecutionError, "confirm stopped"):
                    execute_agent(task, {}, None, None, None)
            self.assertFalse((root / "execution.json").exists())

    def test_missing_first_result_keeps_nonretained_workspace_eligible_after_confirmed_reset(self):
        # Production guest fallback with a fake task's consuming grader seam;
        # no Docker daemon, original SDK verifier or live provider.
        with tempfile.TemporaryDirectory() as directory:
            calls = []
            def unavailable(_argv, **_kwargs):
                raise ExecutionError("first result publication unavailable")
            def execute(*_args, **_kwargs):
                calls.append("client")
                return {"stdout":b"", "stderr":b"", "exit_code":1}
            task = SimpleNamespace(artifacts=Path(directory), agent_timeout=15,
                checked=unavailable, workspace=SimpleNamespace(container=Container(),
                    execute=execute, quiesce_unconfirmed_agent=lambda:calls.append("confirmed-reset")),
                evaluate=lambda:calls.append("grade"))
            with patch("asb_guest.prepare_agent", return_value=({}, {})):
                result = execute_agent(task, {}, None, None, None)
            self.assertTrue(result["metadata"]["tree_quiesced"])
            self.assertIsNone(result["metadata"]["final_response"])
            self.assertEqual(result["metadata"]["token_usage"], {})
            task.evaluate()
            self.assertEqual(calls, ["client", "confirmed-reset", "grade"])

    def test_missing_guest_result_preserves_bounded_stderr_tail(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory); forwarded=[]
            def unavailable(_argv, **_kwargs):
                raise ExecutionError('result unavailable')
            def execute(_argv, **options):
                options['on_output']('stderr', b'x'*(256*1024))
                options['on_output']('stderr', b'actual guest error')
                return {'stdout':b'', 'stderr':b'x', 'exit_code':1}
            task=SimpleNamespace(artifacts=root,agent_timeout=15,checked=unavailable,
                workspace=SimpleNamespace(container=Container(),execute=execute,
                    quiesce_unconfirmed_agent=lambda:None))
            with patch('asb_guest.prepare_agent',return_value=({},{})):
                execute_agent(task,{},None,lambda stream,chunk:forwarded.append((stream,len(chunk))),None)
            path=root/'guest-error-stderr.log'
            self.assertEqual(path.stat().st_mode & 0o777,0o600)
            self.assertEqual(path.stat().st_size,256*1024)
            self.assertTrue(path.read_bytes().endswith(b'actual guest error'))
            self.assertEqual(len(forwarded),2)
            self.assertEqual(json.loads((root/'execution.json').read_text())['guest_error_log'],path.name)

    def test_failed_guest_with_valid_metadata_preserves_bounded_output_tails(self):
        for code in (0,1):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as directory:
                root=Path(directory)
                def execute(_argv, **options):
                    options['on_output']('stdout',b'x'*(256*1024)+b'important stdout')
                    options['on_output']('stderr',b'important stderr')
                    return {'stdout':b'', 'stderr':b'', 'exit_code':code}
                metadata={'schema_version':1,'exit_code':code,'tree_quiesced':True,
                          'final_response':None,'token_usage':{},'steps':None}
                task=SimpleNamespace(artifacts=root,agent_timeout=15,
                    checked=lambda *_args,**_kwargs:json.dumps(metadata).encode(),
                    workspace=SimpleNamespace(container=Container(),execute=execute))
                with patch('asb_guest.prepare_agent',return_value=({},{})):
                    execute_agent(task,{},None,None,None)
                for stream in ('stdout','stderr'):
                    path=root/f'guest-error-{stream}.log'
                    self.assertEqual(path.exists(),code != 0)
                    if code:
                        self.assertEqual(path.stat().st_mode & 0o777,0o600)
                        self.assertLessEqual(path.stat().st_size,256*1024)
                        self.assertTrue(path.read_bytes().endswith(f'important {stream}'.encode()))

    def test_local_forwarding_precedes_install_and_is_closed_on_install_failure_or_success(self):
        for failure in (False, True):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / ".asb").mkdir()
                (root / ".asb/project.toml").write_text("tools_dir='tools'\n")
                binary = root / "asb"
                binary.write_bytes(elf())
                container = Container()
                container.id = "owned-container-fixture"
                container.exec_run = lambda _: SimpleNamespace(exit_code=1)
                calls, resources = [], []
                class Relay:
                    ports = (10001, 10002)
                    closed = False
                    def __init__(self, targets, command, cancellation, diagnostic_path):
                        self.targets = targets
                        self.cancellation = cancellation
                        self.diagnostic_path = diagnostic_path
                        resources.append(self)
                    def close(self):
                        self.closed = True
                    def check(self):
                        if self.closed:
                            raise ExecutionError("relay closed")
                def execute(argv, **options):
                    self.assertFalse(resources[0].closed)
                    self.assertEqual(options["environment"]["HTTP_PROXY"], "http://localhost:10002")
                    calls.append((argv, options))
                    return {"stdout":b"", "stderr":b"", "exit_code":1 if failure else 0,
                            "timed_out":False, "cancelled":False}
                def checked(argv, **_options):
                    return json.dumps({"schema_version":1,"exit_code":0,"final_response":"done",
                                       "token_usage":{},"steps":None,"tree_quiesced":True}).encode() if argv[0] == "cat" else b""
                task = SimpleNamespace(workspace=SimpleNamespace(container=container, execute=execute),
                                       artifacts=root, checked=checked)
                request = {"binary":str(binary),"project_root":str(root),"tools_root":str(root/"tools"),
                    "project_configuration":"tools_dir='tools'\n[proxy]\nhttp='http://localhost:3128'\n",
                    "environment":{"HTTP_PROXY":"http://localhost:3128"},
                    "bootstrap":{"runtime_tools":[],"assets":[]},
                    "request":{"config":{"agent":"pi"},"connection":{"base_url":"http://localhost:8000/v1"}}}
                with patch("asb_guest.HostRelay", Relay), patch("asb_docker.docker_command", return_value=["docker-fixture"]):
                    if failure:
                        with self.assertRaises(ExecutionError):
                            execute_agent(task,request,None,None,None)
                    else:
                        execute_agent(task,request,None,None,None)
                self.assertTrue(resources[0].closed)
                self.assertEqual(resources[0].targets, [("localhost",8000),("localhost",3128)])
                self.assertIsNone(resources[0].cancellation)
                self.assertEqual(resources[0].diagnostic_path, root / "endpoint-relay-0.json")
                project = next(data for destination,name,data in container.files
                               if destination == "/opt/asb-project/.asb" and name == "project.toml")
                self.assertIn(b"http://localhost:10002",project)
                if not failure:
                    guest = json.loads(calls[-1][1]["environment"]["ASB_GUEST_AGENT_REQUEST"])
                    self.assertEqual(guest["connection"]["base_url"],"http://localhost:10001/v1")

    def test_declared_assets_and_runtime_order_do_not_depend_on_agent_names(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root/"tools"
            source = tools/"example/cache/package.tar.gz"
            source.parent.mkdir(parents=True)
            source.write_bytes(b"public pinned distribution fixture")
            container = Container()
            calls = []
            task = SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root,
                                   checked=lambda argv:calls.append(argv))
            plan = {"runtime_tools":["example-runtime"],"assets":[{"source":str(source),"target":"example/cache/package.tar.gz"}]}
            self.assertEqual(stage_bootstrap(task,plan,tools),(["example-runtime"],[]))
            self.assertEqual(container.files,[("/opt/asb-project/.asb-tools","example/cache/package.tar.gz",source.read_bytes())])
            receipt = json.loads((root/"bootstrap.json").read_text())
            self.assertNotIn(str(root),json.dumps(receipt))
            self.assertEqual(receipt["assets"][0]["bytes"],len(source.read_bytes()))

    def test_native_executable_is_staged_with_loader_and_skips_guest_install(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); tools = root/'tools'; source=tools/'example/bin/example'
            source.parent.mkdir(parents=True); source.write_bytes(b'native fixture')
            container=Container(); calls=[]
            task=SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root,
                                 checked=lambda argv:calls.append(argv))
            files=[('tools/example/example',b'loader wrapper',0o755),('tools/example/bin/example',b'native fixture',0o755)]
            receipt={'files':[{'name':name,'bytes':len(data),'sha256':hashlib.sha256(data).hexdigest()} for name,data,_ in files]}
            asset={'source':str(source),'target':'example/bin/example','native_executable':True}
            with patch('asb_guest.bundle_tool',return_value=(files,receipt)):
                runtimes,native=stage_bootstrap(task,{'runtime_tools':[],'assets':[asset]},tools)
            self.assertEqual((runtimes,native),([],['example']))
            launcher=next(data for destination,name,data in container.files if name=='example/bin/example')
            self.assertIn(b'/example/native-runtime/tools/example/example',launcher)
            evidence=json.loads((root/'bootstrap.json').read_text())
            self.assertEqual(evidence['native_executables'][0]['files'],receipt['files'])
            self.assertNotIn(str(root),json.dumps(evidence))

            untouched=Container()
            task.workspace.container=untouched
            with patch('asb_guest.bundle_tool',return_value=(files,receipt)), self.assertRaises(ExecutionError):
                stage_bootstrap(task,{'runtime_tools':[],'assets':[asset,{'source':str(source),'target':'../escape'}]},tools)
            self.assertEqual(untouched.files,[])

    def test_bootstrap_rejects_escape_symlinks_duplicate_targets_and_invalid_dependencies(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root/"tools"
            tools.mkdir()
            source = tools/"package"
            source.write_bytes(b"fixture")
            link = tools/"link"
            link.symlink_to(source)
            external = root/"private"
            external.write_bytes(b"must-not-be-staged")
            assets = [{"source":str(source),"target":"../escape"},{"source":str(source),"target":"/escape"},
                      {"source":str(external),"target":"package"},{"source":str(link),"target":"package"},
                      {"source":str(source),"target":"package","native_executable":True},
                      {"source":str(source),"target":"package","native_executable":"yes"}]
            for asset in assets:
                container = Container()
                task = SimpleNamespace(workspace=SimpleNamespace(container=container),artifacts=root,checked=lambda _:None)
                with self.subTest(asset=asset),self.assertRaises(ExecutionError):
                    stage_bootstrap(task,{"runtime_tools":[],"assets":[asset]},tools)
                self.assertEqual(container.files,[])
            valid = {"source":str(source),"target":"package"}
            for plan in [{"runtime_tools":[],"assets":[valid,valid]},
                         {"runtime_tools":["runtime","runtime"],"assets":[]},
                         {"runtime_tools":["../bad"],"assets":[]},
                         {"runtime_tools":[],"assets":[],"public_package_cache":"not-a-boolean"}, {}]:
                with self.assertRaises(ExecutionError):
                    stage_bootstrap(task,plan,tools)

    def test_runtime_parser_accepts_static_elf_and_rejects_malformed_files(self):
        with tempfile.TemporaryDirectory() as directory:
            binary = Path(directory) / "asb"
            binary.write_bytes(elf())
            self.assertEqual(inspect_elf(binary),(elf(),62,None))
            for invalid in [elf(True), b"#!/bin/sh\n", elf()[:100]]:
                binary.write_bytes(invalid)
                with self.assertRaises(ExecutionError):
                    inspect_elf(binary)

    def test_registry_cache_with_credentials_is_never_staged(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            cache = root / ".asb-cache/npm/_cacache/index-v5"
            cache.mkdir(parents=True)
            (cache / "entry").write_text('hash\t' + json.dumps({"metadata":{"reqHeaders":{"Authorization":"private-fixture"}}}))
            container = Container()
            stage_public_cache(container, root)
            self.assertEqual(container.files, [])
            (cache / "entry").write_text('hash\t' + json.dumps({"metadata":{"reqHeaders":{}}}))
            stage_public_cache(container, root)
            self.assertTrue(any(name == "npm/_cacache/index-v5/entry" for _, name, _ in container.files))

    def test_npm_package_tree_is_bounded_selected_and_never_a_host_mount(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            archive = tools / "cline/cache/guest-node-modules.tar"
            archive.parent.mkdir(parents=True)
            with tarfile.open(archive, "w") as output:
                directory_member = tarfile.TarInfo("node_modules/.bin")
                directory_member.type = tarfile.DIRTYPE
                output.addfile(directory_member)
                payload = b"#!/bin/sh\nexit 0\n"
                target = tarfile.TarInfo("node_modules/@cline/cli-linux-x64/bin/cline")
                target.mode = 0o755
                target.size = len(payload)
                output.addfile(target, io.BytesIO(payload))
                launcher = tarfile.TarInfo("node_modules/.bin/cline")
                launcher.type = tarfile.SYMTYPE
                launcher.linkname = "../@cline/cli-linux-x64/bin/cline"
                output.addfile(launcher)
                postinstall = tarfile.TarInfo("node_modules/cline/bin/.cline")
                postinstall.type = tarfile.LNKTYPE
                postinstall.linkname = "node_modules/@cline/cli-linux-x64/bin/cline"
                output.addfile(postinstall)
            container = Container()
            checked = []
            task = SimpleNamespace(workspace=SimpleNamespace(container=container), artifacts=root,
                                   checked=lambda argv: checked.append(argv))
            tree = {"source":str(archive), "target":"cline", "executable":"cline"}
            stage_npm_package_tree(task, tree, tools, "cline")
            self.assertEqual(checked, [
                ["mkdir", "-p", "/opt/asb-project/.asb-tools/cline"],
                ["test", "-x", "/opt/asb-project/.asb-tools/cline/node_modules/.bin/cline"],
            ])
            self.assertTrue(any(destination == "/opt/asb-project/.asb-tools/cline"
                                and name == "node_modules/.bin/cline"
                                for destination,name,_ in container.files))
            receipt = json.loads((root / "npm-package-tree.json").read_text())
            self.assertEqual(receipt["target"], "cline")
            self.assertNotIn(str(tools), json.dumps(receipt))
            with self.assertRaises(ExecutionError):
                stage_npm_package_tree(task, tree, tools, "other-agent")

    def test_npm_package_tree_accepts_larger_payload_and_enforces_unpacked_bound(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            archive = tools / "opencode/cache/guest-node-modules.tar"
            archive.parent.mkdir(parents=True)
            with tarfile.open(archive, "w") as output:
                payload = b"x" * 600
                member = tarfile.TarInfo("node_modules/example/payload")
                member.size = len(payload)
                output.addfile(member, io.BytesIO(payload))
            task = SimpleNamespace(workspace=SimpleNamespace(container=Container()),
                                   checked=lambda argv: None, artifacts=root)
            tree = {"source": str(archive), "target": "opencode", "executable": "opencode"}
            with patch("asb_guest.NPM_TREE_UNPACKED_LIMIT", 1024):
                stage_npm_package_tree(task, tree, tools, "opencode")
            with patch("asb_guest.NPM_TREE_UNPACKED_LIMIT", 512):
                with self.assertRaisesRegex(ExecutionError, "unpacked bound"):
                    stage_npm_package_tree(task, tree, tools, "opencode")

    def test_python_package_tree_stages_only_selected_bounded_archives(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tools = root / "tools"
            cache = tools / "openjiuwen/cache"
            cache.mkdir(parents=True)
            runtime, packages = cache / "runtime.tar.gz", cache / "packages.tar.gz"
            for path, name in [(runtime, "cpython/bin/python3.12"), (packages, "bin/openjiuwen")]:
                with tarfile.open(path, "w:gz") as archive:
                    payload = b"fixture"
                    entry = tarfile.TarInfo(name)
                    entry.size, entry.mode = len(payload), 0o755
                    archive.addfile(entry, io.BytesIO(payload))
            container, calls = Container(), []
            task = SimpleNamespace(workspace=SimpleNamespace(container=container, image_identity={"architecture":"amd64"}), checked=lambda argv: calls.append(argv))
            tree = {"runtime":str(runtime), "packages":str(packages), "runtime_name":"cpython",
                    "version":"3.12", "architecture":"amd64", "target":"openjiuwen", "executable":"openjiuwen"}
            stage_python_package_tree(task, tree, tools, "openjiuwen")
            self.assertEqual(len(container.files), 2)
            self.assertTrue(any("--without-pip" in call for call in calls))
            with self.assertRaises(ExecutionError):
                stage_python_package_tree(task, tree, tools, "other")
            with self.assertRaises(ExecutionError):
                stage_python_package_tree(task, dict(tree, architecture="arm64"), tools, "openjiuwen")
            with tarfile.open(packages, "w:gz") as archive:
                entry = tarfile.TarInfo("bin/escape")
                entry.type, entry.linkname = tarfile.SYMTYPE, "../../outside"
                archive.addfile(entry)
            with self.assertRaisesRegex(ExecutionError, "escaping link"):
                stage_python_package_tree(task, tree, tools, "openjiuwen")

    def test_bootstrap_uses_exact_selected_tool_and_private_request_not_argv(self):
        for timed_out, measured, confirmed in [(False, False, True), (True, False, False), (False, True, True), (False, False, False)]:
            with self.subTest(timed_out=timed_out, measured=measured), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / ".asb").mkdir()
                (root / ".asb/project.toml").write_text("tools_dir='tools'\n")
                binary = root / "asb"
                binary.write_bytes(elf())
                container = Container()
                calls = []
                probes, attached = [], []
                def probe(argv):
                    probes.append(argv)
                    return SimpleNamespace(exit_code=0, output=b'{"namespace_pid":42,"start_time_ticks":123,"boot_id":"fixture"}')
                container.exec_run = probe

                def execute(argv, **options):
                    calls.append((argv, options))
                    if argv[-1] == "__asb-agent":
                        options["before_resume"]({"namespace_pid":42})
                    return {"stdout":b"out", "stderr":b"err", "exit_code":124 if timed_out and argv[-1] == "__asb-agent" else 0,
                            "timed_out":timed_out and argv[-1] == "__asb-agent", "cancelled":False}

                def checked(argv, **_options):
                    if argv[0] == "cat":
                        if timed_out:
                            raise ExecutionError("no completed metadata")
                        return json.dumps({"schema_version":1,"exit_code":0,"final_response":"done", "token_usage":{},"steps":None,
                                           "tree_quiesced":confirmed}).encode()
                    return b""

                resets = []
                task = SimpleNamespace(workspace=SimpleNamespace(container=container, execute=execute,
                                       quiesce_unconfirmed_agent=lambda:resets.append("stopped")),
                                       artifacts=root, checked=checked, agent_timeout=1234)
                request = {"binary":str(binary), "project_root":str(root), "tools_root":str(root / "tools"),
                           "measure_target":measured,
                           "bootstrap":{"runtime_tools":["node"],"assets":[]},
                           "request":{"config":{"agent":"pi"},"connection":{"credential":"private-fixture"}}}
                ended = []
                result = execute_agent(task, request, attached.append, lambda *_: None, None,
                                       on_ended=lambda result: ended.append(result["exit_code"]))
                self.assertEqual(resets, [] if confirmed else ["stopped"])
                self.assertTrue(result["metadata"]["tree_quiesced"])
                journal = json.loads((root / "execution.json").read_text())
                self.assertEqual(journal["agent_tree_cleanup"], "supervisor-confirmed" if confirmed else "owned-container-reset")
                self.assertEqual([call[0][-1] for call in calls], ["node", "pi", "__asb-agent"])
                self.assertNotIn("private-fixture", repr([call[0] for call in calls]))
                private = calls[-1][1]["environment"]["ASB_GUEST_AGENT_REQUEST"]
                self.assertEqual(json.loads(private)["connection"]["credential"], "private-fixture")
                self.assertEqual(json.loads(private)["timeout_seconds"], 1234)
                self.assertEqual(calls[-1][1]["timeout"], 1234)
                self.assertEqual(calls[-1][1]["gate"], "application")
                self.assertEqual(calls[-1][1]["preparation_timeout"], 90)
                self.assertEqual(ended, [124 if timed_out else 0])
                self.assertEqual(len(probes), int(measured))
                self.assertEqual(len(attached), 1)
                if measured:
                    self.assertEqual(probes[0], ["/opt/asb-runtime/asb", "__asb-target", "42"])
                    self.assertEqual(attached[0]["process_identity"]["start_time_ticks"], 123)
                self.assertIsNone(result["metadata"]["token_usage"].get("total_tokens"))
                self.assertEqual(result["metadata"]["exit_code"], 124 if timed_out else 0)


if __name__ == "__main__":
    unittest.main()
