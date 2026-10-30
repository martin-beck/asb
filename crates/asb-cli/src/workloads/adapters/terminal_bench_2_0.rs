use super::terminal_bench::TerminalBenchAdapter;

/// The original immutable Terminal-Bench 2.0 task population.
pub(super) static ADAPTER: TerminalBenchAdapter = TerminalBenchAdapter {
    id: "terminal-bench-2.0",
    source: "harbor-framework/terminal-bench-2",
    revision: "2fd12b88aafdd04a52c298e3940bcb189f9766d6",
    split: "repository-root-tasks",
    archive: "https://github.com/harbor-framework/terminal-bench-2/archive/2fd12b88aafdd04a52c298e3940bcb189f9766d6.tar.gz",
    engine: super::terminal_bench::HARBOR_ENGINE,
    version: "0.24.0",
    expected_tasks: 89,
    tasks_root: "",
};
