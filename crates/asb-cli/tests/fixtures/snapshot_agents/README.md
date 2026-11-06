# Installed agent snapshot qualification

`swe_c2.py` exercises one installed agent through ASB with an official
SWE-bench trial, a deterministic local model server, and a local HTTP side
service. It records a shell command that contacts the side service and applies
the upstream reference patch, then stops both services and replays the sealed
recording in two isolated replicas. The fixture requires official passing
grades, cleanup and complete native replay consumption for both replicas, and
checks that the recorded HTTP request is not repeated. It retains its project
artifacts and does not call an online model provider. This is a replay test,
not a model-solving or performance measurement.

Run with an isolated initialized project containing the selected installed
agent, SWE-bench and its SDK, and a cached task image. Supply the ASB binary and
run this script with the installed SWE-bench Python:

```sh
ASB_TEST_SWECATALOG_ROOT=/path/to/project \
ASB_TEST_BENCHMARK_ASB=/path/to/asb \
ASB_SNAPSHOT_AGENT=opencode \
/path/to/project/.asb-tools/swebench/venv/bin/python \
  crates/asb-cli/tests/fixtures/snapshot_agents/swe_c2.py
```

The default task is `sympy__sympy-14774`; set `ASB_SNAPSHOT_TASK` to use
another installed task and its reference patch. `ASB_SNAPSHOT_AGENT` accepts
`opencode`, `claude-code`, or `openjiuwen`. To revalidate a retained successful
recording without repeating recording, set `ASB_TEST_RESUME_RECORDING` to its
fixture evidence directory. New replay outputs go into a fresh child directory.
The fixture requires a snapshot-capable guest and its normal Btrfs and rsync
prerequisites; a VM can supply those independently of the host filesystem.
