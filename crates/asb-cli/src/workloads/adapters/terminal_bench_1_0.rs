use super::terminal_bench::TerminalBenchAdapter;
pub(super) static ADAPTER: TerminalBenchAdapter = TerminalBenchAdapter {
    id: "terminal-bench-1.0",
    source: "harbor-framework/terminal-bench-1",
    revision: "91e10457b5410f16c44364da1a34cb6de8c488a5",
    split: "terminal-bench-core@0.1.1",
    archive: "https://github.com/harbor-framework/terminal-bench-1/archive/91e10457b5410f16c44364da1a34cb6de8c488a5.tar.gz",
    engine: super::terminal_bench::ORIGINAL_ENGINE,
    version: "0.2.18",
    expected_tasks: 80,
    tasks_root: "tasks",
};
