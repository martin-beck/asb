//! Loopback-only regression for the Goose adapter's record/replay routing.
#![cfg(unix)]

mod support;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use support::IsolatedEnvironment;

struct MockProvider {
    endpoint: String,
    requests: Arc<Mutex<Vec<String>>>,
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl MockProvider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(AtomicBool::new(false));
        let (observed, stop) = (Arc::clone(&requests), Arc::clone(&done));
        let thread = std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut received = Vec::new();
                        let mut buffer = [0u8; 8192];
                        loop {
                            let size = stream.read(&mut buffer).unwrap();
                            assert!(size > 0, "incomplete mock provider request");
                            received.extend_from_slice(&buffer[..size]);
                            assert!(received.len() < 1024 * 1024, "oversized mock request");
                            if let Some(end) =
                                received.windows(4).position(|part| part == b"\r\n\r\n")
                            {
                                let headers = String::from_utf8_lossy(&received[..end]);
                                let length = headers
                                    .lines()
                                    .filter_map(|line| line.split_once(':'))
                                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                                    .unwrap_or(0);
                                if received.len() >= end + 4 + length {
                                    break;
                                }
                            }
                        }
                        let head = String::from_utf8_lossy(&received);
                        let line = head.lines().next().unwrap_or("").to_owned();
                        observed.lock().unwrap().push(line.clone());
                        let body = if line.starts_with("GET /v1/models ") {
                            r#"{"data":[{"id":"test-model","supported_parameters":["tools"]}]}"#
                        } else {
                            r#"{"choices":[{"message":{"role":"assistant","content":"done"}}]}"#
                        };
                        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("mock provider: {error}"),
                }
            }
        });
        Self {
            endpoint,
            requests,
            done,
            thread: Some(thread),
        }
    }
}
impl Drop for MockProvider {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn run(root: &Path, endpoint: &str, args: &[&str]) -> Output {
    run_with_changed_request(root, endpoint, args, false)
}

fn run_with_changed_request(root: &Path, endpoint: &str, args: &[&str], changed: bool) -> Output {
    // The capacity admission guard deliberately requires a terminal and an
    // explicit answer when its conservative estimate exceeds free space. Run
    // this fixture through util-linux `script`, which gives the child a PTY,
    // and supply that answer. This keeps the production binary's fail-closed
    // non-interactive behavior covered rather than adding a test-only bypass.
    let mut command_line = shell_quote(env!("CARGO_BIN_EXE_asb"));
    for argument in args {
        command_line.push(' ');
        command_line.push_str(&shell_quote(argument));
    }
    let mut command = Command::new("script");
    command
        .args(["--quiet", "--return", "--flush", "--command"])
        .arg(command_line)
        .arg("/dev/null")
        .current_dir(root)
        .isolated_env()
        .env("HOME", root)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("OPENROUTER_BASE_URL", endpoint)
        .env("OPENROUTER_API_KEY", "fixture-key")
        .env("ASB_TEST_UPSTREAM", endpoint.trim_end_matches("/v1"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if changed {
        command.env("ASB_TEST_CHANGED", "1");
    }
    let mut child = command.spawn().expect("ASB starts in a PTY");
    child
        .stdin
        .take()
        .expect("PTY launcher has stdin")
        .write_all(b"y\n")
        .expect("answer capacity confirmation");
    child.wait_with_output().expect("ASB exits")
}

fn shell_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', "'\\\"'\\\"'"))
}

#[test]
fn goose_routes_record_and_replay_through_tape_without_live_credentials() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("asb-goose-tape-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = MockProvider::start();
    let init = run(&root, &provider.endpoint, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::create_dir_all(root.join("asb-workspace/src")).unwrap();
    std::fs::write(
        root.join("asb-workspace/src/parser.py"),
        "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n",
    )
    .unwrap();
    let agent_dir = root.join(".asb-tools/goose/bin");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let agent = agent_dir.join("goose");
    std::fs::write(&agent, r##"#!/usr/bin/env node
const host = process.env.OPENAI_HOST;
if (!/^http:\/\/127\.0\.0\.1:\d+$/.test(host || '') ||
    (process.env.ASB_TAPE_MODE ? host === process.env.ASB_TEST_UPSTREAM : host !== process.env.ASB_TEST_UPSTREAM)) {
  console.error('Goose endpoint is not the expected loopback service'); process.exit(7);
}
const expectedKey = process.env.ASB_TAPE_MODE === 'replay' ? 'asb-replay' : 'fixture-key';
if (process.env.OPENAI_API_KEY !== expectedKey) { console.error('incorrect key'); process.exit(8); }
if (process.env.ASB_TAPE_MODE === 'replay' &&
    (process.env.OPENROUTER_API_KEY !== 'asb-replay' || process.env.ANTHROPIC_API_KEY !== 'asb-replay')) {
  console.error('replay exposed a live credential'); process.exit(9);
}
async function invokeExtension(extension) {
  return new Promise((resolve, reject) => {
    const child = require('child_process').spawn('/bin/sh', ['-c', extension], {stdio:['pipe', 'pipe', 'pipe']});
    let output = '';
    let error = '';
    child.stdout.on('data', chunk => {
      output += chunk;
    });
    child.stderr.on('data', chunk => { error += chunk; });
    child.on('error', reject);
    child.on('close', () => output.includes('"content"') ? resolve() : reject(new Error(error)));
    child.stdin.end(JSON.stringify({jsonrpc:'2.0', id:1, method:'tools/call', params:{name:'write_file', arguments:{path:'src/parser.py', content:"def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n"}}}) + '\n');
  });
}
async function main() {
if (process.env.ASB_TAPE_MODE) {
  const extensionIndex = process.argv.indexOf('--with-extension');
  if (extensionIndex < 0 || !process.argv.includes('--no-profile')) {
    console.error('Goose did not receive the private ASB extension'); process.exit(10);
  }
  const extension = process.argv[extensionIndex + 1];
  await invokeExtension(extension);
} else {
  require('fs').writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
}
const body = {model:'test-model', messages:[{role:'user', content:process.env.ASB_TEST_CHANGED ? 'changed' : 'test'}], tools:[{type:'function'}]};
const response = await fetch(host + process.env.OPENAI_BASE_PATH, {method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(body)});
if (!response.ok) throw new Error(await response.text());
console.log('{"status":"ok"}');
}
main().catch(e => { console.error(e); process.exitCode = 1; });
"##).unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("benchmark.toml"), "agent='goose'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='test-model'\nconcurrency=1\n").unwrap();
    let record = run(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--record",
            "--cassette",
            "tape.jsonl",
        ],
    );
    assert!(
        record.status.success(),
        "record: stdout={} stderr={}",
        String::from_utf8_lossy(&record.stdout),
        String::from_utf8_lossy(&record.stderr)
    );
    let mut archive = asb_cli::cassette::Archive::open(
        &root.join("tape.jsonl"),
        asb_cli::cassette::Limits::default(),
    )
    .unwrap();
    assert_eq!(archive.manifest().tracks.len(), 1);
    assert!(archive.manifest().tracks[0].complete);
    assert!(
        !archive.read_artifact(1, "local-tools").unwrap().is_empty(),
        "Goose recording must retain its local-tool transcript for offline replay"
    );
    let cassette = String::from_utf8(archive.read_track(1).unwrap()).unwrap();
    assert_eq!(cassette.lines().count(), 1);
    let before_replay = provider.requests.lock().unwrap().clone();
    assert!(
        before_replay
            .iter()
            .any(|line| line.starts_with("GET /v1/models "))
    );
    assert_eq!(
        before_replay
            .iter()
            .filter(|line| line.starts_with("POST /v1/chat/completions "))
            .count(),
        1
    );
    let replay = run(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
    );
    assert!(
        replay.status.success(),
        "replay: stdout={} stderr={}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(
        *provider.requests.lock().unwrap(),
        before_replay,
        "replay must not contact the live upstream"
    );
    let mismatch = run_with_changed_request(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
        true,
    );
    assert!(
        !mismatch.status.success(),
        "changed replay request must fail"
    );
    assert!(
        String::from_utf8_lossy(&mismatch.stdout).contains("rejected")
            || String::from_utf8_lossy(&mismatch.stderr).contains("rejected"),
        "mismatch: stdout={} stderr={}",
        String::from_utf8_lossy(&mismatch.stdout),
        String::from_utf8_lossy(&mismatch.stderr)
    );
    assert_eq!(
        *provider.requests.lock().unwrap(),
        before_replay,
        "a mismatched request must not fall back to the upstream"
    );
    let missing = run(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "missing.jsonl",
        ],
    );
    assert!(!missing.status.success(), "a missing cassette must fail");
    assert_eq!(
        *provider.requests.lock().unwrap(),
        before_replay,
        "a missing cassette must not fall back to the upstream"
    );
    let live = run(&root, &provider.endpoint, &["run", "benchmark.toml"]);
    assert!(
        live.status.success(),
        "live: stdout={} stderr={}",
        String::from_utf8_lossy(&live.stdout),
        String::from_utf8_lossy(&live.stderr)
    );
    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|line| line.starts_with("POST /v1/chat/completions "))
            .count(),
        2,
        "normal execution must still contact its configured provider"
    );
    drop(requests);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn workbuddy_routes_record_and_replay_through_inline_private_mcp() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        std::env::temp_dir().join(format!("asb-workbuddy-tape-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let provider = MockProvider::start();
    let init = run(&root, &provider.endpoint, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::create_dir_all(root.join("asb-workspace/src")).unwrap();
    std::fs::write(
        root.join("asb-workspace/src/parser.py"),
        "def parse_path(value: str) -> list[str]:\n    return []\n",
    )
    .unwrap();
    let agent_dir = root.join(".asb-tools/workbuddy/node_modules/.bin");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let agent = agent_dir.join("codebuddy");
    std::fs::write(&agent, r##"#!/usr/bin/env node
const base = process.env.CODEBUDDY_BASE_URL;
if (!/^http:\/\/127\.0\.0\.1:\d+\/v1$/.test(base || '')) { console.error('unexpected WorkBuddy endpoint'); process.exit(7); }
const taped = Boolean(process.env.ASB_TAPE_MODE);
if (taped && (process.env.CODEBUDDY_API_KEY !== 'asb-replay' && process.env.CODEBUDDY_API_KEY !== 'fixture-key')) { console.error('incorrect replay key'); process.exit(8); }
if (taped) {
  if (!process.argv.includes('--strict-mcp-config') || process.argv[process.argv.indexOf('--tools') + 1] !== '') { console.error('strict tool configuration missing'); process.exit(9); }
  const config = JSON.parse(process.argv[process.argv.indexOf('--mcp-config') + 1]);
  const server = config.mcpServers && config.mcpServers['asb-local-tools'];
  if (!server || server.type !== 'stdio' || !server.command || !Array.isArray(server.args) || server.alwaysLoad !== true) { console.error('first-turn private MCP configuration missing'); process.exit(10); }
  const localTool = require('child_process').spawnSync(server.command, server.args, {
    input: JSON.stringify({jsonrpc:'2.0', id:1, method:'tools/call', params:{name:'write_file', arguments:{path:'src/parser.py', content:"def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n"}}}) + '\n',
    encoding: 'utf8',
  });
  if (localTool.status !== 0 || !localTool.stdout.includes('"content"')) { console.error('private MCP tool failed'); process.exit(11); }
} else {
  require('fs').writeFileSync('src/parser.py', "def parse_path(value: str) -> list[str]:\n    return [segment for segment in value.split('/') if segment]\n");
}
fetch(base + '/responses', {method:'POST', headers:{'content-type':'application/json'}, body:JSON.stringify({model:'test-model',input:'fixture',tools:[{type:'function'}]})})
  .then(async r => { if (!r.ok) throw new Error(await r.text()); console.log('{"type":"result","num_turns":1,"usage":{"input_tokens":1,"output_tokens":1}}'); })
  .catch(error => { console.error(error); process.exitCode = 1; });
"##)
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(root.join("benchmark.toml"), "agent='workbuddy'\nworkload='bug-fix'\nprovider='openrouter'\nmodel='test-model'\nconcurrency=1\n").unwrap();
    let record = run(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--record",
            "--cassette",
            "tape.jsonl",
        ],
    );
    assert!(
        record.status.success(),
        "record: {}",
        String::from_utf8_lossy(&record.stderr)
    );
    let mut archive = asb_cli::cassette::Archive::open(
        &root.join("tape.jsonl"),
        asb_cli::cassette::Limits::default(),
    )
    .unwrap();
    assert!(!archive.read_artifact(1, "local-tools").unwrap().is_empty());
    let before_replay = provider.requests.lock().unwrap().clone();
    let replay = run(
        &root,
        &provider.endpoint,
        &[
            "run",
            "benchmark.toml",
            "--replay",
            "--cassette",
            "tape.jsonl",
        ],
    );
    assert!(
        replay.status.success(),
        "replay: {}",
        String::from_utf8_lossy(&replay.stderr)
    );
    assert_eq!(*provider.requests.lock().unwrap(), before_replay);
    let live = run(&root, &provider.endpoint, &["run", "benchmark.toml"]);
    assert!(
        live.status.success(),
        "live: {}",
        String::from_utf8_lossy(&live.stderr)
    );
    assert!(provider.requests.lock().unwrap().len() > before_replay.len());
    std::fs::remove_dir_all(root).unwrap();
}
