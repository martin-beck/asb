# Experimental Goose native dispatch fixture

This fixture tests a pinned Goose source build with ASB's native broker. It is
offline after dependencies are installed. It uses directory copies for snapshot
effects and does not qualify Btrfs, full agent runs, or subagent replay.

Set `ASB_ROOT` to this checkout and `GOOSE_SOURCE` to a disposable, clean
Goose 1.54.0 source tree. Prepare the tree with
`python3 "$ASB_ROOT/crates/asb-cli/assets/scripts/goose-native-prepare.py" "$GOOSE_SOURCE"`.
Copy `probe.rs` to `crates/goose/src/agents/extension_manager/asb_native_probe.rs`
in that tree and copy the three adjacent `asb_*_probe` files to the same
directory. Then, from the Goose source tree, run:

```sh
ASB_GOOSE_HOOK_SOURCE="$ASB_ROOT/crates/asb-cli/assets/scripts/goose-native-hook.rs" \
ASB_TEST_NATIVE_BROKER_MODULE="$ASB_ROOT/crates/asb-cli/assets/scripts/native-tool-broker.mjs" \
ASB_TEST_BOUNDARY_MODULE="$ASB_ROOT/crates/asb-cli/assets/scripts/native-snapshot-boundary.mjs" \
ASB_TEST_SNAPSHOT_MODULES="$ASB_ROOT/crates/asb-cli/assets/benchmarks" \
ASB_TEST_NODE="$(command -v node)" \
cargo test --offline --locked -p goose --features scheduler --lib asb_native_probe -- --test-threads=1
```

The fixture is opt-in; normal ASB CI does not download or build Goose.

To separately exercise the real Btrfs snapshot supervisor, with one recording
and four concurrent replay workspaces, run the following on a disposable Linux
host or VM with `btrfs`, `mkfs.btrfs`, `rsync`, loop devices, and mount
capability. Set the directory to a new path on a filesystem with ample space:

```sh
sudo env ASB_BTRFS_FIXTURE_DIR=/path/to/new/disposable-btrfs-fixture \
  python3 "$ASB_ROOT/crates/asb-cli/tests/fixtures/goose_native/btrfs_smoke.py"
```

The fixture retains its images and diagnostics for inspection. It tests
snapshot recording and restoration only; it does not execute Goose or qualify
full filesystem replay.

For the combined Goose and Btrfs fixture, first build the patched Goose CLI
with `cargo build --locked -p goose-cli --bin goose --features goose/asb-native-probe`.
This retains Goose's default features. Then run:

```sh
sudo env ASB_GOOSE_BINARY=/absolute/path/to/patched/goose \
  ASB_TEST_NODE=/absolute/path/to/node \
  python3 "$ASB_ROOT/crates/asb-cli/tests/fixtures/goose_native/goose_btrfs_probe.py" \
  /path/to/new/disposable-goose-btrfs-fixture
```

This fixture records a Goose background child and replays four independent
replicas with the mock provider stopped, using real Btrfs snapshot streams.
Each Goose replica sends its recorded-to-live session mapping to its own LLM
proxy; the proxy does not read the fixture's retained identity files. Setting
`ASB_TEST_SKIP_MAP_URL=1` makes this a negative control that should fail.
It is not an ASB CLI or SWE-bench qualification and does not cover all Goose
tools, arbitrary filesystem access, or all subagent behaviors.

Set `ASB_TEST_CONTAINER_IMAGE` to a locally available Linux task image to run
Goose inside network-isolated containers using ASB's relay. Also set
`ASB_RELAY_BINARY`, `ASB_TEST_LIB_DIR`, and `ASB_TEST_LOADER` as described
below. The fixture uses the host loader for the test binaries and bind-mounts
the disposable fixture directory; it does not test ASB's installer or perform an
official workload evaluation.

`runtime_bundle_probe.py` separately stages `ASB_GOOSE_BINARY` with ASB's
runtime bundler and runs its version command in the image named by
`ASB_TEST_IMAGE`. Pass a new disposable directory as its only argument. This
checks loader and size compatibility, not replay or installation.

To check the separate container-to-host control transport, run
`relay_control_probe.py` with a new output directory and set
`ASB_RELAY_BINARY` to a built ASB executable, `ASB_TEST_NODE` to Node,
`ASB_TEST_CASSETTE` to the previous fixture's `llm.jsonl`, and
`ASB_TEST_IMAGE` to an already installed Linux task image containing Python.
Set `ASB_TEST_LIB_DIR` to the ASB executable's host library directory and
`ASB_TEST_LOADER` to the loader filename in that directory. The fixture starts
a container with networking disabled, uses ASB's own relay, and checks that
the control message does not consume a provider exchange. It requires Docker
access and retains only the output directory; it removes its container.
