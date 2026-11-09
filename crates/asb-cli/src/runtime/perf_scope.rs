//! Explicit perf recording argument construction by sampling scope.

/// Build the existing process-tree recording command's argument vector.
/// A portable software event avoids expanding hardware defaults to hybrid PMUs;
/// the 199 Hz per-thread frequency also retains short-lived worker samples.
pub fn process_tree_record_args(pid: u32) -> Vec<String> {
    [
        "record",
        "-e",
        "cpu-clock",
        "-g",
        "-F",
        "199",
        "--inherit",
        "-p",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain(std::iter::once(pid.to_string()))
    .collect()
}

/// Build CPU-wide recording arguments for one assigned logical CPU group.
///
/// This samples every task and kernel activity on those CPUs, including
/// unrelated host work and collectors. It does not reserve CPUs or change the
/// separate process-tree perf-stat scope. Callers must enforce and verify
/// allocation for every relevant process and container before using it.
pub fn cpu_wide_record_args(cpus: &[u32]) -> Result<Vec<String>, String> {
    super::affinity::AffinityMask::new(cpus)?;
    let list = cpus
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    Ok(["record", "-e", "cpu-clock", "-g", "-F", "199", "-a", "-C"]
        .into_iter()
        .map(str::to_owned)
        .chain(std::iter::once(list))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_wide_scope_is_explicit_and_never_uses_a_pid() {
        assert_eq!(
            cpu_wide_record_args(&[1, 3, 4]).unwrap(),
            [
                "record",
                "-e",
                "cpu-clock",
                "-g",
                "-F",
                "199",
                "-a",
                "-C",
                "1,3,4"
            ]
        );
        assert!(cpu_wide_record_args(&[]).is_err());
        assert!(cpu_wide_record_args(&[1, 1]).is_err());
        assert_eq!(
            process_tree_record_args(42),
            [
                "record",
                "-e",
                "cpu-clock",
                "-g",
                "-F",
                "199",
                "--inherit",
                "-p",
                "42"
            ]
        );
    }
}
