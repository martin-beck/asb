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
