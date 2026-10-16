//! Project-local installers for agents and workload prompt files.

use crate::Error;
use crate::project;
use crate::suggestions;
use crate::workloads;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

const LLMTAPE_WRAPPER: &str = include_str!("../../../scripts/llmtape.mjs");
const BPFTRACE_SOURCE: &str =
    "https://github.com/bpftrace/bpftrace/archive/refs/tags/v0.20.2.tar.gz";

trait LoggedStatus {
    fn logged_status(&mut self) -> io::Result<ExitStatus>;
}

impl LoggedStatus for Command {
    fn logged_status(&mut self) -> io::Result<ExitStatus> {
        let log_path = Path::new(".asb/install.log");
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        let stderr = stdout.try_clone()?;
        self.stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .status()
    }
}

#[derive(Clone, Copy)]
enum Recipe {
    Npm {
        package: &'static str,
        executable: &'static str,
    },
    Python {
        package: &'static str,
        executable: &'static str,
    },
    Host {
        executable: &'static str,
    },
    Bpftrace,
    Flamegraph,
}

struct ExternalWorkload {
    name: &'static str,
    source: &'static str,
}

struct BundleSpec {
    archive: &'static str,
    marker: &'static str,
    prompt: &'static str,
}

const BUILTIN_TOOLS: &[&str] = &[
    "codex",
    "pi",
    "opencode",
    "workbuddy",
    "gemini",
    "qwen-code",
    "aider",
    "mini-swe",
    "openhands",
    "openjiuwen",
    "cli2key",
    "llmtape",
    "mpstat",
    "iostat",
    "perf",
    "bpftrace",
    "flamegraph",
];

const EXTERNAL_WORKLOADS: &[ExternalWorkload] = &[
    ExternalWorkload {
        name: "agentbench",
        source: "https://raw.githubusercontent.com/THUDM/AgentBench/d1e4a10db08c87075c78972e48ecc182be03e2d5/README.md",
    },
    ExternalWorkload {
        name: "agentdojo",
        source: "https://raw.githubusercontent.com/ethz-spylab/agentdojo/089ed468cf3ed0322acc66b0211f26d9d90dbf60/README.md",
    },
    ExternalWorkload {
        name: "aider-polyglot",
        source: "https://raw.githubusercontent.com/Aider-AI/aider/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/README.md",
    },
    ExternalWorkload {
        name: "bigcodebench",
        source: "https://raw.githubusercontent.com/bigcode-project/bigcodebench/9059fb84d1188c02edeac4995361656a2fdecbef/README.md",
    },
    ExternalWorkload {
        name: "core-bench",
        source: "https://raw.githubusercontent.com/siegelz/core-bench/e32a2980e72fe6eb04ee04eb749458f570625663/README.md",
    },
    ExternalWorkload {
        name: "evalplus",
        source: "https://raw.githubusercontent.com/evalplus/evalplus/e5d0ed0bab96280b60b637ec7f15b5e4841b0cb2/README.md",
    },
    ExternalWorkload {
        name: "livecodebench",
        source: "https://raw.githubusercontent.com/LiveCodeBench/LiveCodeBench/28fef95ea8c9f7a547c8329f2cd3d32b92c1fa24/README.md",
    },
    ExternalWorkload {
        name: "swe-bench",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-lite",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-pro",
        source: "https://raw.githubusercontent.com/scaleapi/SWE-bench_Pro-os/ca10a60a5fcae51e6948ffe1485d4153d421e6c5/README.md",
    },
    ExternalWorkload {
        name: "swe-bench-verified",
        source: "https://raw.githubusercontent.com/SWE-bench/SWE-bench/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e/README.md",
    },
    ExternalWorkload {
        name: "swe-efficiency",
        source: "https://raw.githubusercontent.com/swefficiency/swefficiency/12d32a2d6800824a7d84bdb6797b5708e7b7957f/README.md",
    },
    ExternalWorkload {
        name: "swe-lancer",
        source: "https://raw.githubusercontent.com/openai/frontier-evals/51052cede8cc608f95bb00346635e03759013e5a/README.md",
    },
    ExternalWorkload {
        name: "swe-perf",
        source: "https://raw.githubusercontent.com/SWE-Perf/SWE-Perf/9e8fed285ed640b51baaf66b13773706dc95aa45/README.md",
    },
    ExternalWorkload {
        name: "swe-rebench",
        source: "https://raw.githubusercontent.com/SWE-rebench/SWE-bench-fork/e4907b7a90eafaa1f0a6428fd04fe31cdd8b4284/README.md",
    },
    ExternalWorkload {
        name: "tau-bench",
        source: "https://raw.githubusercontent.com/sierra-research/tau-bench/59a200c6d575d595120f1cb70fea53cef0632f6b/README.md",
    },
    ExternalWorkload {
        name: "terminal-bench",
        source: "https://raw.githubusercontent.com/harbor-framework/terminal-bench/452bf305c6daa62fc59061d22133a7cbc7c1572e/README.md",
    },
];

fn external_workload(name: &str) -> Option<&'static ExternalWorkload> {
    EXTERNAL_WORKLOADS
        .iter()
        .find(|workload| workload.name == name)
}

/// Return the pinned external workloads that the TUIs can offer for install.
pub fn external_workload_names() -> impl Iterator<Item = &'static str> {
    EXTERNAL_WORKLOADS.iter().map(|workload| workload.name)
}

/// Check whether an external workload has its runnable local bundle and
/// prompt. This is side-effect free and safe to call while rendering a TUI.
pub fn workload_available(name: &str, directory: &Path) -> bool {
    if bundle_spec(name).is_some() {
        directory.join(name).join("ASB-BUNDLE").is_file()
            && directory.join(name).join("source").is_dir()
            && directory.join(format!("{name}.txt")).is_file()
    } else {
        directory.join(format!("{name}.txt")).is_file()
    }
}

fn bundle_spec(name: &str) -> Option<BundleSpec> {
    match name {
        "swe-bench" => Some(BundleSpec {
            archive: "https://github.com/SWE-bench/SWE-bench/archive/02e7a74ffd0b707aab73d203fe87bdc7c76afc8e.tar.gz",
            marker: "swe-bench/SWE-bench@02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
            prompt: "Use the upstream SWE-bench source in `external-workload/source`. Inspect its README and package metadata, select a concrete SWE-bench task or dataset-backed command available in this checkout, make the requested change, and run the narrowest applicable tests. Keep all changes inside this local checkout and do not download additional data during the run.\n",
        }),
        "terminal-bench" => Some(BundleSpec {
            archive: "https://github.com/harbor-framework/terminal-bench/archive/452bf305c6daa62fc59061d22133a7cbc7c1572e.tar.gz",
            marker: "terminal-bench/terminal-bench@452bf305c6daa62fc59061d22133a7cbc7c1572e",
            prompt: "Use the upstream Terminal-Bench source in `external-workload/source`. Inspect its README, task definitions, and local runner instructions, then execute or improve a concrete terminal task using only the installed checkout and run its focused tests. Do not download additional data during the run.\n",
        }),
        _ => None,
    }
}

fn recipe(name: &str) -> Option<Recipe> {
    match name {
        "codex" => Some(Recipe::Npm {
            package: "@openai/codex@0.162.1",
            executable: "codex",
        }),
        "pi" => Some(Recipe::Npm {
            package: "@mariozechner/pi-coding-agent@0.73.1",
            executable: "pi",
        }),
        "opencode" => Some(Recipe::Npm {
            package: "opencode-ai@1.19.0",
            executable: "opencode",
        }),
        "workbuddy" => Some(Recipe::Npm {
            package: "@tencent-ai/codebuddy-code@2.167.0",
            executable: "codebuddy",
        }),
        "gemini" => Some(Recipe::Npm {
            package: "@google/gemini-cli@0.64.0",
            executable: "gemini",
        }),
        "qwen-code" => Some(Recipe::Npm {
            package: "@qwen-code/qwen-code@0.25.1",
            executable: "qwen",
        }),
        "aider" => Some(Recipe::Python {
            package: "aider-chat==0.86.2",
            executable: "aider",
        }),
        "mini-swe" => Some(Recipe::Python {
            package: "mini-swe-agent==2.4.6",
            executable: "mini",
        }),
        "openhands" => Some(Recipe::Python {
            package: "openhands==1.16.0",
            executable: "openhands",
        }),
        "openjiuwen" => Some(Recipe::Python {
            package: "openjiuwen[cli]==0.1.19.post2",
            executable: "openjiuwen",
        }),
        "cli2key" => Some(Recipe::Npm {
            package: "codex-anthropic-bridge@0.1.10",
            executable: "codex-bridge",
        }),
        "llmtape" => Some(Recipe::Npm {
            package: "llmtape@1.2.0",
            executable: "llmtape",
        }),
        "mpstat" => Some(Recipe::Host {
            executable: "mpstat",
        }),
        "iostat" => Some(Recipe::Host {
            executable: "iostat",
        }),
        "perf" => Some(Recipe::Host { executable: "perf" }),
        "bpftrace" => Some(Recipe::Bpftrace),
        "flamegraph" => Some(Recipe::Flamegraph),
        _ => None,
    }
}

pub fn tool(args: &[String], output: &mut dyn std::io::Write) -> Result<(), Error> {
    let mut progress = |_percent: u8, _stage: &str| {};
    tool_with_progress(args, output, &mut progress)
}

/// Install a project-local tool and report concrete installer stages.
pub fn tool_with_progress(
    args: &[String],
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let (id, source, root, dry_run) = parse_tool(args)?;
    progress(2, "preparing");
    let root = root.join(&id);
    if let Some(source) = source {
        let destination = root.join("bin").join(executable_name(&id));
        if dry_run {
            writeln!(output, "download {source} -> {}", destination.display())
                .map_err(super::io_error)?;
            return Ok(());
        }
        progress(20, "copying source");
        fs::create_dir_all(destination.parent().expect("bin parent"))
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
        install_source(&source, &destination, "tool")?;
        make_executable(&destination)?;
        progress(100, "ready");
        writeln!(output, "Installed tool {id} at {}", destination.display())
            .map_err(super::io_error)?;
        return Ok(());
    }
    let plan = match recipe(&id) {
        Some(plan) => plan,
        None => {
            return Err(Error::Input(format!(
                "{}; pass --source URL or PATH",
                suggestions::unknown("tool", &id, BUILTIN_TOOLS)
            )));
        }
    };
    if dry_run {
        writeln!(output, "{}", describe(plan, &root)).map_err(super::io_error)?;
        return Ok(());
    }
    let executable = install_recipe(plan, &root, progress)?;
    if id == "cli2key" {
        progress(90, "generating local API key");
        let status = Command::new(&executable)
            .args(["key", "refresh"])
            .logged_status()
            .map_err(|error| Error::Io(format!("cannot refresh cli2key API key: {error}")))?;
        if !status.success() {
            return Err(Error::Io(
                "cli2key was installed, but its local API key could not be generated".into(),
            ));
        }
        writeln!(output, "Generated a local cli2key API key for project runs")
            .map_err(super::io_error)?;
    }
    progress(100, "ready");
    let kind = if id == "llmtape" { "tool" } else { "agent" };
    writeln!(output, "Installed {kind} {id} at {}", executable.display())
        .map_err(super::io_error)?;
    Ok(())
}

/// List every built-in tool recipe and its project-local executable.
pub fn tool_list(output: &mut dyn std::io::Write) -> Result<(), Error> {
    writeln!(output, "Available tools:").map_err(super::io_error)?;
    for &name in BUILTIN_TOOLS {
        let plan = recipe(name).expect("listed tool has a recipe");
        writeln!(output, "  {name:<12} {}", tool_summary(plan)).map_err(super::io_error)?;
    }
    writeln!(output, "  custom       --source URL|PATH").map_err(super::io_error)
}

/// Report whether a built-in tool is runnable from the host or this project.
/// This is intentionally side-effect free so TUIs can refresh status on every
/// navigation or after a Ctrl-I installation attempt.
pub fn tool_available(name: &str, tools_root: &Path) -> bool {
    let Some(plan) = recipe(name) else {
        return false;
    };
    let local = executable(plan, &tools_root.join(name));
    if local.is_file() {
        return true;
    }
    matches!(plan, Recipe::Host { executable } if host_executable(executable).is_ok())
}

pub fn workload(args: &[String], output: &mut dyn std::io::Write) -> Result<(), Error> {
    let mut progress = |_percent: u8, _stage: &str| {};
    workload_with_progress(args, output, &mut progress)
}

/// Install a workload and report concrete download/extraction stages.
pub fn workload_with_progress(
    args: &[String],
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let (source, name, directory) = parse_workload(args)?;
    progress(2, "preparing");
    fs::create_dir_all(&directory)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", directory.display())))?;
    if let Some(bundle) = bundle_spec(&name) {
        install_bundle(&name, bundle, &directory, output, progress)?;
        return Ok(());
    }
    progress(30, "downloading prompt");
    let destination = directory.join(format!("{name}.txt"));
    install_source(&source, &destination, "workload")?;
    progress(100, "ready");
    writeln!(
        output,
        "Installed workload {name} at {}",
        destination.display()
    )
    .map_err(super::io_error)?;
    Ok(())
}

fn install_bundle(
    name: &str,
    bundle: BundleSpec,
    directory: &Path,
    output: &mut dyn std::io::Write,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<(), Error> {
    let bundle_dir = directory.join(name);
    let source_dir = bundle_dir.join("source");
    let marker = bundle_dir.join("ASB-BUNDLE");
    if marker.is_file() && source_dir.is_dir() {
        progress(80, "validating existing bundle");
        let installed = fs::read_to_string(&marker).map_err(|error| {
            Error::Io(format!(
                "cannot read external workload marker {}: {error}",
                marker.display()
            ))
        })?;
        if installed.trim() != bundle.marker {
            return Err(Error::Io(format!(
                "external workload {name} is installed at a different pinned revision; remove {} and retry",
                bundle_dir.display()
            )));
        }
        let prompt = directory.join(format!("{name}.txt"));
        if !prompt.exists() {
            fs::write(&prompt, bundle.prompt).map_err(|error| {
                Error::Io(format!(
                    "cannot write external workload prompt {}: {error}",
                    prompt.display()
                ))
            })?;
        }
        workloads::write_default_trial_set(name, directory)
            .map_err(|error| Error::Io(format!("cannot write {name} trial set: {error}")))?;
        writeln!(
            output,
            "External workload {name} already installed at {}",
            bundle_dir.display()
        )
        .map_err(super::io_error)?;
        progress(100, "ready");
        return Ok(());
    }
    if bundle_dir.exists() {
        return Err(Error::Io(format!(
            "external workload {name} has an incomplete local bundle at {}; remove that directory and retry",
            bundle_dir.display()
        )));
    }
    let archive = directory.join(format!(".{name}.tar.gz"));
    let extract = directory.join(format!(".{name}.extract"));
    if extract.exists() {
        fs::remove_dir_all(&extract).map_err(|error| {
            Error::Io(format!(
                "cannot clear stale extraction {}: {error}",
                extract.display()
            ))
        })?;
    }
    fs::create_dir_all(&extract).map_err(|error| {
        Error::Io(format!(
            "cannot create extraction directory {}: {error}",
            extract.display()
        ))
    })?;
    progress(20, "downloading bundle");
    install_source(bundle.archive, &archive, "external workload bundle")?;
    progress(45, "extracting bundle");
    let status = Command::new("tar")
        .args(["--extract", "--gzip", "--file"])
        .arg(&archive)
        .args(["--directory"])
        .arg(&extract)
        .logged_status()
        .map_err(|error| missing_dependency("tar", "external workload installation", error))?;
    if !status.success() {
        return Err(Error::Io(format!(
            "could not extract external workload {name}"
        )));
    }
    let top = fs::read_dir(&extract)
        .map_err(|error| Error::Io(format!("cannot inspect extracted {name}: {error}")))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.is_dir())
        .ok_or_else(|| {
            Error::Io(format!(
                "external workload {name} archive has no source directory"
            ))
        })?;
    fs::create_dir_all(&bundle_dir).map_err(|error| {
        Error::Io(format!(
            "cannot create external workload {}: {error}",
            bundle_dir.display()
        ))
    })?;
    fs::rename(&top, &source_dir).map_err(|error| {
        Error::Io(format!(
            "cannot place external workload source {}: {error}",
            source_dir.display()
        ))
    })?;
    fs::write(&marker, bundle.marker).map_err(|error| {
        Error::Io(format!(
            "cannot write external workload marker {}: {error}",
            marker.display()
        ))
    })?;
    let prompt = directory.join(format!("{name}.txt"));
    if !prompt.exists() {
        fs::write(&prompt, bundle.prompt).map_err(|error| {
            Error::Io(format!(
                "cannot write external workload prompt {}: {error}",
                prompt.display()
            ))
        })?;
    }
    workloads::write_default_trial_set(name, directory)
        .map_err(|error| Error::Io(format!("cannot write {name} trial set: {error}")))?;
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&extract);
    writeln!(
        output,
        "Installed external workload {name} at {} with local source and prompt",
        bundle_dir.display()
    )
    .map_err(super::io_error)?;
    progress(100, "ready");
    Ok(())
}

/// List prompt workloads and configurable external workload trial sets.
pub fn workload_list(args: &[String], output: &mut dyn std::io::Write) -> Result<(), Error> {
    let (project_root, project_config) = project::load(Path::new("."))?;
    let mut directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
    let mut index = 0;
    while index < args.len() {
        if args[index] != "--dir" {
            return Err(Error::Usage("workload list accepts --dir DIR"));
        }
        directory = project::resolve(
            &project_root,
            args.get(index + 1)
                .ok_or(Error::Usage("--dir requires a directory"))?,
        );
        index += 2;
    }
    let mut names = fs::read_dir(&directory)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", directory.display())))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("txt"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    names.sort();
    let mut trial_sets = fs::read_dir(&directory)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", directory.display())))?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            name.strip_suffix(".workload.toml").map(str::to_owned)
        })
        .collect::<Vec<_>>();
    trial_sets.sort();
    writeln!(output, "Available workloads in {}:", directory.display()).map_err(super::io_error)?;
    if names.is_empty() {
        writeln!(output, "  (none)").map_err(super::io_error)?;
    } else {
        for name in names {
            writeln!(output, "  {name}").map_err(super::io_error)?;
        }
    }
    writeln!(output, "Configured workload trial sets:").map_err(super::io_error)?;
    if trial_sets.is_empty() {
        writeln!(output, "  (none)").map_err(super::io_error)?;
    } else {
        for name in trial_sets {
            writeln!(output, "  {name} [trial set]").map_err(super::io_error)?;
        }
    }
    writeln!(output, "Installable external workloads:").map_err(super::io_error)?;
    for workload in EXTERNAL_WORKLOADS {
        writeln!(output, "  {}", workload.name).map_err(super::io_error)?;
    }
    Ok(())
}

fn install_recipe(
    recipe: Recipe,
    root: &Path,
    progress: &mut dyn FnMut(u8, &str),
) -> Result<PathBuf, Error> {
    fs::create_dir_all(root)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
    let status = match recipe {
        Recipe::Npm { package, .. } => {
            progress(15, "installing npm package");
            Command::new("npm")
            .args(["install", "--prefix"])
            .arg(root)
            .args(["--no-save", package])
            .logged_status()
        }
        Recipe::Python { package, .. } => {
            let venv = root.join("venv");
            let created = match Command::new("python3")
                .args(["-m", "venv"])
                .arg(&venv)
                .logged_status()
            {
                Ok(status) => status,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Command::new("uv")
                    .args(["venv", "--seed", "--python", "3.12"])
                    .arg(&venv)
                    .logged_status()
                    .map_err(|uv_error| {
                        missing_dependency("python3", "Python agent installation", uv_error)
                    })?,
                Err(error) => {
                    return Err(missing_dependency(
                        "python3",
                        "Python agent installation",
                        error,
                    ));
                }
            };
            if !created.success() {
                return Err(Error::Io(
                    "python3 could not create the agent environment".into(),
                ));
            }
            progress(35, "installing Python package");
            Command::new(venv.join("bin/python"))
                .args(["-m", "pip", "install", package])
                .logged_status()
        }
        Recipe::Host { executable } => {
            progress(55, "copying host tool");
            let source = host_executable(executable)?;
            let destination = root.join("bin").join(executable);
            fs::create_dir_all(destination.parent().expect("host tool parent"))
                .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
            fs::copy(&source, &destination).map_err(|error| {
                Error::Io(format!(
                    "cannot copy host tool {} to {}: {error}",
                    source.display(),
                    destination.display()
                ))
            })?;
            make_executable(&destination)?;
            return Ok(destination);
        }
        Recipe::Bpftrace => return install_bpftrace(root, progress),
        Recipe::Flamegraph => {
            progress(25, "downloading FlameGraph scripts");
            let bin = root.join("bin");
            fs::create_dir_all(&bin)
                .map_err(|error| Error::Io(format!("cannot create {}: {error}", bin.display())))?;
            for (name, url) in [
                (
                    "stackcollapse-perf.pl",
                    "https://raw.githubusercontent.com/brendangregg/FlameGraph/41fee1f99f9276008b7cd112fca19dc3ea84ac32/stackcollapse-perf.pl",
                ),
                (
                    "flamegraph",
                    "https://raw.githubusercontent.com/brendangregg/FlameGraph/41fee1f99f9276008b7cd112fca19dc3ea84ac32/flamegraph.pl",
                ),
            ] {
                let destination = bin.join(name);
                install_source(url, &destination, "flamegraph tool")?;
                make_executable(&destination)?;
                progress(if name == "flamegraph" { 90 } else { 60 }, "installing FlameGraph scripts");
            }
            return Ok(bin.join("flamegraph"));
        }
    }
    .map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            let dependency = match recipe {
                Recipe::Npm { .. } => "npm",
                Recipe::Python { .. } => "python3",
                Recipe::Host { .. } => "host measurement binary",
                Recipe::Bpftrace => "bpftrace and uname",
                Recipe::Flamegraph => "curl",
            };
            missing_dependency(dependency, "agent installation", error)
        } else {
            Error::Io(format!("cannot start agent installer: {error}"))
        }
    })?;
    if !status.success() {
        return Err(Error::Io(
            "agent package installation failed; verify the prerequisite tool and rerun `asb tool install`".into(),
        ));
    }
    let executable = executable(recipe, root);
    if is_llmtape(recipe) {
        fs::create_dir_all(executable.parent().expect("llmtape wrapper parent")).map_err(
            |error| Error::Io(format!("cannot create llmtape wrapper directory: {error}")),
        )?;
        fs::write(&executable, LLMTAPE_WRAPPER).map_err(|error| {
            Error::Io(format!("cannot write {}: {error}", executable.display()))
        })?;
        make_executable(&executable)?;
    }
    Ok(executable)
}

/// Copy the host bpftrace binary and persist the kernel pairing used to
/// install it.  bpftrace is kernel-sensitive; using the exact host binary and
/// recording `uname -r` prevents silently reusing a project tool from another
/// machine or kernel image.
fn install_bpftrace(root: &Path, progress: &mut dyn FnMut(u8, &str)) -> Result<PathBuf, Error> {
    progress(10, "checking host bpftrace");
    let source = match host_executable("bpftrace") {
        Ok(source) => source,
        Err(host_error) => match build_bpftrace(root, progress) {
            Ok(source) => source,
            Err(build_error) => {
                return Err(Error::Io(format!(
                    "could not install bpftrace: {host_error}\n\n{build_error}\n\nTo install a matching host tool instead, run one of these commands:\n{}",
                    bpftrace_host_commands()
                )));
            }
        },
    };
    let version = command_output(&source, &["--version"], "bpftrace version")?;
    progress(95, "recording kernel compatibility");
    let kernel = command_output(Path::new("uname"), &["-r"], "kernel release")?;
    let machine = command_output(Path::new("uname"), &["-m"], "kernel architecture")?;
    let bin = root.join("bin");
    fs::create_dir_all(&bin)
        .map_err(|error| Error::Io(format!("cannot create {}: {error}", bin.display())))?;
    let destination = bin.join("bpftrace");
    fs::copy(&source, &destination).map_err(|error| {
        Error::Io(format!(
            "cannot copy host bpftrace {} to {}: {error}",
            source.display(),
            destination.display()
        ))
    })?;
    make_executable(&destination)?;
    let metadata = format!(
        "{{\n  \"schema_version\": 1,\n  \"tool\": \"bpftrace\",\n  \"version\": {},\n  \"kernel_release\": {},\n  \"kernel_architecture\": {}\n}}\n",
        json_string(&version),
        json_string(&kernel),
        json_string(&machine)
    );
    fs::write(root.join("bpftrace-compatibility.json"), metadata).map_err(|error| {
        Error::Io(format!(
            "cannot write bpftrace compatibility metadata: {error}"
        ))
    })?;
    progress(100, "ready");
    Ok(destination)
}

fn build_bpftrace(root: &Path, progress: &mut dyn FnMut(u8, &str)) -> Result<PathBuf, Error> {
    let source_dir = root.join("source");
    let build_dir = root.join("build");
    let archive = root.join(".bpftrace-source.tar.gz");
    if !source_dir.is_dir() {
        fs::create_dir_all(root)
            .map_err(|error| Error::Io(format!("cannot create {}: {error}", root.display())))?;
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(&archive)
            .arg(BPFTRACE_SOURCE)
            .logged_status()
            .map_err(|error| bpftrace_missing_dependency("curl", error))?;
        if !status.success() {
            return Err(Error::Io(format!(
                "could not download pinned bpftrace source from {BPFTRACE_SOURCE}; verify network access and retry\n\n{}",
                bpftrace_source_commands()
            )));
        }
        progress(25, "downloading bpftrace source");
        fs::create_dir_all(&source_dir).map_err(|error| {
            Error::Io(format!("cannot create bpftrace source directory: {error}"))
        })?;
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&source_dir)
            .args(["--strip-components=1"])
            .logged_status()
            .map_err(|error| bpftrace_missing_dependency("tar", error))?;
        if !status.success() {
            return Err(Error::Io(format!(
                "could not extract the pinned bpftrace source; install tar and retry\n\n{}",
                bpftrace_dependency_commands("tar")
            )));
        }
        progress(40, "extracting bpftrace source");
    }
    let status = Command::new("cmake")
        .args(["-S"])
        .arg(&source_dir)
        .args(["-B"])
        .arg(&build_dir)
        .args(["-DBUILD_TESTING=OFF", "-DBPFTRACE_BUILD_TESTING=OFF"])
        .logged_status()
        .map_err(|error| bpftrace_missing_dependency("cmake", error))?;
    if !status.success() {
        return Err(Error::Io(format!(
            "cmake could not configure bpftrace; the BCC, libbpf, libclang, compiler, or CMake development packages are missing. Install them and rerun `asb tool install bpftrace`:\n\n{}",
            bpftrace_source_commands()
        )));
    }
    progress(60, "configuring bpftrace build");
    let status = Command::new("cmake")
        .args(["--build"])
        .arg(&build_dir)
        .args(["--target", "bpftrace", "--parallel"])
        .logged_status()
        .map_err(|error| bpftrace_missing_dependency("cmake", error))?;
    if !status.success() {
        return Err(Error::Io(format!(
            "bpftrace source compilation failed; install the compiler and BCC/libbpf/libclang development packages, then rerun `asb tool install bpftrace`:\n\n{}",
            bpftrace_source_commands()
        )));
    }
    progress(90, "building bpftrace");
    find_built_bpftrace(&build_dir).ok_or_else(|| {
        Error::Io(format!(
            "bpftrace built without a discoverable executable under {}",
            build_dir.display()
        ))
    })
}

fn find_built_bpftrace(directory: &Path) -> Option<PathBuf> {
    let mut entries = fs::read_dir(directory).ok()?.filter_map(Result::ok);
    while let Some(entry) = entries.next() {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some("bpftrace") && path.is_file() {
            return Some(path);
        }
        if path.is_dir()
            && path.file_name().and_then(|name| name.to_str()) != Some("CMakeFiles")
            && let Some(found) = find_built_bpftrace(&path)
        {
            return Some(found);
        }
    }
    None
}

fn bpftrace_host_commands() -> &'static str {
    "Red Hat/Fedora:\n  sudo dnf install -y bpftrace\nDebian/Ubuntu:\n  sudo apt-get update && sudo apt-get install -y bpftrace\nopenEuler:\n  sudo dnf install -y bpftrace"
}

fn bpftrace_source_commands() -> &'static str {
    "Red Hat/Fedora:\n  sudo dnf install -y bcc-devel libbpf-devel libclang-devel clang llvm cmake make gcc-c++ curl tar\nDebian/Ubuntu:\n  sudo apt-get update && sudo apt-get install -y libbpfcc-dev libbpf-dev libclang-dev clang llvm cmake build-essential curl tar\nopenEuler:\n  sudo dnf install -y bcc-devel libbpf-devel clang llvm cmake make gcc-c++ curl tar"
}

fn bpftrace_dependency_commands(dependency: &str) -> &'static str {
    match dependency {
        "curl" => {
            "Red Hat/Fedora:\n  sudo dnf install -y curl\nDebian/Ubuntu:\n  sudo apt-get update && sudo apt-get install -y curl\nopenEuler:\n  sudo dnf install -y curl"
        }
        "tar" => {
            "Red Hat/Fedora:\n  sudo dnf install -y tar\nDebian/Ubuntu:\n  sudo apt-get update && sudo apt-get install -y tar\nopenEuler:\n  sudo dnf install -y tar"
        }
        "cmake" => {
            "Red Hat/Fedora:\n  sudo dnf install -y cmake make gcc-c++\nDebian/Ubuntu:\n  sudo apt-get update && sudo apt-get install -y cmake build-essential\nopenEuler:\n  sudo dnf install -y cmake make gcc-c++"
        }
        _ => bpftrace_source_commands(),
    }
}

fn bpftrace_missing_dependency(dependency: &str, error: std::io::Error) -> Error {
    Error::Io(format!(
        "bpftrace source build needs `{dependency}`, but it could not be started: {error}. Install it, then rerun `asb tool install bpftrace`:\n\n{}",
        bpftrace_dependency_commands(dependency)
    ))
}

fn command_output(program: &Path, args: &[&str], label: &str) -> Result<String, Error> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| Error::Io(format!("cannot inspect {label}: {error}")))?;
    if !output.status.success() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command exited with {}",
            output.status
        )));
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if text.is_empty() {
        return Err(Error::Io(format!(
            "cannot inspect {label}: command returned no output"
        )));
    }
    Ok(text)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"unavailable\"".into())
}

fn executable(recipe: Recipe, root: &Path) -> PathBuf {
    match recipe {
        Recipe::Npm {
            package,
            executable,
        } if package == "llmtape@1.2.0" => root.join("bin").join(executable),
        Recipe::Npm { executable, .. } => root.join("node_modules/.bin").join(executable),
        Recipe::Python { executable, .. } => root.join("venv/bin").join(executable),
        Recipe::Host { executable } => root.join("bin").join(executable),
        Recipe::Bpftrace => root.join("bin").join("bpftrace"),
        Recipe::Flamegraph => root.join("bin").join("flamegraph"),
    }
}

fn is_llmtape(recipe: Recipe) -> bool {
    matches!(
        recipe,
        Recipe::Npm {
            package: "llmtape@1.2.0",
            ..
        }
    )
}

fn describe(recipe: Recipe, root: &Path) -> String {
    match recipe {
        Recipe::Npm { package, .. } => format!("npm install --prefix {} {package}", root.display()),
        Recipe::Python { package, .. } => format!(
            "python3 -m venv {}/venv && pip install {package}",
            root.display()
        ),
        Recipe::Host { executable } => {
            format!("copy host `{executable}` into {}/bin", root.display())
        }
        Recipe::Bpftrace => format!(
            "copy the host-compatible bpftrace into {}/bin and record uname metadata",
            root.display()
        ),
        Recipe::Flamegraph => format!(
            "download pinned FlameGraph scripts into {}/bin",
            root.display()
        ),
    }
}

fn tool_summary(recipe: Recipe) -> String {
    match recipe {
        Recipe::Npm {
            package,
            executable,
            ..
        } => format!("npm {package} -> {executable}"),
        Recipe::Python {
            package,
            executable,
            ..
        } => format!("python {package} -> {executable}"),
        Recipe::Host { executable } => format!("host binary -> {executable}"),
        Recipe::Bpftrace => "host-compatible bpftrace + kernel metadata".into(),
        Recipe::Flamegraph => "pinned FlameGraph scripts -> flamegraph".into(),
    }
}

fn host_executable(name: &str) -> Result<PathBuf, Error> {
    let path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .chain([PathBuf::from("/usr/bin").join(name), PathBuf::from("/bin").join(name)])
        .find(|path| path.is_file())
        .ok_or_else(|| {
            Error::Io(format!(
                "measurement tool `{name}` is not installed on this host; install its distribution package and rerun `asb tool install {name}`"
            ))
        })?;
    Ok(path)
}

fn parse_tool(args: &[String]) -> Result<(String, Option<String>, PathBuf, bool), Error> {
    let id = args
        .first()
        .filter(|value| valid_name(value))
        .cloned()
        .ok_or(Error::Usage("tool install requires a tool name"))?;
    let mut source = None;
    let (project_root, project_config) = project::load(Path::new("."))?;
    let mut root = project::resolve(&project_root, &project_config.tools_dir);
    let mut dry_run = false;
    parse_options(&args[1..], |flag, value| match flag {
        "--source" => Ok(source = Some(value.to_owned())),
        "--dir" => Ok(root = project::resolve(&project_root, value)),
        "--dry-run" if value.is_empty() => Ok(dry_run = true),
        _ => Err(Error::Usage(
            "tool install accepts --source URL|PATH, --dir DIR, and --dry-run",
        )),
    })?;
    Ok((id, source, root, dry_run))
}

fn parse_workload(args: &[String]) -> Result<(String, String, PathBuf), Error> {
    let requested = args.first().cloned().ok_or(Error::Usage(
        "workload install requires a local path or URL",
    ))?;
    let source = external_workload(&requested)
        .map(|workload| workload.source.to_owned())
        .unwrap_or_else(|| requested.clone());
    if external_workload(&requested).is_none()
        && valid_name(&requested)
        && !Path::new(&requested).is_file()
    {
        let aliases = EXTERNAL_WORKLOADS
            .iter()
            .map(|workload| workload.name)
            .collect::<Vec<_>>();
        if let Some(candidate) = suggestions::closest(&requested, &aliases) {
            return Err(Error::Input(format!(
                "workload source `{requested}` was not found; did you mean external workload `{candidate}`?"
            )));
        }
    }
    let filename = source
        .split('?')
        .next()
        .and_then(|value| value.rsplit('/').next())
        .unwrap_or(&source);
    let default_name = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| valid_name(value))
        .ok_or(Error::Input(
            "workload source must have a simple file name".into(),
        ))?
        .to_owned();
    let mut name = external_workload(&requested)
        .map(|workload| workload.name.to_owned())
        .unwrap_or(default_name);
    let (project_root, project_config) = project::load(Path::new("."))?;
    let mut directory = std::env::var_os("ASB_WORKLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| project::resolve(&project_root, &project_config.workloads_dir));
    parse_options(&args[1..], |flag, value| match flag {
        "--name" if valid_name(value) => Ok(name = value.to_owned()),
        "--dir" => Ok(directory = project::resolve(&project_root, value)),
        _ => Err(Error::Usage(
            "workload install accepts --name NAME and --dir DIR",
        )),
    })?;
    Ok((source, name, directory))
}

fn executable_name(agent: &str) -> &str {
    match agent {
        "workbuddy" => "codebuddy",
        "qwen-code" => "qwen",
        "mini-swe" => "mini",
        other => other,
    }
}

fn parse_options(
    args: &[String],
    mut apply: impl FnMut(&str, &str) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut index = 0;
    while index < args.len() {
        let flag = args[index].as_str();
        if flag == "--dry-run" {
            apply(flag, "")?;
            index += 1;
            continue;
        }
        let value = args
            .get(index + 1)
            .ok_or(Error::Usage("install options require values"))?;
        apply(flag, value)?;
        index += 2;
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn missing_dependency(tool: &str, operation: &str, error: std::io::Error) -> Error {
    let command = match tool {
        "npm" => {
            "curl -fsSL https://fnm.vercel.app/install | bash && eval \"$(fnm env --shell bash)\" && fnm install --lts && fnm use --lts"
        }
        "python3" => {
            "curl -LsSf https://astral.sh/uv/install.sh | sh && ~/.local/bin/uv python install 3.12"
        }
        "curl" => "sudo apt-get update && sudo apt-get install -y curl",
        "tar" => "sudo apt-get update && sudo apt-get install -y tar",
        _ => "install the missing prerequisite and rerun the command",
    };
    Error::Io(format!(
        "{operation} needs `{tool}`, but it could not be started: {error}. Install it, then rerun the command. Suggested command:\n  {command}"
    ))
}

fn install_source(source: &str, destination: &Path, kind: &str) -> Result<(), Error> {
    if source.starts_with("https://") || source.starts_with("http://") {
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--output",
            ])
            .arg(destination)
            .arg(source)
            .logged_status()
            .map_err(|error| missing_dependency("curl", kind, error))?;
        if !status.success() {
            return Err(Error::Io(format!(
                "could not download {kind} from {source}"
            )));
        }
        return Ok(());
    }
    let source_path = Path::new(source);
    let metadata = fs::metadata(source_path)
        .map_err(|error| Error::Input(format!("cannot read {kind} source {source}: {error}")))?;
    if !metadata.is_file() {
        return Err(Error::Input(format!(
            "{kind} source is not a file: {source}"
        )));
    }
    fs::copy(source_path, destination).map_err(|error| {
        Error::Io(format!(
            "cannot install {kind} at {}: {error}",
            destination.display()
        ))
    })?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| Error::Io(format!("cannot inspect {}: {error}", path.display())))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|error| {
            Error::Io(format!(
                "cannot make {} executable: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_recipes_cover_all_documented_agents() {
        for agent in [
            "codex",
            "pi",
            "opencode",
            "workbuddy",
            "gemini",
            "qwen-code",
            "aider",
            "mini-swe",
            "openhands",
            "openjiuwen",
            "cli2key",
            "llmtape",
            "mpstat",
            "iostat",
            "perf",
            "bpftrace",
            "flamegraph",
        ] {
            let recipe = recipe(agent).expect("documented agent has a recipe");
            assert!(!describe(recipe, Path::new(".asb-tools")).is_empty());
            assert!(!executable_name(agent).is_empty());
        }
    }

    #[test]
    fn llmtape_recipe_materializes_a_project_local_wrapper() {
        let recipe = recipe("llmtape").expect("llmtape recipe");
        assert!(is_llmtape(recipe));
        assert_eq!(
            executable(recipe, Path::new(".asb-tools/llmtape")),
            Path::new(".asb-tools/llmtape/bin/llmtape")
        );
        assert!(LLMTAPE_WRAPPER.contains("llmtape record"));
        assert!(LLMTAPE_WRAPPER.contains("llmtape replay"));
    }

    #[test]
    fn tool_availability_prefers_project_local_installations() {
        let root = std::env::temp_dir().join(format!("asb-tool-status-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("llmtape/bin")).unwrap();
        fs::write(root.join("llmtape/bin/llmtape"), "#!/bin/sh\n").unwrap();
        assert!(tool_available("llmtape", &root));
        assert!(!tool_available("not-a-tool", &root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn install_progress_reports_real_workload_stages() {
        let root =
            std::env::temp_dir().join(format!("asb-install-progress-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let source = root.join("prompt.txt");
        fs::write(&source, "prompt").unwrap();
        let mut events = Vec::new();
        let mut progress = |percent: u8, stage: &str| events.push((percent, stage.to_owned()));
        let mut output = Vec::new();
        workload_with_progress(
            &[
                source.to_string_lossy().into_owned(),
                "--dir".into(),
                root.join("workloads").to_string_lossy().into_owned(),
            ],
            &mut output,
            &mut progress,
        )
        .expect("local workload installation succeeds");
        assert!(
            events
                .iter()
                .any(|(percent, stage)| *percent == 2 && stage == "preparing")
        );
        assert!(
            events
                .iter()
                .any(|(percent, stage)| *percent == 100 && stage == "ready")
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tool_list_includes_builtins_and_source_fallback() {
        let mut output = Vec::new();
        tool_list(&mut output).expect("tool list succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(!text.contains("codeagent"));
        assert!(text.contains("pi"));
        assert!(text.contains("openjiuwen"));
        assert!(text.contains("llmtape"));
        assert!(text.contains("mpstat"));
        assert!(text.contains("bpftrace"));
        assert!(text.contains("custom"));
    }

    #[test]
    fn missing_prerequisite_messages_include_copyable_repair_commands() {
        let npm = missing_dependency(
            "npm",
            "agent installation",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(npm.contains("fnm install --lts"));
        let python = missing_dependency(
            "python3",
            "Python agent installation",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(python.contains("uv python install 3.12"));
        let curl = missing_dependency(
            "curl",
            "model discovery",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(curl.contains("apt-get install -y curl"));
    }

    #[test]
    fn bpftrace_repair_messages_cover_supported_distributions() {
        let host = bpftrace_host_commands();
        assert!(host.contains("Red Hat/Fedora:"));
        assert!(host.contains("sudo dnf install -y bpftrace"));
        assert!(host.contains("sudo apt-get update && sudo apt-get install -y bpftrace"));
        assert!(host.contains("openEuler:"));

        let source = bpftrace_source_commands();
        assert!(source.contains("bcc-devel"));
        assert!(source.contains("libbpfcc-dev"));
        assert!(source.contains("libbpf-dev"));
        assert!(source.contains("cmake"));
        assert!(source.contains("gcc-c++"));

        let cmake = bpftrace_missing_dependency(
            "cmake",
            std::io::Error::from(std::io::ErrorKind::NotFound),
        )
        .to_string();
        assert!(cmake.contains("Red Hat/Fedora:"));
        assert!(cmake.contains("Debian/Ubuntu:"));
        assert!(cmake.contains("openEuler:"));
        assert!(cmake.contains("rerun `asb tool install bpftrace`"));
    }

    #[test]
    fn workload_list_enumerates_prompt_files() {
        let directory =
            std::env::temp_dir().join(format!("asb-workload-list-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("directory");
        fs::write(directory.join("zeta.txt"), "z").expect("prompt");
        fs::write(directory.join("alpha.txt"), "a").expect("prompt");
        fs::write(directory.join("ignored.md"), "ignored").expect("other file");
        let mut output = Vec::new();
        workload_list(
            &["--dir".into(), directory.to_string_lossy().into_owned()],
            &mut output,
        )
        .expect("workload list succeeds");
        let text = String::from_utf8(output).expect("utf8");
        assert!(text.find("alpha").expect("alpha") < text.find("zeta").expect("zeta"));
        assert!(!text.contains("ignored"));
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn external_workload_aliases_are_short_unique_and_pinned() {
        let mut names = std::collections::BTreeSet::new();
        for workload in EXTERNAL_WORKLOADS {
            assert!(valid_name(workload.name));
            assert!(names.insert(workload.name));
            assert!(
                workload
                    .source
                    .starts_with("https://raw.githubusercontent.com/")
            );
            assert!(workload.source.split('/').any(|part| part.len() == 40));
        }
        assert_eq!(
            external_workload("swe-bench").map(|item| item.name),
            Some("swe-bench")
        );
        assert!(bundle_spec("swe-bench").is_some());
        assert!(bundle_spec("terminal-bench").is_some());
        assert!(external_workload("methodology-only").is_none());
    }

    #[test]
    fn complete_external_bundle_is_reused_when_already_installed() {
        let directory =
            std::env::temp_dir().join(format!("asb-external-bundle-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        let bundle_dir = directory.join("swe-bench");
        fs::create_dir_all(bundle_dir.join("source")).unwrap();
        fs::write(bundle_dir.join("source/README.md"), "local source").unwrap();
        let spec = bundle_spec("swe-bench").unwrap();
        fs::write(bundle_dir.join("ASB-BUNDLE"), spec.marker).unwrap();
        let mut output = Vec::new();
        let mut progress = |_percent: u8, _stage: &str| {};
        install_bundle("swe-bench", spec, &directory, &mut output, &mut progress)
            .expect("existing bundle is reused");
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("already installed")
        );
        assert_eq!(
            fs::read_to_string(bundle_dir.join("source/README.md")).unwrap(),
            "local source"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn unknown_agent_install_suggests_the_closest_builtin() {
        let error = tool(&["opencod".into()], &mut Vec::new()).expect_err("typo rejected");
        assert!(error.to_string().contains("did you mean `opencode`?"));
    }
}
