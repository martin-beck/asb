//! Rust orchestration fixtures use a separate, digest-checking SDK seam.
//! They do not qualify the upstream archive. The real helper's unchanged
//! production pin is independently checked below with a corrupt input.
use super::*;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::process::Command;
const ADAPTER: SweBenchAdapter = SweBenchAdapter {
    id: "fixture-suite",
    dataset: "fixture/source",
    revision: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    source_sha256: "fcd30fba8098151a003ce953022e7e6db16f53f82298290a285b22722483a5a1",
    expected_tasks: 0,
};
const PAYLOAD: &[u8] = b"small fixture source archive";

#[cfg(unix)]
#[test]
fn isolated_pinned_source_orchestration_preserves_and_validates_local_archive_bytes() {
    use std::os::unix::fs::PermissionsExt;
    const CASE: &str = "ASB_SWE_SOURCE_CASE";
    let Ok(case) = std::env::var(CASE) else {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/swe-source-fixtures")
            .join(TaskIdentity::local("fixture", None).attempt_id);
        fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let python = crate::tools::host_executable(Path::new("python3")).unwrap();
        let name = format!(
            "{}::isolated_pinned_source_orchestration_preserves_and_validates_local_archive_bytes",
            module_path!().split_once("::").unwrap().1
        );
        for case in [
            "download",
            "cached",
            "installed",
            "changed-cache",
            "changed-installed",
            "shared-cache-poisoned",
            "curl-fails",
            "curl-127",
            "missing-curl",
            "checksum-fails",
            "catalog-fails",
            "missing-python",
            "python-127",
            "cache-directory-error",
            "destination-directory",
        ] {
            let directory = root.join(case);
            fs::create_dir_all(directory.join("commands")).unwrap();
            let curl = directory.join("commands/curl");
            if case != "missing-curl" {
                crate::write_test_file(&directory, &curl,r#"#!/bin/sh
printf '%s\n' "$*" > "$ASB_SWE_SOURCE_ROOT/curl-arguments"
[ "$ASB_SWE_SOURCE_CASE" != curl-fails ] || exit 2
[ "$ASB_SWE_SOURCE_CASE" != curl-127 ] || exit 127
while [ "$#" -gt 0 ]; do
 if [ "$1" = --output ]; then
  shift
  if [ "$ASB_SWE_SOURCE_CASE" = checksum-fails ]; then printf 'corrupt source'; else printf 'small fixture source archive'; fi > "$1"
 fi
 shift
done
"#).unwrap();
                fs::set_permissions(curl, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .current_dir(&directory)
                .env(CASE, case)
                .env("ASB_SWE_SOURCE_ROOT", &directory)
                .env("ASB_SWE_SOURCE_PYTHON", &python)
                .env("HOME", &directory)
                .env("PATH", directory.join("commands"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        }
        fs::remove_dir_all(root).unwrap();
        return;
    };
    let root = PathBuf::from(std::env::var_os("ASB_SWE_SOURCE_ROOT").unwrap());
    let tools = root.join("tools");
    let workloads = root.join("workloads");
    let tool = tools.join("swebench");
    fs::create_dir_all(tool.join("venv/bin")).unwrap();
    fs::create_dir_all(workloads.join(ADAPTER.id)).unwrap();
    if case == "python-127" {
        let interpreter = tool.join("venv/bin/python");
        fs::write(&interpreter, "#!/bin/sh\nprintf 'started127\\n' >> \"$ASB_SWE_SOURCE_ROOT/verify-calls\"\nexit 127\n").unwrap();
        fs::set_permissions(interpreter, fs::Permissions::from_mode(0o700)).unwrap();
    } else if case != "missing-python" {
        std::os::unix::fs::symlink(
            std::env::var_os("ASB_SWE_SOURCE_PYTHON").unwrap(),
            tool.join("venv/bin/python"),
        )
        .unwrap();
    }
    fs::write(tool.join("asb_swebench_sources.py"),r#"import argparse,hashlib,os,sys
from pathlib import Path
parser=argparse.ArgumentParser()
parser.add_argument('--archive',required=True,type=Path)
parser.add_argument('--catalog',type=Path)
args=parser.parse_args()
with Path(os.environ['ASB_SWE_SOURCE_ROOT']).joinpath('verify-calls').open('a') as log:
 log.write(('catalog' if args.catalog else 'archive')+'\n')
if args.archive.is_symlink() or not args.archive.is_file(): sys.exit(2)
if hashlib.sha256(args.archive.read_bytes()).digest()!=hashlib.sha256(b'small fixture source archive').digest(): sys.exit(3)
if args.catalog and os.environ['ASB_SWE_SOURCE_CASE']=='catalog-fails': sys.exit(4)
"#).unwrap();
    let cache = root.join(".asb-cache/workload-sources");
    fs::create_dir_all(&cache).unwrap();
    let destination = tool.join(ARCHIVE);
    let archive = cache.join(ARCHIVE);
    match case.as_str() {
        "cached" | "catalog-fails" | "changed-installed" | "destination-directory" => {
            fs::write(&archive, PAYLOAD).unwrap()
        }
        "changed-cache" => fs::write(&archive, b"damaged cached evidence").unwrap(),
        "installed" => fs::write(&destination, PAYLOAD).unwrap(),
        "missing-python" | "python-127" => fs::write(&archive, PAYLOAD).unwrap(),
        "cache-directory-error" => {
            fs::remove_dir(&cache).unwrap();
            fs::write(&cache, b"blocked directory").unwrap();
        }
        "shared-cache-poisoned" => {
            let shared = root.join(".cache/asb/downloads");
            fs::create_dir_all(&shared).unwrap();
            let key = format!("{:x}", Sha256::digest(URL.as_bytes()));
            let payload = b"corrupt source";
            fs::write(shared.join(format!("{key}.blob")), payload).unwrap();
            fs::write(
                shared.join(format!("{key}.sha256")),
                format!("{:x}\n", Sha256::digest(payload)),
            )
            .unwrap();
        }
        _ => {}
    }
    if case == "changed-installed" {
        fs::write(&destination, b"damaged installed evidence").unwrap();
    }
    if case == "destination-directory" {
        fs::create_dir(&destination).unwrap();
    }
    let context = InstallationContext {
        process: &crate::install::OperationContext::new(
            &root,
            &crate::project::ProjectConfig::default(),
        )
        .unwrap(),
        project_root: &root,
        tools_root: &tools,
        workloads_root: &workloads,
    };
    let mut output = Vec::new();
    let mut stages = Vec::new();
    let result = install(&ADAPTER, &context, &mut output, &mut |percent, _| {
        stages.push(percent)
    });
    if matches!(
        case.as_str(),
        "download"
            | "cached"
            | "installed"
            | "changed-cache"
            | "changed-installed"
            | "shared-cache-poisoned"
    ) {
        result.unwrap();
        assert_eq!(fs::read(&destination).unwrap(), PAYLOAD);
        if case == "installed" {
            assert!(stages.is_empty());
            assert!(!archive.exists());
        } else {
            assert!(stages.contains(&90));
            assert_eq!(fs::read(&archive).unwrap(), PAYLOAD);
        }
        if case.starts_with("changed-") {
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains("Preserved the changed original task archive")
            );
            let parent = if case == "changed-cache" {
                &cache
            } else {
                &tool
            };
            let backup = fs::read_dir(parent)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| path.to_string_lossy().contains(".asb-backup-"))
                .unwrap();
            assert_eq!(
                fs::read(backup).unwrap(),
                if case == "changed-cache" {
                    b"damaged cached evidence".as_slice()
                } else {
                    b"damaged installed evidence".as_slice()
                }
            );
        }
        if matches!(
            case.as_str(),
            "download" | "changed-cache" | "shared-cache-poisoned"
        ) {
            let args = fs::read_to_string(root.join("curl-arguments")).unwrap();
            assert!(args.contains(URL));
            let args = args.split_whitespace().collect::<Vec<_>>();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--connect-timeout", "10"])
            );
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--max-filesize", "67108864"])
            );
            // ProcessScope owns the frozen remaining operation/download
            // deadline; no separate fixed90s curl timer may override it.
            assert!(!args.contains(&"--max-time"));
            assert!(args.contains(&"--fail"));
        }
        if case == "shared-cache-poisoned" {
            let key = format!("{:x}", Sha256::digest(URL.as_bytes()));
            assert_eq!(
                fs::read(
                    root.join(".cache/asb/downloads")
                        .join(format!("{key}.blob"))
                )
                .unwrap(),
                PAYLOAD
            );
        }
    } else {
        let error = result.unwrap_err().to_string();
        let expected = match case.as_str() {
            "curl-fails" | "curl-127" => "could not download original SWE-bench Dockerfiles",
            "missing-curl" => "original SWE-bench Dockerfiles needs `curl`",
            "checksum-fails" => "checksum mismatch",
            "python-127" => "did not match its pinned checksum",
            "catalog-fails" => "do not match the installed dataset",
            "missing-python" => "cannot validate original SWE-bench source",
            "cache-directory-error" => "",
            "destination-directory" => "must be a regular file",
            _ => unreachable!(),
        };
        assert!(error.contains(expected), "{error}");
        if matches!(
            case.as_str(),
            "curl-fails" | "curl-127" | "missing-curl" | "checksum-fails" | "python-127"
        ) {
            assert!(!archive.exists());
            assert!(!destination.exists());
        }
        if matches!(case.as_str(), "curl-fails" | "curl-127" | "missing-curl") {
            assert_eq!(stages, [80]);
            assert!(!root.join("verify-calls").exists());
            assert_eq!(root.join("curl-arguments").exists(), case != "missing-curl");
            if case == "curl-127" {
                assert!(error.contains("exit status: 127"), "{error}");
                assert!(!error.contains("needs `curl`"));
            }
        }
        if case == "missing-python" {
            assert!(!root.join("verify-calls").exists());
            assert!(!root.join("curl-arguments").exists());
            assert_eq!(fs::read(&archive).unwrap(), PAYLOAD);
            assert!(error.contains(&tool.join("venv/bin/python").display().to_string()));
        }
        if case == "python-127" {
            assert_eq!(
                fs::read_to_string(root.join("verify-calls")).unwrap(),
                "started127\nstarted127\n"
            );
            assert!(!error.contains("cannot validate original SWE-bench source"));
        }
    }
}

#[test]
fn real_source_helper_rejects_a_corrupt_archive_with_its_production_pin() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/swe-source-pin-fixtures")
        .join(TaskIdentity::local("fixture", None).attempt_id);
    fs::create_dir_all(&root).unwrap();
    let archive = root.join("corrupt.tar.gz");
    fs::write(&archive, b"not the official source archive").unwrap();
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/benchmarks");
    let code = r#"import sys
sys.path.insert(0,sys.argv[1])
from asb_swebench_sources import read_sources,SOURCE_SHA256
from asb_swebench import CatalogError
assert SOURCE_SHA256=='ff31aa1f8d2bb78f16559524264a99b868b7218ca82b390dc750bbc59538aa20'
try: read_sources(sys.argv[2])
except CatalogError as error:
 assert 'checksum does not match' in str(error)
else: raise AssertionError('corrupted source accepted')
"#;
    let output = Command::new("python3")
        .args(["-I", "-S", "-B", "-c", code])
        .arg(assets)
        .arg(&archive)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(archive).unwrap(),
        b"not the official source archive"
    );
    fs::remove_dir_all(root).unwrap();
}
