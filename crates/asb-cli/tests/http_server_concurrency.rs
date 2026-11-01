//! Concurrent report server lifecycle with an offline local HTTP fixture.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;
use std::sync::{Arc, Barrier};

#[test]
fn concurrent_server_starts_keep_latest_state_and_report() {
    let root = std::env::temp_dir().join(format!(
        "asb-http-concurrency-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join(".asb")).unwrap();
    fs::create_dir_all(root.join("results")).unwrap();
    fs::create_dir_all(root.join("tools/http/bin")).unwrap();
    fs::write(
        root.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    fs::write(root.join("results/report.html"), "fixture report").unwrap();
    let server = root.join("tools/http/bin/http-server");
    fs::write(&server, "#!/usr/bin/python3\nimport http.server,sys\na=sys.argv\np=int(a[a.index('-p')+1])\nh=http.server.SimpleHTTPRequestHandler\ns=http.server.ThreadingHTTPServer(('0.0.0.0',p),lambda *args,**kwargs:h(*args,directory=a[-1],**kwargs))\nprint('Starting up http-server, serving fixture',flush=True)\ns.serve_forever()\n").unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let outputs = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let root = &root;
                scope.spawn(move || {
                    barrier.wait();
                    Command::new(env!("CARGO_BIN_EXE_asb"))
                        .current_dir(root)
                        .env("ASB_HTTP_PUBLIC_HOST", "127.0.0.1")
                        .args(["tool", "http", "start"])
                        .output()
                        .unwrap()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".asb/http-server.json")).unwrap()).unwrap();
    let publication = std::path::PathBuf::from(state["publication"].as_str().unwrap());
    assert_eq!(
        fs::read_to_string(publication.join("report.html")).unwrap(),
        "fixture report"
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&root)
        .args(["tool", "http", "stop"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    for output in outputs {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn failed_start_does_not_leave_late_server_running() {
    let root = std::env::temp_dir().join(format!(
        "asb-http-startup-custody-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join(".asb")).unwrap();
    fs::create_dir_all(root.join("results")).unwrap();
    fs::create_dir_all(root.join("tools/http/bin")).unwrap();
    fs::write(
        root.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    fs::write(root.join("results/report.html"), "fixture").unwrap();
    let server = root.join("tools/http/bin/http-server");
    fs::write(&server, format!("#!/usr/bin/python3\nimport os,time\nopen({:?},'w').write(str(os.getpid())+' '+str(os.getppid()))\ntime.sleep(30)\n", root.join("pid").to_string_lossy())).unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
    let mut parent = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&root)
        .env("ASB_HTTP_PUBLIC_HOST", "127.0.0.1")
        .args(["tool", "http", "start"])
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !root.join("pid").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let identities = fs::read_to_string(root.join("pid")).unwrap();
    let mut ids = identities
        .split_whitespace()
        .map(|value| value.parse::<u32>().unwrap());
    let pid = ids.next().unwrap();
    let wrapper = ids.next().unwrap();
    parent.kill().unwrap();
    parent.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let proc_path = format!("/proc/{pid}");
    while std::path::Path::new(&proc_path).exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let alive = std::path::Path::new(&proc_path).exists();
    if alive {
        let _ = Command::new("kill")
            .args(["-KILL", &wrapper.to_string()])
            .status();
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    fs::remove_dir_all(root).unwrap();
    assert!(
        !alive,
        "failed startup left its owned server process running"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn killed_wrapper_does_not_leave_its_server_running() {
    let root = std::env::temp_dir().join(format!(
        "asb-http-wrapper-loss-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join(".asb")).unwrap();
    fs::create_dir_all(root.join("results")).unwrap();
    fs::create_dir_all(root.join("tools/http/bin")).unwrap();
    fs::write(
        root.join(".asb/project.toml"),
        "tools_dir='tools'\nworkloads_dir='workloads'\nworkspace_dir='workspace'\n",
    )
    .unwrap();
    fs::write(root.join("results/report.html"), "fixture").unwrap();
    let server = root.join("tools/http/bin/http-server");
    fs::write(&server, "#!/usr/bin/python3\nimport http.server,sys\na=sys.argv\np=int(a[a.index('-p')+1])\nh=http.server.SimpleHTTPRequestHandler\ns=http.server.ThreadingHTTPServer(('0.0.0.0',p),lambda *args,**kwargs:h(*args,directory=a[-1],**kwargs))\nprint('Starting up http-server, serving fixture',flush=True)\ns.serve_forever()\n").unwrap();
    fs::set_permissions(&server, fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&root)
        .env("ASB_HTTP_PUBLIC_HOST", "127.0.0.1")
        .args(["tool", "http", "start"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join(".asb/http-server.json")).unwrap()).unwrap();
    let pid = state["pid"].as_u64().unwrap() as u32;
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let wrapper: u32 = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let executable = fs::read_link(format!("/proc/{wrapper}/exe")).unwrap();
    assert_eq!(
        executable,
        fs::canonicalize(env!("CARGO_BIN_EXE_asb")).unwrap()
    );
    assert!(
        Command::new("kill")
            .args(["-KILL", &wrapper.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let running = || {
        fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|text| {
            !text.lines().any(|line| {
                line.starts_with("State:") && line.split_whitespace().nth(1) == Some("Z")
            })
        })
    };
    while running() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let leaked = running();
    if leaked {
        let _ = Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        !leaked,
        "wrapper termination left its owned HTTP server alive"
    );
    let publication = std::path::PathBuf::from(state["publication"].as_str().unwrap());
    let restarted = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&root)
        .env("ASB_HTTP_PUBLIC_HOST", "127.0.0.1")
        .args(["tool", "http", "start"])
        .output()
        .unwrap();
    assert!(
        restarted.status.success(),
        "{}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    assert_eq!(
        fs::read_to_string(publication.join("report.html")).unwrap(),
        "fixture"
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_asb"))
        .current_dir(&root)
        .args(["tool", "http", "stop"])
        .output()
        .unwrap();
    assert!(
        stopped.status.success(),
        "{}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    fs::remove_dir_all(root).unwrap();
}
