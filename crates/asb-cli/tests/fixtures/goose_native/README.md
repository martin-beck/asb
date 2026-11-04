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
