use super::terminal_bench::TerminalBenchAdapter;
pub(super) static ADAPTER: TerminalBenchAdapter = TerminalBenchAdapter {
    id: "terminal-bench-2.1",
    source: "harbor-framework/terminal-bench-2-1",
    revision: "7131e4375048a0e408a8fb404b5f499d726b695b",
    split: "tasks",
    archive: "https://github.com/harbor-framework/terminal-bench-2-1/archive/7131e4375048a0e408a8fb404b5f499d726b695b.tar.gz",
    engine: super::terminal_bench::HARBOR_ENGINE,
    version: "0.24.0",
    expected_tasks: 89,
    tasks_root: "tasks",
};
