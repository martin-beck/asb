"""Actual native and guest A/B artifact isolation, entirely local fixtures."""
import json
import os
from pathlib import Path
import shutil
import shlex
import signal
import subprocess
import sys
import tempfile
import time

ASB = Path(sys.argv[1]).resolve()
CRATE = Path(__file__).resolve().parents[1]
ARTIFACTS = CRATE / "target"
ARTIFACTS.mkdir(exist_ok=True)
ROOT = Path(tempfile.mkdtemp(prefix="agent-artifact-repro-", dir=ARTIFACTS))
PERMISSION_JOURNALS = []


def restore_fixture_permissions():
    # Keep evidence, but end the permission fault after all scenario assertions.
    # Only the exact directories recorded before our fake client's chmod qualify;
    # never recursively chmod a tree or follow a replacement symlink.
    journals = PERMISSION_JOURNALS[:]
    PERMISSION_JOURNALS.clear()
    failures = []
    for journal in journals:
        if not journal.exists():
            continue
        for line in journal.read_text().splitlines():
            try:
                entry = json.loads(line)
                path = Path(entry["path"])
                assert path.is_relative_to(journal.parent), "foreign permission journal path"
                descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
                try:
                    actual = os.fstat(descriptor)
                    assert (actual.st_dev, actual.st_ino) == (int(entry["dev"]), int(entry["ino"])), "permission target replaced"
                    os.fchmod(descriptor, entry["mode"])
                    assert os.fstat(descriptor).st_mode & 0o777 == entry["mode"]
                finally:
                    os.close(descriptor)
            except Exception as error:
                failures.append(str(error))
    assert not failures, ("fixture permission restoration failed", failures)


def project(name, scenario="normal"):
    root = ROOT / name
    (root / ".asb").mkdir(parents=True)
    PERMISSION_JOURNALS.append(root / "permission-faults.jsonl")
    (root / ".asb/project.toml").write_text(
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n"
    )
    shutil.copytree(CRATE / "assets/workloads", root / "workloads")
    shutil.copytree(CRATE / "assets/workloads/fixtures", root / ".asb/fixtures")
    binary = root / "tools/mini-swe/bin/mini"
    binary.parent.mkdir(parents=True)
    node = shutil.which("node")
    assert node
    # The second invocation emits nothing. A seals the workspace against
    # unlink, exercising the existing ignored remove_file failure naturally.
    binary.write_text(f"#!{node}\n" + r"""
const fs=require('node:fs'), path=require('node:path');
const index=process.argv.indexOf('--output');
if (index<0 || !process.argv[index+1]) throw Error('missing private artifact output');
const destination=path.resolve(process.argv[index+1]);
const home=path.resolve(process.env.HOME);
const fixtureRoot=path.resolve(process.env.ASB_FIXTURE_ROOT);
const outputRoot=process.env.ASB_ARTIFACT_NATIVE
 ? path.join(fixtureRoot,'results','benchmark-runs')
 : path.join(fixtureRoot,'private');
if (!destination.startsWith(outputRoot+path.sep) || !destination.endsWith(path.sep+path.join('mini-swe','trajectory.json')))
 throw Error('artifact output outside fixture-owned output root');
const marker=path.join(fixtureRoot,'executions');
function permissionFault(directory,mode) {
 const before=fs.statSync(directory,{bigint:true});
 fs.appendFileSync(path.join(fixtureRoot,'permission-faults.jsonl'),JSON.stringify({
  path:directory,dev:String(before.dev),ino:String(before.ino),mode:Number(before.mode & 0o777n)
 })+'\n',{mode:0o600});
 fs.chmodSync(directory,mode);
}
const previous=fs.existsSync(marker)?fs.readFileSync(marker,'utf8'):'';
fs.appendFileSync(marker,'called\n');
if (!previous) {
 fs.mkdirSync(path.dirname(destination),{recursive:true});
 if (process.env.ASB_ARTIFACT_SCENARIO==='symlink') {
   const target=path.join(process.env.HOME,'foreign.json');
   fs.writeFileSync(target,'{"answer":"foreign bytes"}');fs.symlinkSync(target,destination);
 } else if (process.env.ASB_ARTIFACT_SCENARIO==='malformed') {
   fs.writeFileSync(destination,'{"partial":');
 } else if (process.env.ASB_ARTIFACT_SCENARIO==='oversized') {
   fs.writeFileSync(destination,'x'.repeat(16*1024*1024+1));
 } else fs.writeFileSync(destination,JSON.stringify({trajectory_format:'mini-swe-agent-1.1',
 info:{exit_status:'Submitted',submission:'src/cli.py src/registry.py lookup',model_stats:{api_calls:4}},messages:[{role:'assistant',
 content:'src/cli.py src/registry.py lookup',
 extra:{response:{usage:{prompt_tokens:13,completion_tokens:21}}}}]}));
 permissionFault(process.cwd(),0o555);
 if (process.env.ASB_ARTIFACT_SCENARIO==='cleanup') permissionFault(path.dirname(destination),0o500);
}
if (process.env.ASB_ARTIFACT_NATIVE) {
 fetch(process.env.OPENAI_BASE_URL+'/chat/completions',{method:'POST',headers:{'content-type':'application/json'},
 body:JSON.stringify({model:'fixture',messages:[],tools:[{type:'function',function:{name:'fixture',parameters:{type:'object'}}}]})})
 .then(async response=>{if(!response.ok)throw Error('fixture replay failed');await response.text();})
 .catch(()=>{process.exitCode=9;});
}
""")
    binary.chmod(0o700)
    config = {
        "agent": "mini-swe", "provider": "openrouter", "model": "fixture",
        "workload": "repository-navigation", "workspace": str(root / "workspace"),
        "log_file": str(root / "run.jsonl"),
    }
    (root / "workspace").mkdir()
    (root / "workspace/.asb-mini-swe.traj.json").write_text("foreign legacy workspace file")
    return root, config


def environment(root, scenario="normal"):
    return {"HOME": str(root), "PATH": os.environ["PATH"], "LANG": "C.UTF-8",
        "ASB_ARTIFACT_SCENARIO":scenario, "ASB_FIXTURE_ROOT":str(root)}


def native_result_directory(root, index):
    # Public direct runs never reuse the configured workspace, log, or
    # measurements paths.  They atomically own a benchmark-scoped generation
    # instead, so a later invocation cannot replace this attempt's evidence.
    return root / "results" / "benchmark-runs" / f"run-{index:03}"


def deferred_collection(run, record):
    """Read the guest's one sealed adapter-collection receipt.

    Isolated guests cannot safely interpret an adapter's raw evidence after it
    crosses the process boundary.  They export the generic frozen envelope;
    the host-side adapter collects the final answer, usage and steps only after
    it has validated the receipt.  Keep this fixture at that boundary rather
    than accidentally asserting controller-owned fields in the guest record.
    """
    receipts = record["exported_artifacts"]
    assert len(receipts) == 1, receipts
    receipt = receipts[0]
    assert receipt["kind"] == "asb-deferred-adapter-collection", receipt
    filename = receipt["filename"]
    assert filename == "artifact-0", receipt
    artifact = run / "exports" / filename
    assert artifact.is_file() and not artifact.is_symlink(), artifact
    payload = json.loads(artifact.read_text())
    assert payload["schema_version"] == 1, payload
    assert set(payload) == {"schema_version", "documents", "bytes"}, payload
    return payload


def native(scenario="normal"):
    root, config = project("native-"+scenario)
    config_path = root / "benchmark.toml"
    config_path.write_text("".join(f"{key}={json.dumps(value)}\n" for key, value in config.items()))
    # The actual client contacts only ASB's loopback replay fixture.
    tape = root / "tape.jsonl"
    tape.write_text(json.dumps({"request":{"method":"POST","url":"/v1/chat/completions",
        "body":json.dumps({"model":"fixture","messages":[],"tools":[{"type":"function","function":{"name":"fixture","parameters":{"type":"object"}}}]})},"response":{"status":200,"headers":{},"body":"{}"}})+"\n")
    records = []
    for _ in range(2):
        if (root / "workspace").exists():
            (root / "workspace").chmod(0o755)
        env = environment(root,"cleanup" if scenario.startswith("cleanup") else scenario)
        env["ASB_ARTIFACT_NATIVE"] = "1"
        # The production capacity guard is intentionally fail-closed for a
        # detached process.  Exercise its actual confirmation path through a
        # PTY instead of bypassing the guard in this native artifact fixture.
        # `--json` intentionally disables prompts, so omit it here: this
        # fixture validates retained artifacts rather than CLI JSON output.
        invocation = [str(ASB), "run", str(config_path), "--llm-replay",
                      "--cassette", str(tape)]
        result = subprocess.run(
            ["script", "--quiet", "--return", "--flush", "--command",
             shlex.join(invocation), "/dev/null"],
            cwd=root, env=env, input=b"y\n", capture_output=True, timeout=30,
        )
        (root / f"invocation-{len(records)}.stderr").write_bytes(result.stderr)
        (root / f"invocation-{len(records)}.stdout").write_bytes(result.stdout)
        result_directory = native_result_directory(root, len(records) + 1)
        records.append(json.loads((result_directory / "run.log").read_text().splitlines()[-1]))
    assert records[0]["token_usage"]["total_tokens"] == 34, records[0]
    assert records[0]["steps"] == 4, records[0]
    assert records[0]["workload_success"] is True, records[0]
    assert records[1]["token_usage"]["total_tokens"] is None, (
        "native B inherited A usage", records[1]["token_usage"])
    assert records[1]["steps"] is None, "native B inherited A steps"
    assert records[1]["workload_success"] is False, "native B inherited A answer"
    assert native_result_directory(root, 1).is_dir()
    assert native_result_directory(root, 2).is_dir()
    if scenario.startswith("cleanup"):
        assert "scratch settlement failed" in records[0]["failure_reason"], records[0]
        assert records[0]["benchmark_outcome"]["cleanup_confirmed"] is True
        assert records[0]["workload_success"] is True
        assert list(root.glob("**/mini-swe/trajectory.json")), "retain failed cleanup evidence"
    assert (root / "executions").read_text().splitlines() == ["called","called"]
    assert (root / "workspace/.asb-mini-swe.traj.json").read_text() == "foreign legacy workspace file"


def guest(scenario="normal"):
    root, config = project("guest-"+scenario)
    records = []
    for index in range(2):
        (root / "workspace").chmod(0o755)
        runtime = root / "private" / f"runtime-{index}"
        run = runtime / "attempt"
        request = {"config":config,"connection":{"interface":"open_ai_chat",
            "authentication":"bearer","base_url":"http://127.0.0.1:1/v1",
            "credential":"fixture"},"prompt":"offline task","project_root":str(root),
            "runtime_dir":str(runtime),"run_dir":str(run)}
        if scenario == "publication" and index == 0:
            (run / "agent-result.json").mkdir(parents=True)
        env = environment(root,scenario)
        env["ASB_GUEST_AGENT_REQUEST"] = json.dumps(request)
        child = subprocess.Popen([str(ASB),"__asb-agent"],cwd=root,env=env,
            stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        try:
            ready = child.stdout.readline()
            assert ready.strip() == f"__ASB_GATE__:{child.pid}".encode(), ready
            deadline = time.monotonic()+5
            while "\nState:\tT" not in Path(f"/proc/{child.pid}/status").read_text():
                assert time.monotonic() < deadline, "guest never stopped"
                time.sleep(.005)
            child.send_signal(signal.SIGCONT)
            _, stderr = child.communicate(timeout=10)
            (root / f"invocation-{index}.stderr").write_bytes(stderr)
            if scenario == "publication" and index == 0:
                assert child.returncode != 0, "publication blocker must fail"
                raw = json.loads((run / "mini-swe/trajectory.json").read_text())
                assert raw["info"]["model_stats"]["api_calls"] == 4, "failed publication must retain raw evidence"
                return
            assert child.returncode == 0, stderr
            records.append(json.loads((run / "agent-result.json").read_text()))
        finally:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=5)
    if scenario in ["symlink","malformed","oversized"]:
        assert records[0]["artifact_error"], records[0]
        assert records[0]["tree_quiesced"] is True
        assert records[0]["final_response"] is None
        assert records[0]["token_usage"]["total_tokens"] is None
        if scenario == "symlink":
            assert (root / "private/runtime-0/agent-state/home/foreign.json").read_text() == '{"answer":"foreign bytes"}'
            assert (root / "private/runtime-0/attempt/mini-swe/trajectory.json").is_symlink()
        return
    first = deferred_collection(root / "private/runtime-0/attempt", records[0])
    # The direct guest result deliberately contains no adapter-interpreted
    # fields.  Its host controller reconstructs this exact frozen document and
    # asks the Mini-SWE adapter to derive the public answer, usage and steps.
    assert records[0]["final_response"] is None, records[0]
    assert records[0]["token_usage"]["total_tokens"] is None, records[0]
    assert records[0]["steps"] is None, records[0]
    trajectory = first["documents"]["trajectory.json"]
    assert trajectory["messages"][0]["content"] == "src/cli.py src/registry.py lookup", trajectory
    assert trajectory["messages"][0]["extra"]["response"]["usage"] == {
        "prompt_tokens": 13, "completion_tokens": 21
    }, trajectory
    assert trajectory["info"]["model_stats"]["api_calls"] == 4, trajectory
    assert records[1]["final_response"] is None, ("guest B inherited A answer", records[1])
    assert records[1]["token_usage"]["total_tokens"] is None, "guest B inherited A usage"
    assert records[1]["steps"] is None, "guest B inherited A steps"
    second = deferred_collection(root / "private/runtime-1/attempt", records[1])
    assert second["documents"] == {}, "guest B inherited A adapter evidence"
    assert second["bytes"] == {}, "guest B inherited A opaque evidence"
    if scenario == "cleanup":
        assert "scratch settlement failed" in records[0]["artifact_error"]
        assert records[0]["tree_quiesced"] is True
        assert (root / "private/runtime-0/attempt/mini-swe/trajectory.json").is_file()
    assert (root / "executions").read_text().splitlines() == ["called","called"]
    assert (root / "workspace/.asb-mini-swe.traj.json").read_text() == "foreign legacy workspace file"


failures = []
cases = [("native",native),("guest",guest)]
if "--extended" in sys.argv:
    cases += [("native-cleanup",lambda:native("cleanup"))]
    cases += [("guest-"+kind,lambda kind=kind:guest(kind)) for kind in ["cleanup","publication","symlink","malformed","oversized"]]
for name, operation in cases:
    try:
        try:
            operation()
        finally:
            restore_fixture_permissions()
        print(f"{name}: PASS",flush=True)
    except Exception as error:
        failures.append(name)
        print(f"{name}: FAIL {error}",flush=True)
print(f"Retained fixture evidence: {ROOT}",flush=True)
sys.exit(bool(failures))
