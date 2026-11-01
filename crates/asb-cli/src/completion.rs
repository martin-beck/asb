//! Shell completions generated from the authoritative Clap command tree.

use crate::{Error, io_error};
use clap_complete::Shell;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;

/// Write Bash completions without starting project discovery or any providers.
pub(crate) fn bash(output: &mut dyn Write) -> Result<(), Error> {
    write(Shell::Bash, output)
}

/// Write Zsh completions from the same schema as the CLI and Bash completions.
pub(crate) fn zsh(output: &mut dyn Write) -> Result<(), Error> {
    write(Shell::Zsh, output)
}

fn generate(shell: Shell, mut command: clap::Command) -> Vec<u8> {
    let mut bytes = if matches!(shell, Shell::Bash) {
        b"# bash completion for asb\n".to_vec()
    } else {
        Vec::new()
    };
    // Upstream expects an infallible writer. Buffer locally, then propagate
    // user output failures instead of panicking on a broken pipe.
    clap_complete::generate(shell, &mut command, "asb", &mut bytes);
    match shell {
        Shell::Bash => append_bash_catalog_lookup(&mut bytes),
        Shell::Zsh => append_zsh_catalog_lookup(&mut bytes),
        _ => {}
    }
    bytes
}

fn write(shell: Shell, output: &mut dyn Write) -> Result<(), Error> {
    output
        .write_all(&generate(shell, crate::parser::completion_command()))
        .map_err(io_error)
}

/// Print safe, one-per-line suggestions for the generated shell wrappers.
/// Project-local workload/trial entries are resolved at tab time, rather than
/// being frozen when `make install` generated the completion file.
pub(crate) fn values(args: &[OsString], output: &mut dyn Write) -> Result<(), Error> {
    let kind = args
        .first()
        .and_then(|value| value.to_str())
        .ok_or_else(|| Error::Input("completion value kind is required".into()))?;
    let context = args.get(1).and_then(|value| value.to_str());
    let explicit_project = args.get(2).filter(|value| !value.is_empty()).map(Path::new);
    let project_root = match explicit_project {
        Some(path) => crate::project::require_project_directory(path)?.0,
        None => crate::project::load(Path::new("."))?.0,
    };
    let invocation_dir = match explicit_project {
        Some(_) => project_root.clone(),
        None => std::env::current_dir().map_err(io_error)?,
    };
    let _network = crate::project::network_project_at(&project_root, &invocation_dir);
    let mut values = match kind {
        "agent" => crate::agents::names().map(str::to_owned).collect(),
        "provider" => crate::providers::names()
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        "tool" => crate::install::tool_names().map(str::to_owned).collect(),
        "collector" => crate::measurements::available_collectors(),
        "workload" => local_workloads(&project_root, &invocation_dir),
        "trial-set" => local_trial_sets(&project_root, &invocation_dir),
        "trial" => context.map_or_else(Vec::new, |name| {
            local_trials(name, &project_root, &invocation_dir)
        }),
        // A provider-qualified model completion asks the same discovery path as
        // setup, so a refreshed provider catalog is immediately useful.  A
        // failed/offline lookup falls back to the documented local snapshot.
        "model" => context.map_or_else(all_known_models, |provider| {
            crate::models::discover_for_setup(provider).unwrap_or_else(|_| known_models(provider))
        }),
        _ => {
            return Err(Error::Input(format!(
                "unknown completion value kind `{kind}`"
            )));
        }
    };
    values.sort();
    values.dedup();
    for value in values {
        if !value.contains(['\n', '\r']) {
            writeln!(output, "{value}").map_err(io_error)?;
        }
    }
    Ok(())
}

fn known_models(provider: &str) -> Vec<String> {
    crate::providers::known_models(provider)
        .unwrap_or_default()
        .iter()
        .map(|value| (*value).to_owned())
        .collect()
}

fn all_known_models() -> Vec<String> {
    crate::providers::names()
        .iter()
        .flat_map(|provider| {
            known_models(provider)
                .into_iter()
                .flat_map(move |model| [model.clone(), format!("{provider}={model}")])
        })
        .collect()
}

fn workload_directory(project: &Path, invocation_dir: &Path) -> Option<std::path::PathBuf> {
    let (root, config) = crate::project::load(project).ok()?;
    Some(
        crate::project::environment_path("ASB_WORKLOAD_DIR", invocation_dir)
            .unwrap_or_else(|| crate::project::resolve(&root, &config.workloads_dir)),
    )
}

fn local_workloads(project: &Path, invocation_dir: &Path) -> Vec<String> {
    let mut values = crate::workloads::builtin_names()
        .chain(crate::install::external_workload_names())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(directory) = workload_directory(project, invocation_dir) {
        values.extend(read_directory_names(&directory, ".txt"));
    }
    values
}

fn local_trial_sets(project: &Path, invocation_dir: &Path) -> Vec<String> {
    workload_directory(project, invocation_dir).map_or_else(Vec::new, |directory| {
        read_directory_names(&directory, ".workload.toml")
    })
}

fn local_trials(workload: &str, project: &Path, invocation_dir: &Path) -> Vec<String> {
    let Some(directory) = workload_directory(project, invocation_dir) else {
        return Vec::new();
    };
    crate::workloads::installed_trials(workload, &directory)
        .unwrap_or_default()
        .into_iter()
        .map(|trial| trial.id)
        .collect()
}

fn read_directory_names(directory: &Path, suffix: &str) -> Vec<String> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_suffix(suffix))
                .map(str::to_owned)
        })
        .collect()
}

fn append_bash_catalog_lookup(bytes: &mut Vec<u8>) {
    let mut script = String::from_utf8(std::mem::take(bytes)).expect("clap emits UTF-8");
    script = script.replacen("_asb() {", "_asb_static() {", 1);
    script = script.replace("complete -F _asb asb", "complete -F _asb_static asb");
    script.push_str(BASH_CATALOG_LOOKUP);
    *bytes = script.into_bytes();
}

fn append_zsh_catalog_lookup(bytes: &mut Vec<u8>) {
    let mut script = String::from_utf8(std::mem::take(bytes)).expect("clap emits UTF-8");
    script = script.replacen("_asb() {", "_asb_static() {", 1);
    script.push_str(ZSH_CATALOG_LOOKUP);
    *bytes = script.into_bytes();
}

const BASH_CATALOG_LOOKUP: &str = r#"

_asb_catalog_value() {
    "${COMP_WORDS[0]}" --__complete-values "$@" 2>/dev/null
}

_asb_catalog_option() {
    local name="$1" value="" i
    for ((i=1; i<COMP_CWORD; i++)); do
        if [[ "${COMP_WORDS[i]}" == "${name}" ]]; then
            if [[ "${COMP_WORDS[i+1]}" == = ]]; then
                value="${COMP_WORDS[i+2]}"
            else
                value="${COMP_WORDS[i+1]}"
            fi
        elif [[ "${COMP_WORDS[i]}" == "${name}="* ]]; then
            value="${COMP_WORDS[i]#*=}"
        fi
    done
    printf '%s' "$value"
}

_asb_catalog_complete() {
    local cur prev kind context="" values project command="" subcommand="" i skip=0 word
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    for ((i=1; i<COMP_CWORD; i++)); do
        word="${COMP_WORDS[i]}"
        if ((skip)); then ((skip--)); continue; fi
        case "$word" in
            --project-dir) if [[ "${COMP_WORDS[i+1]}" == = ]]; then skip=2; else skip=1; fi; continue ;;
            --project-dir=*) continue ;;
        esac
        if [[ -z "$command" ]]; then command="$word"; else subcommand="$word"; break; fi
    done
    case "$prev" in
        --agent) kind=agent ;;
        --provider) kind=provider ;;
        --model) kind=model; context="$(_asb_catalog_option --provider)" ;;
        --workload) kind=workload ;;
        --trial) kind=trial; context="$(_asb_catalog_option --workload)" ;;
        --trial-set) kind=trial-set ;;
        --measure|--collector) kind=collector ;;
    esac
    if [[ -z "$kind" && "$command" == tool && "$subcommand" == install ]]; then
        kind=tool
    elif [[ -z "$kind" && "$command" == workload && ( "$subcommand" == install || "$subcommand" == describe ) ]]; then
        kind=workload
    elif [[ -z "$kind" && "$command" == provider && "$subcommand" == models ]]; then
        kind=provider
    fi
    [[ -n "$kind" ]] || return 1
    project="$(_asb_catalog_option --project-dir)"
    values="$(_asb_catalog_value "$kind" "$context" "$project")" || return 1
    COMPREPLY=( $(compgen -W "$values" -- "$cur") )
    return 0
}

_asb_config_file_complete() {
    # clap_complete's Bash backend completes option values with ValueHint but
    # treats a first positional as a command-token slot.  These are ASB's
    # positional config-file commands; preserve normal option completion once
    # a config was supplied.
    local cur="${COMP_WORDS[COMP_CWORD]}" command="${COMP_WORDS[1]}" word i skip_value=0
    case "$command" in
        run|sweep|compare) ;;
        *) return 1 ;;
    esac
    [[ "$cur" != -* ]] || return 1
    for ((i=2; i<COMP_CWORD; i++)); do
        word="${COMP_WORDS[i]}"
        if ((skip_value)); then
            skip_value=0
            continue
        fi
        case "$word" in
            --*=*) continue ;;
            --repeat|--max-steps|--track|--seed|--cassette|--measurements|--cassette-root|--jobs|--max-active-attempts)
                skip_value=1
                continue
                ;;
            -*) continue ;;
            *) return 1 ;;
        esac
    done
    COMPREPLY=($(compgen -f -- "$cur"))
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        compopt -o filenames
    fi
    return 0
}

_asb() {
    _asb_catalog_complete && return
    _asb_config_file_complete && return
    _asb_static "$@"
}
complete -F _asb asb
"#;

const ZSH_CATALOG_LOOKUP: &str = r#"

_asb_catalog_option() {
    local option="$1" value="" i
    for ((i=2; i<CURRENT; i++)); do
        if [[ "${words[i]}" == "$option" ]]; then
            value="${words[i+1]}"
        elif [[ "${words[i]}" == "$option="* ]]; then
            value="${words[i]#*=}"
        fi
    done
    print -r -- "$value"
}

_asb_catalog_complete() {
    local previous="${words[CURRENT-1]}" kind="" context="" project command="" subcommand="" word
    local i skip=0
    for ((i=2; i<CURRENT; i++)); do
        word="${words[i]}"
        if ((skip)); then skip=0; continue; fi
        case "$word" in
            --project-dir) skip=1; continue ;;
            --project-dir=*) continue ;;
        esac
        if [[ -z "$command" ]]; then command="$word"; else subcommand="$word"; break; fi
    done
    case "$previous" in
        --agent) kind=agent ;;
        --provider) kind=provider ;;
        --model) kind=model; context="$(_asb_catalog_option --provider)" ;;
        --workload) kind=workload ;;
        --trial) kind=trial; context="$(_asb_catalog_option --workload)" ;;
        --trial-set) kind=trial-set ;;
        --measure|--collector) kind=collector ;;
    esac
    if [[ -z "$kind" && "$command" == tool && "$subcommand" == install ]]; then
        kind=tool
    elif [[ -z "$kind" && "$command" == workload && ( "$subcommand" == install || "$subcommand" == describe ) ]]; then
        kind=workload
    elif [[ -z "$kind" && "$command" == provider && "$subcommand" == models ]]; then
        kind=provider
    fi
    [[ -n "$kind" ]] || return 1
    local -a candidates
    project="$(_asb_catalog_option --project-dir)"
    candidates=("${(@f)$("${words[1]}" --__complete-values "$kind" "$context" "$project" 2>/dev/null)}")
    (( ${#candidates} )) || return 1
    _describe -t values "$kind" candidates
}

_asb() {
    _asb_catalog_complete || _asb_static "$@"
}
compdef _asb asb
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_schema(command: &clap::Command, script: &str) {
        for argument in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
            if let Some(long) = argument.get_long() {
                assert!(
                    script.contains(&format!("--{long}")),
                    "{}: missing --{long}",
                    command.get_name()
                );
            }
            if let Some(short) = argument.get_short() {
                assert!(script.contains(&format!("-{short}")), "missing -{short}");
            }
        }
        for child in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
            assert!(
                script.contains(child.get_name()),
                "missing {}",
                child.get_name()
            );
            assert_schema(child, script);
        }
    }

    #[test]
    fn every_current_public_command_and_option_is_in_both_shells() {
        for shell in [Shell::Bash, Shell::Zsh] {
            let mut command = crate::parser::completion_command();
            command.build();
            let script = String::from_utf8(generate(shell, command.clone())).unwrap();
            assert_schema(&command, &script);
            for (kind, values) in [
                (
                    "agent",
                    crate::agents::names()
                        .map(str::to_owned)
                        .collect::<Vec<_>>(),
                ),
                (
                    "provider",
                    crate::providers::names()
                        .iter()
                        .map(|value| (*value).to_owned())
                        .collect(),
                ),
                (
                    "tool",
                    crate::install::tool_names().map(str::to_owned).collect(),
                ),
                ("collector", crate::measurements::available_collectors()),
            ] {
                for value in values {
                    assert!(script.contains(&value), "{shell:?} missing {kind} {value}");
                }
            }
            for workload in
                crate::workloads::builtin_names().chain(crate::install::external_workload_names())
            {
                assert!(
                    script.contains(workload),
                    "{shell:?} missing workload {workload}"
                );
            }
            for provider in crate::providers::names() {
                for model in crate::providers::known_models(provider).unwrap_or_default() {
                    assert!(
                        script.contains(model),
                        "{shell:?} missing known {provider} model {model}"
                    );
                }
            }
            for option in [
                "--configure-host",
                "--show-output",
                "--respect-record-timings",
                "--import-grades",
                "--grade",
                "--inspect-grade",
            ] {
                assert!(script.contains(option), "{shell:?} missing {option}");
            }
            assert!(!script.contains("--__generate-completion"));
        }
    }

    #[test]
    fn new_commands_and_options_do_not_need_a_second_completion_definition() {
        let command = crate::parser::completion_command().subcommand(
            clap::Command::new("future-command")
                .arg(clap::Arg::new("future-flag").long("future-flag")),
        );
        for shell in [Shell::Bash, Shell::Zsh] {
            let script = String::from_utf8(generate(shell, command.clone())).unwrap();
            assert!(script.contains("future-command"));
            assert!(script.contains("--future-flag"));
        }
    }

    #[test]
    fn both_completion_outputs_propagate_writer_failure() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "fixture",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(bash(&mut Broken).is_err());
        assert!(zsh(&mut Broken).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn bash_completes_positional_config_files_for_run_sweep_and_compare() {
        let fixture = std::env::temp_dir().join(format!(
            "asb-completion-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        fs::write(fixture.join("selected.toml"), "# fixture\n").unwrap();
        let script =
            String::from_utf8(generate(Shell::Bash, crate::parser::completion_command())).unwrap();

        for words in [
            &["run", "sel"][..],
            &["run", "--record", "sel"],
            &["sweep", "sel"],
            &["sweep", "--jobs", "1", "sel"],
            &["compare", "sel"],
        ] {
            let command = words[0];
            let result = std::process::Command::new("bash")
                .args(["--noprofile", "--norc", "-c"])
                .arg(format!(
                    "{script}\ncd \"$1\"; shift; COMP_WORDS=(asb \"$@\"); COMP_CWORD=$#; _asb; printf '%s\\n' \"${{COMPREPLY[@]}}\""
                ))
                .arg("completion-fixture")
                .arg(&fixture)
                .args(words)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{command}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert!(
                String::from_utf8(result.stdout)
                    .unwrap()
                    .lines()
                    .any(|candidate| candidate == "selected.toml"),
                "{command} did not complete the config file"
            );
        }
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn zsh_emits_native_path_completion_for_every_path_position() {
        let script =
            String::from_utf8(generate(Shell::Zsh, crate::parser::completion_command())).unwrap();
        // Zsh's backend honours Clap ValueHint directly.  Keep the assertions
        // on generated output so a future parser change cannot silently turn
        // path positions back into generic string completion.
        for label in ["run", "sweep", "compare"] {
            let start = script.find(&format!("({label})")).unwrap();
            let section = &script[start..script[start..].find(";;").unwrap() + start];
            assert!(section.contains(":path:_files"), "{label}: {section}");
        }
        let recover = script.find("(recover)").unwrap();
        let section = &script[recover..script[recover..].find(";;").unwrap() + recover];
        assert!(section.contains(":reference:_files"), "{section}");
    }

    #[cfg(unix)]
    #[test]
    fn bash_completes_each_command_context_with_its_actual_flags() {
        fn visit(command: &clap::Command, path: Vec<String>, script: &str) {
            let mut child = std::process::Command::new("bash");
            child.env_clear();
            if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
                child.env("LLVM_PROFILE_FILE", profile);
            }
            let result = child
                .args(["--noprofile", "--norc", "-c"])
                .arg(format!("{script}\nCOMP_WORDS=(\"$@\"); COMP_CWORD=$(($# - 1)); _asb asb --; printf '%s\\n' \"${{COMPREPLY[@]}}\""))
                .arg("completion-fixture").args(&path).arg("--")
                .output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let candidates = String::from_utf8(result.stdout).unwrap();
            for argument in command.get_arguments().filter(|arg| !arg.is_hide_set()) {
                if let Some(long) = argument.get_long() {
                    assert!(
                        candidates
                            .lines()
                            .any(|candidate| candidate == format!("--{long}")),
                        "{path:?} missing --{long}: {candidates}"
                    );
                }
            }
            for child in command.get_subcommands().filter(|cmd| !cmd.is_hide_set()) {
                let mut next = path.clone();
                next.push(child.get_name().into());
                visit(child, next, script);
            }
        }
        let mut command = crate::parser::completion_command();
        command.build();
        let script = String::from_utf8(generate(Shell::Bash, command.clone())).unwrap();
        visit(&command, vec!["asb".into()], &script);
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::install;

    #[test]
    fn both_shells_publish_core_commands_registry_tools_and_stable_output() {
        for generator in [bash, zsh] {
            let mut output = Vec::new();
            generator(&mut output).unwrap();
            let text = String::from_utf8(output.clone()).unwrap();
            for command in [
                "init", "setup", "run", "sweep", "compare", "provider", "tool", "workload",
            ] {
                assert!(text.contains(command), "missing {command}");
            }
            for tool in install::tool_names() {
                assert!(text.contains(tool), "missing installable tool {tool}");
            }
            for flag in [
                "--repeat",
                "--max-steps",
                "--track",
                "--seed",
                "--record",
                "--replay",
                "--report",
                "--source",
                "--dry-run",
            ] {
                assert!(text.contains(flag), "missing {flag}");
            }
            let mut repeated = Vec::new();
            generator(&mut repeated).unwrap();
            assert_eq!(repeated, output, "completion output must be reproducible");
        }
    }

    #[test]
    fn both_shells_report_output_failures_without_panicking() {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("fixture completion output failure"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        for generator in [bash, zsh] {
            assert!(
                generator(&mut BrokenWriter)
                    .unwrap_err()
                    .to_string()
                    .contains("fixture completion output failure")
            );
        }
    }
}
