use super::swe_bench::SweBenchAdapter;
pub(super) static ADAPTER: SweBenchAdapter = SweBenchAdapter {
    id: "swe-bench-verified",
    dataset: "SWE-bench/SWE-bench_Verified",
    revision: "78f471bf655a3137b2e8a75af1501690ec009ec3",
    source_sha256: "ff31aa1f8d2bb78f16559524264a99b868b7218ca82b390dc750bbc59538aa20",
    expected_tasks: 500,
};
