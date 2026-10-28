use super::swe_bench::SweBenchAdapter;
pub(super) static ADAPTER: SweBenchAdapter = SweBenchAdapter {
    id: "swe-bench-lite",
    dataset: "SWE-bench/SWE-bench_Lite",
    revision: "b0dde1093fe417d83b7184254edf8199c1f0dff5",
    source_sha256: "ff31aa1f8d2bb78f16559524264a99b868b7218ca82b390dc750bbc59538aa20",
    expected_tasks: 300,
};
