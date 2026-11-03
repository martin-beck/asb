//! Passive metadata tests: synthetic receipt state, never agent execution.
use super::*;
use crate::runner::campaign::StartIntent;
use crate::runner::campaign::generation::{Catalog, Context, Expected};

#[cfg(target_os = "linux")]
#[path = "generation_owner_death_tests.rs"]
mod owner_death;

fn load(guard: &Guard, sweep: &SweepPlan, results: &Path) -> Result<Catalog, Error> {
    load_with_limit(guard, sweep, results, MAX_GENERATION)
}

fn load_with_limit(
    guard: &Guard,
    sweep: &SweepPlan,
    results: &Path,
    limit: usize,
) -> Result<Catalog, Error> {
    let layouts = sweep.read_campaigns(results)?;
    let expected = sweep
        .entries
        .iter()
        .zip(&layouts)
        .map(|(entry, layout)| Expected {
            reference: entry.campaign.as_ref().unwrap(),
            plan: &layout.plan,
        })
        .collect::<Vec<_>>();
    Catalog::read_with_metadata_limit(
        &guard.inputs.directory,
        Context {
            run_id: &sweep.run_id,
            input_digest: &guard.inputs.initial.digest,
            original_index: Blob::of(&fs::read(results.join("plan.json")).unwrap()),
        },
        &expected,
        limit,
    )
}

fn rewrite_child(results: &Path, mutate: impl FnOnce(&mut Admission, &Path)) {
    let base = results.join(".checkpoint").join(ROOT);
    let child = base.join("successors/000001");
    let path = child.join("admitted.json");
    let mut admission: Admission = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    mutate(&mut admission, &child);
    let bytes = serde_json::to_vec_pretty(&admission).unwrap();
    fs::write(path, &bytes).unwrap();
    let path = base.join("next.json");
    let mut link: crate::runner::campaign::generation::NextReference =
        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    link.admission = Blob::of(&bytes);
    fs::write(path, serde_json::to_vec_pretty(&link).unwrap()).unwrap();
}

#[test]
fn successor_lineage_rejects_recomputed_hashes_for_mutated_inheritance_or_identity() {
    for case in [
        "ordinal",
        "previous-generation",
        "previous-admission",
        "receipt",
        "duplicate-id",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        append_metadata_node(&results, 1, &[2]);
        rewrite_child(&results, |admission, child| match case {
            "ordinal" => admission.lineage.as_mut().unwrap().ordinal += 1,
            "previous-generation" => {
                admission.lineage.as_mut().unwrap().previous_generation = "aaaa".into()
            }
            "previous-admission" => {
                admission
                    .lineage
                    .as_mut()
                    .unwrap()
                    .previous_admission
                    .sha256 = "0".repeat(64)
            }
            "receipt" => {
                let path = child.join("000000.json");
                let mut manifest: Manifest =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                manifest.completed[0].receipt.sha256 = "0".repeat(64);
                let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
                fs::write(path, &bytes).unwrap();
                admission.manifests[0].content = Blob::of(&bytes);
            }
            "duplicate-id" => {
                admission.generation = admission
                    .lineage
                    .as_ref()
                    .unwrap()
                    .previous_generation
                    .clone();
                for reference in &mut admission.manifests {
                    let path = child.join(manifest_path(reference.configuration));
                    let mut manifest: Manifest =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    manifest.generation = admission.generation.clone();
                    let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
                    fs::write(path, &bytes).unwrap();
                    reference.content = Blob::of(&bytes);
                }
            }
            _ => unreachable!(),
        });
        if case == "duplicate-id" {
            let path = results.join(".checkpoint").join(ROOT).join("next.json");
            let mut link: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let admission: Admission = serde_json::from_slice(
                &fs::read(results.join(".checkpoint").join(ROOT).join("admitted.json")).unwrap(),
            )
            .unwrap();
            link["generation"] = admission.generation.into();
            fs::write(path, serde_json::to_vec_pretty(&link).unwrap()).unwrap();
        }
        assert!(load(&guard, &sweep, &results).is_err(), "{case}");
    }
}

#[test]
fn successor_metadata_budget_is_cumulative_and_exact_at_the_boundary() {
    let (_fixture, sweep, results, guard, _workers) = published_mixed();
    let child = append_metadata_node(&results, 1, &[2]);
    let base = results.join(".checkpoint").join(ROOT);
    let mut total = fs::metadata(base.join("next.json")).unwrap().len() as usize;
    for node in [&base, &child] {
        total += fs::metadata(node.join("admitted.json")).unwrap().len() as usize;
        for configuration in 0..sweep.entries.len() {
            total += fs::metadata(node.join(manifest_path(configuration)))
                .unwrap()
                .len() as usize;
        }
    }
    assert!(load_with_limit(&guard, &sweep, &results, total).is_ok());
    assert!(load_with_limit(&guard, &sweep, &results, total - 1).is_err());
    let alias = base.join(".aaaa.pending");
    fs::hard_link(base.join("admitted.json"), &alias).unwrap();
    let charged = total + fs::metadata(&alias).unwrap().len() as usize + ".aaaa.pending".len();
    assert!(load_with_limit(&guard, &sweep, &results, charged).is_ok());
    assert!(load_with_limit(&guard, &sweep, &results, charged - 1).is_err());
    assert!(
        alias.exists(),
        "passive limit validation must not clean staging"
    );
}

#[test]
fn declared_pending_aliases_remain_passive_until_owned_durability_confirmation() {
    for leaf in ["admitted.json", "000000.json", "next.json"] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let child = append_metadata_node(&results, 1, &[2]);
        let base = results.join(".checkpoint").join(ROOT);
        let target = base.join(leaf);
        let original = fs::read(&target).unwrap();
        let alias = base.join(".aaaa.pending");
        fs::hard_link(&target, &alias).unwrap();
        let mut catalog = load(&guard, &sweep, &results).unwrap();
        let reference = catalog.reference(0).unwrap();
        assert!(alias.exists());
        catalog.recheck_for_publication(None).unwrap();
        // File-sync failure retains the duplicate and grants no confirmation.
        assert!(
            catalog
                .confirm_reactivation(|path| if path == alias {
                    Err(Error::Io("injected alias sync failure".into()))
                } else {
                    Ok(())
                })
                .is_err()
        );
        assert!(alias.exists());
        assert_eq!(fs::read(&target).unwrap(), original);
        // Unlink succeeded, then directory sync failed. A fresh reader sees no
        // alias; it must STILL run the complete retained authority confirmation.
        assert!(
            catalog
                .confirm_reactivation(|path| if path == base {
                    Err(Error::Io(
                        "injected post-unlink directory sync failure".into(),
                    ))
                } else {
                    Ok(())
                })
                .is_err()
        );
        assert!(!alias.exists());
        let mut recovered = load(&guard, &sweep, &results).unwrap();
        let mut synced = std::collections::BTreeSet::new();
        recovered
            .confirm_reactivation(|path| {
                synced.insert(path.to_path_buf());
                Ok(())
            })
            .unwrap();
        for path in [
            &target,
            &base,
            base.parent().unwrap(),
            &base.join("successors"),
            &child,
            &child.join("admitted.json"),
            &base.join("next.json"),
        ] {
            assert!(
                synced.contains(path),
                "missing retained authority sync {}",
                path.display()
            );
        }
        assert_eq!(recovered.reference(0).unwrap(), reference);
        assert_eq!(fs::read(target).unwrap(), original);
    }
}

#[test]
fn pending_alias_validation_refuses_foreign_copies_links_and_replacements_without_cleanup() {
    for kind in [
        "copy",
        "foreign-link",
        "duplicate",
        "malformed",
        "too-long",
        "symlink",
        "target-replaced",
        "target-missing",
        "alias-replaced",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let base = results.join(".checkpoint").join(ROOT);
        let target = base.join("admitted.json");
        let bytes = fs::read(&target).unwrap();
        let alias = base.join(".aaaa.pending");
        fs::hard_link(&target, &alias).unwrap();
        let outside = results.join("retained-foreign-alias");
        let cached = (kind == "alias-replaced").then(|| load(&guard, &sweep, &results).unwrap());
        match kind {
            "copy" | "alias-replaced" => {
                fs::remove_file(&alias).unwrap();
                fs::copy(&target, &alias).unwrap();
            }
            "foreign-link" => {
                fs::remove_file(&alias).unwrap();
                fs::copy(&target, &outside).unwrap();
                fs::hard_link(&outside, &alias).unwrap();
            }
            "duplicate" => fs::hard_link(&target, base.join(".bbbb.pending")).unwrap(),
            "malformed" => fs::rename(&alias, base.join(".not-an-id.pending")).unwrap(),
            "too-long" => {
                fs::rename(&alias, base.join(format!(".{}.pending", "a".repeat(129)))).unwrap()
            }
            "symlink" => {
                fs::remove_file(&alias).unwrap();
                std::os::unix::fs::symlink(&target, &alias).unwrap();
            }
            "target-replaced" => {
                fs::rename(&target, &outside).unwrap();
                fs::copy(&outside, &target).unwrap();
            }
            "target-missing" => fs::rename(&target, &outside).unwrap(),
            _ => unreachable!(),
        }
        let before: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        if let Some(mut catalog) = cached {
            assert!(catalog.recheck_for_publication(None).is_err());
            assert!(catalog.confirm_reactivation(|_| Ok(())).is_err());
        } else {
            assert!(load(&guard, &sweep, &results).is_err(), "{kind}");
        }
        let after: Vec<_> = fs::read_dir(&base)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            std::collections::BTreeSet::from_iter(before),
            std::collections::BTreeSet::from_iter(after)
        );
        assert_eq!(
            fs::read(if kind == "target-missing" {
                outside
            } else {
                target
            })
            .unwrap(),
            bytes
        );
    }
}

#[cfg(unix)]
#[test]
fn successor_metadata_never_follows_child_or_link_aliases() {
    for leaf in ["next.json", "successors/000001"] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        append_metadata_node(&results, 1, &[2]);
        let base = results.join(".checkpoint").join(ROOT);
        let original = base.join(leaf);
        // Outside ROOT: an extra root entry would make the envelope fail before
        // traversal reached the child alias, proving the wrong property.
        let retained = results.join("retained-alias-target");
        fs::rename(&original, &retained).unwrap();
        use std::os::unix::fs::MetadataExt;
        let identity = fs::metadata(&retained).unwrap();
        std::os::unix::fs::symlink(&retained, &original).unwrap();
        assert!(load(&guard, &sweep, &results).is_err(), "{leaf}");
        let after = fs::metadata(&retained).unwrap();
        assert_eq!(
            (identity.dev(), identity.ino(), identity.len()),
            (after.dev(), after.ino(), after.len())
        );
    }
}

#[test]
fn pending_cleanup_preserves_evidence_if_target_changes_during_sync() {
    for mutation in ["replace", "remove", "mutate", "replace-alias"] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let base = results.join(".checkpoint").join(ROOT);
        let target = base.join("admitted.json");
        let original = fs::read(&target).unwrap();
        let alias = base.join(".aaaa.pending");
        fs::hard_link(&target, &alias).unwrap();
        let mut catalog = load(&guard, &sweep, &results).unwrap();
        let outside = results.join("retained-before-sync");
        let mut injected = false;
        let result = catalog.confirm_reactivation(|path| {
            if path == alias && !injected {
                injected = true;
                match mutation {
                    "replace" => {
                        fs::rename(&target, &outside).unwrap();
                        fs::copy(&outside, &target).unwrap();
                    }
                    "remove" => fs::rename(&target, &outside).unwrap(),
                    "mutate" => fs::write(&target, b"changed during owned sync").unwrap(),
                    _ => {
                        fs::remove_file(&alias).unwrap();
                        fs::copy(&target, &alias).unwrap();
                    }
                }
            }
            Ok(())
        });
        assert!(injected);
        assert!(result.is_err(), "{mutation}");
        assert!(
            alias.exists(),
            "never unlink last retained original or replacement ({mutation})"
        );
        assert_eq!(
            fs::read(&alias).unwrap(),
            if mutation == "mutate" {
                b"changed during owned sync".to_vec()
            } else {
                original.clone()
            }
        );
        if outside.exists() {
            assert_eq!(fs::read(outside).unwrap(), original);
        }
    }
}

fn published_mixed() -> (Fixture, SweepPlan, PathBuf, Guard, Vec<AdmittedCampaign>) {
    let (fixture, sweep, results, prepared) = mixed();
    let (guard, workers) = prepared.publish(&results, &sweep).unwrap();
    (fixture, sweep, results, guard, workers)
}

/// Passive wire fixture only: publishing these synthetic completed bindings
/// does not demonstrate an actual execution or grant a worker capability.
fn append_metadata_node(results: &Path, ordinal: usize, complete: &[u64]) -> PathBuf {
    use crate::runner::campaign::generation::{Lineage, NextReference};
    let base = results.join(".checkpoint").join(ROOT);
    let previous = if ordinal == 1 {
        base.clone()
    } else {
        base.join("successors").join(format!("{:06}", ordinal - 1))
    };
    let previous_bytes = fs::read(previous.join("admitted.json")).unwrap();
    let previous_admission: Admission = serde_json::from_slice(&previous_bytes).unwrap();
    let generation = format!("ffff-0000-{ordinal:06}");
    let directory = base.join("successors").join(format!("{ordinal:06}"));
    fs::create_dir_all(&directory).unwrap();
    let mut manifests = Vec::new();
    for reference in &previous_admission.manifests {
        let mut manifest: Manifest = serde_json::from_slice(
            &fs::read(previous.join(manifest_path(reference.configuration))).unwrap(),
        )
        .unwrap();
        manifest.generation = generation.clone();
        if reference.configuration == 0 {
            for slot in complete {
                manifest.completed.push(Completed {
                    slot: *slot,
                    receipt: Blob::of(format!("synthetic receipt {slot}").as_bytes()),
                });
            }
            manifest.completed.sort_by_key(|entry| entry.slot);
            for batch in &mut manifest.pending {
                batch.cohort.admitted_slots.retain(|replica| {
                    !complete.contains(&((batch.batch - 1) * 4 + *replica as u64))
                });
                batch.cohort.dispatch = None;
                batch.cohort.dispatch = batch
                    .cohort
                    .admitted_slots
                    .first()
                    .map(|replica| batch.cohort.dispatch_for(*replica, 4).unwrap());
            }
            manifest
                .pending
                .retain(|batch| !batch.cohort.admitted_slots.is_empty());
        }
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(
            directory.join(manifest_path(reference.configuration)),
            &bytes,
        )
        .unwrap();
        manifests.push(ManifestReference {
            configuration: reference.configuration,
            content: Blob::of(&bytes),
        });
    }
    let admission = Admission {
        schema_version: 1,
        scope: SCOPE.into(),
        generation: generation.clone(),
        run_id: previous_admission.run_id,
        input_digest: previous_admission.input_digest,
        original_index: previous_admission.original_index,
        manifests,
        lineage: Some(Lineage {
            schema_version: 1,
            ordinal,
            previous_generation: previous_admission.generation,
            previous_admission: Blob::of(&previous_bytes),
        }),
    };
    let bytes = serde_json::to_vec_pretty(&admission).unwrap();
    fs::write(directory.join("admitted.json"), &bytes).unwrap();
    fs::write(
        previous.join("next.json"),
        serde_json::to_vec_pretty(&NextReference {
            schema_version: 1,
            ordinal,
            generation,
            admission: Blob::of(&bytes),
            previous_admission: Blob::of(&previous_bytes),
        })
        .unwrap(),
    )
    .unwrap();
    directory
}

#[test]
fn successive_metadata_nodes_preserve_original_start_and_receipt_bindings() {
    let (_fixture, sweep, results, guard, workers) = published_mixed();
    let first = load(&guard, &sweep, &results)
        .unwrap()
        .reference(0)
        .unwrap();
    let original = &workers[0];
    append_metadata_node(&results, 1, &[2]);
    let second = load(&guard, &sweep, &results)
        .unwrap()
        .reference(0)
        .unwrap();
    let second_manifest: Manifest = serde_json::from_slice(
        &fs::read(
            results
                .join(".checkpoint")
                .join(ROOT)
                .join("successors/000001/000000.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let inherited = load(&guard, &sweep, &results)
        .unwrap()
        .completed_receipt(0, 2)
        .unwrap()
        .clone();
    append_metadata_node(&results, 2, &[4]);
    let catalog = load(&guard, &sweep, &results).unwrap();
    assert_ne!(first, second);
    assert_ne!(second, catalog.reference(0).unwrap());
    assert_eq!(catalog.completed_receipt(0, 2), Some(&inherited));
    for (reference, manifest, id) in [
        (&first, &original.manifest, 2),
        (&second, &second_manifest, 4),
    ] {
        let slot = &original.plan.slots[id - 1];
        let batch = &manifest.pending[0];
        let resolved = catalog
            .resolve(
                Some(reference),
                0,
                &original.plan,
                slot,
                Some(&batch.cohort),
            )
            .unwrap();
        assert_eq!(
            resolved.prepared,
            batch.directory(&reference.generation).join("prepared.json")
        );
        assert!(
            catalog
                .resolve(None, 0, &original.plan, slot, Some(&batch.cohort))
                .is_err()
        );
        assert!(
            catalog
                .resolve(
                    Some(&catalog.reference(0).unwrap()),
                    0,
                    &original.plan,
                    slot,
                    Some(&batch.cohort)
                )
                .is_err()
        );
    }
    assert!(
        catalog
            .resolve(None, 0, &original.plan, &original.plan.slots[0], None)
            .unwrap()
            .evidence
            .is_empty()
    );
}

#[test]
fn successor_metadata_rejects_no_progress_and_unlinked_or_foreign_children() {
    for case in [
        "no-progress",
        "orphan",
        "foreign",
        "wrong-link",
        "changed-ancestor",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let child =
            append_metadata_node(&results, 1, if case == "no-progress" { &[] } else { &[2] });
        let base = results.join(".checkpoint").join(ROOT);
        match case {
            "orphan" => fs::remove_file(base.join("next.json")).unwrap(),
            "foreign" => fs::create_dir(base.join("successors/999999")).unwrap(),
            "wrong-link" => {
                let path = base.join("next.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                value["previous_admission"]["sha256"] = "0".repeat(64).into();
                fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "changed-ancestor" => rewrite_manifest(&results, 0, |value| {
                value["completed"][0]["receipt"]["sha256"] = "0".repeat(64).into();
            }),
            _ => (),
        }
        assert!(load(&guard, &sweep, &results).is_err(), "{case}");
        assert!(child.join("admitted.json").exists());
    }
}

fn rewrite_manifest(results: &Path, configuration: usize, mutate: impl FnOnce(&mut Value)) {
    let directory = results.join(".checkpoint").join(ROOT);
    let path = directory.join(manifest_path(configuration));
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    mutate(&mut value);
    let bytes = serde_json::to_vec_pretty(&value).unwrap();
    fs::write(path, &bytes).unwrap();
    let path = directory.join("admitted.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["manifests"][configuration]["content"] = serde_json::to_value(Blob::of(&bytes)).unwrap();
    fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

#[test]
fn exact_start_reference_resolves_only_pending_members_and_preserves_old_bindings() {
    let (_fixture, sweep, results, guard, workers) = published_mixed();
    let catalog = load(&guard, &sweep, &results).unwrap();
    let worker = &workers[0];
    let reference = catalog.reference(0).unwrap();
    let batch = &worker.manifest.pending[0];
    for id in [2, 4] {
        let slot = &worker.plan.slots[id - 1];
        let resolved = catalog
            .resolve(Some(&reference), 0, &worker.plan, slot, Some(&batch.cohort))
            .unwrap();
        assert_eq!(
            resolved.prepared,
            batch.directory(&reference.generation).join("prepared.json")
        );
        assert_eq!(
            resolved.evidence,
            [
                root(&guard.inputs).join("000000.json"),
                root(&guard.inputs).join("admitted.json")
            ]
        );
        assert!(
            catalog
                .resolve(None, 0, &worker.plan, slot, Some(&batch.cohort))
                .is_err()
        );
        assert!(
            catalog
                .resolve(Some(&reference), 0, &worker.plan, slot, None)
                .is_err()
        );
    }
    for id in [1, 3, 5, 6, 7, 8] {
        let slot = &worker.plan.slots[id - 1];
        let old = catalog.resolve(None, 0, &worker.plan, slot, None).unwrap();
        assert!(old.evidence.is_empty());
        assert_eq!(
            old.prepared,
            PathBuf::from(format!("batch-{}/prepared.json", slot.batch))
        );
        assert!(
            catalog
                .resolve(Some(&reference), 0, &worker.plan, slot, Some(&batch.cohort))
                .is_err()
        );
    }
    let slot = &worker.plan.slots[1];
    for field in ["generation", "configuration", "manifest", "admission"] {
        let mut wrong = reference.clone();
        match field {
            "generation" => wrong.generation.push('a'),
            "configuration" => wrong.configuration = 1,
            "manifest" => wrong.manifest.sha256 = "0".repeat(64),
            _ => wrong.admission.bytes += 1,
        }
        assert!(
            catalog
                .resolve(Some(&wrong), 0, &worker.plan, slot, Some(&batch.cohort))
                .is_err(),
            "{field}"
        );
    }
    let mut wrong = batch.cohort.clone();
    wrong.id.push('b');
    assert!(
        catalog
            .resolve(Some(&reference), 0, &worker.plan, slot, Some(&wrong))
            .is_err()
    );
    let mut wrong_slot: crate::runner::campaign::Slot =
        serde_json::from_value(serde_json::to_value(slot).unwrap()).unwrap();
    wrong_slot.id = 4;
    assert!(
        catalog
            .resolve(
                Some(&reference),
                0,
                &worker.plan,
                &wrong_slot,
                Some(&batch.cohort)
            )
            .is_err()
    );
}

#[test]
fn global_metadata_admits_no_partial_foreign_aliased_or_changed_index() {
    for case in [
        "missing-last",
        "extra",
        "pending",
        "alias",
        "index",
        "membership",
        "scope",
        "unknown",
        "digest",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let directory = root(&guard.inputs);
        match case {
            "missing-last" => fs::remove_file(directory.join("000001.json")).unwrap(),
            "extra" | "pending" => fs::write(
                directory.join(if case == "extra" {
                    "foreign.json"
                } else {
                    ".interrupted.pending"
                }),
                b"retained",
            )
            .unwrap(),
            "alias" => {
                fs::rename(directory.join("000001.json"), directory.join("retained")).unwrap();
                std::os::unix::fs::symlink("retained", directory.join("000001.json")).unwrap();
            }
            "index" => {
                let path = results.join("plan.json");
                let mut bytes = fs::read(&path).unwrap();
                bytes.push(b'\n');
                fs::write(path, bytes).unwrap();
            }
            _ => {
                let path = directory.join("admitted.json");
                let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                match case {
                    "membership" => value["manifests"][1]["configuration"] = json!(0),
                    "scope" => value["scope"] = json!("foreign"),
                    "unknown" => value["extra"] = json!(true),
                    _ => value["manifests"][1]["content"]["sha256"] = json!("0".repeat(64)),
                }
                fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
            }
        }
        assert!(load(&guard, &sweep, &results).is_err(), "{case}");
    }
}

#[test]
fn manifest_partition_and_frozen_cohort_validation_survive_recomputed_file_hashes() {
    for case in [
        "completed-overlap",
        "missing-slot",
        "duplicate",
        "unsorted",
        "dispatch",
        "unknown-dispatch",
        "wrong-c",
        "receipt",
        "batch",
        "original",
        "generation",
        "late-original",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        rewrite_manifest(
            &results,
            usize::from(case == "late-original"),
            |value| match case {
                "completed-overlap" => {
                    value["pending"][0]["cohort"]["admitted_slots"] = json!([1, 4])
                }
                "missing-slot" => {
                    value["completed"].as_array_mut().unwrap().pop();
                }
                "duplicate" => value["pending"][0]["cohort"]["admitted_slots"] = json!([2, 2]),
                "unsorted" => value["pending"][0]["cohort"]["admitted_slots"] = json!([4, 2]),
                "dispatch" => value["pending"][0]["cohort"]["dispatch"]["admitted"] = json!(4),
                "unknown-dispatch" => value["pending"][0]["cohort"]["dispatch"] = Value::Null,
                "wrong-c" => value["pending"][0]["cohort"]["selected_replicas"] = json!(2),
                "receipt" => value["completed"][0]["receipt"]["bytes"] = json!(0),
                "batch" => value["pending"][0]["batch"] = json!(3),
                "generation" => value["generation"] = json!("../escape"),
                _ => value["original"]["sha256"] = json!("0".repeat(64)),
            },
        );
        assert!(load(&guard, &sweep, &results).is_err(), "{case}");
    }
}

#[test]
fn start_resolution_selects_generation_material_without_changing_old_receipt_roots() {
    let (_fixture, sweep, results, guard, workers) = published_mixed();
    let catalog = load(&guard, &sweep, &results).unwrap();
    let layouts = sweep.read_campaigns(&results).unwrap();
    let layout = &layouts[0];
    let slot = &layout.plan.slots[1];
    let cohort = &workers[0].manifest.pending[0].cohort;
    let reference = catalog.reference(0).unwrap();
    let path = layout.root.join("batch-1/starts/2.json");
    let start = StartIntent {
        schema_version: 1,
        scope: "execution-start-intent-not-actual-attempt".into(),
        execution: layout.plan.execution(slot),
        execution_cohort: Some(cohort.clone()),
        generation: Some(reference),
    };
    fs::write(&path, serde_json::to_vec(&start).unwrap()).unwrap();
    let resolved = super::super::super::resolve_start(layout, slot, 0, Some(&catalog)).unwrap();
    assert!(resolved.binding.prepared.starts_with(".asb-generations"));
    assert_eq!(resolved.binding.evidence.len(), 2);
    let mut stripped = start;
    stripped.generation = None;
    fs::write(&path, serde_json::to_vec(&stripped).unwrap()).unwrap();
    assert!(super::super::super::resolve_start(layout, slot, 0, Some(&catalog)).is_err());
    let old = super::super::super::resolve_start(layout, &layout.plan.slots[0], 0, Some(&catalog))
        .unwrap();
    assert_eq!(old.binding.prepared, Path::new("batch-1/prepared.json"));
    assert!(old.binding.evidence.is_empty());
}

#[test]
fn historical_receipts_remain_byte_frozen_after_admission_and_are_not_republished() {
    for rewrite in [false, true] {
        let (fixture, sweep, results, guard, _workers) = published_mixed();
        let path = guard.inputs.campaign_completion_path(1, 8);
        let original = fs::read(&path).unwrap();
        if rewrite {
            let mut equivalent = original.clone();
            equivalent.push(b'\n');
            assert_eq!(
                serde_json::from_slice::<Value>(&original).unwrap(),
                serde_json::from_slice::<Value>(&equivalent).unwrap()
            );
            fs::write(&path, &equivalent).unwrap();
            let error = guard
                .inputs
                .seal_campaigns(&fixture.0, &results, &sweep)
                .unwrap_err();
            assert!(
                error.to_string().contains("receipt bytes differ"),
                "{error}"
            );
            assert_eq!(fs::read(&path).unwrap(), equivalent);
        } else {
            // Pending members have no results yet; only previously completed
            // peers are verified. Do not manufacture receipts for pending slots.
            guard
                .inputs
                .seal_campaigns(&fixture.0, &results, &sweep)
                .unwrap();
            assert_eq!(fs::read(path).unwrap(), original);
            for id in [2, 4] {
                assert!(!guard.inputs.campaign_completion_path(0, id).exists());
            }
        }
    }
}

#[test]
fn resolved_generation_prepared_records_are_bounded_before_receipt_publication() {
    let (fixture, sweep, results, guard, workers) = published_mixed();
    let catalog = load(&guard, &sweep, &results).unwrap();
    let layout = sweep.read_campaigns(&results).unwrap().remove(0);
    let slot = &layout.plan.slots[1];
    let cohort = &workers[0].manifest.pending[0].cohort;
    let reference = catalog.reference(0).unwrap();
    let start = StartIntent {
        schema_version: 1,
        scope: "execution-start-intent-not-actual-attempt".into(),
        execution: layout.plan.execution(slot),
        execution_cohort: Some(cohort.clone()),
        generation: Some(reference.clone()),
    };
    fs::write(
        layout.root.join("batch-1/starts/2.json"),
        serde_json::to_vec(&start).unwrap(),
    )
    .unwrap();
    let bound = catalog
        .resolve(Some(&reference), 0, &layout.plan, slot, Some(cohort))
        .unwrap();
    let path = layout.root.join(bound.prepared);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(&path)
        .unwrap()
        .set_len(64 * 1024 + 1)
        .unwrap();
    let original = fs::read(guard.inputs.campaign_completion_path(0, 1)).unwrap();
    let error = guard
        .inputs
        .seal_campaigns(&fixture.0, &results, &sweep)
        .unwrap_err();
    assert!(
        error.to_string().contains("bounded regular file"),
        "{error}"
    );
    assert_eq!(
        fs::read(guard.inputs.campaign_completion_path(0, 1)).unwrap(),
        original
    );
    assert!(!guard.inputs.campaign_completion_path(0, 2).exists());
}

fn settle_generation_replicas(
    fixture: &Fixture,
    sweep: &SweepPlan,
    results: &Path,
    guard: &Guard,
    workers: &[AdmittedCampaign],
    replicas: &[usize],
) {
    let catalog = load(guard, sweep, results).unwrap();
    let layouts = sweep.read_campaigns(results).unwrap();
    let layout = &layouts[0];
    let reference = catalog.reference(0).unwrap();
    let cohort = &workers[0].manifest.pending[0].cohort;
    let old: Vec<_> = workers
        .iter()
        .flat_map(|worker| {
            worker.manifest.completed.iter().map(|receipt| {
                let path = guard
                    .inputs
                    .campaign_completion_path(worker.manifest.configuration, receipt.slot);
                let bytes = fs::read(&path).unwrap();
                (path, bytes)
            })
        })
        .collect();
    let source: Value = serde_json::from_slice(
        &fs::read(layout.root.join("batch-1/.asb-completions/fixture-1.json")).unwrap(),
    )
    .unwrap();
    for &replica in replicas {
        let slot = &layout.plan.slots[replica - 1];
        let task = crate::workloads::TaskIdentity::local("bug-fix", None);
        let workspace = layout.root.join(&slot.workspace);
        let log = layout.root.join(&slot.log);
        fs::create_dir_all(&workspace).unwrap();
        fs::write(&log, "new generation fixture log").unwrap();
        let start = StartIntent {
            schema_version: 1,
            scope: "execution-start-intent-not-actual-attempt".into(),
            execution: layout.plan.execution(slot),
            execution_cohort: Some(cohort.clone()),
            generation: Some(reference.clone()),
        };
        fs::write(
            layout.root.join(format!("batch-1/starts/{replica}.json")),
            serde_json::to_vec(&start).unwrap(),
        )
        .unwrap();
        let binding = catalog
            .resolve(
                start.generation.as_ref(),
                0,
                &layout.plan,
                slot,
                Some(cohort),
            )
            .unwrap();
        let prepared = layout.root.join(binding.prepared);
        if !prepared.exists() {
            fs::create_dir_all(prepared.parent().unwrap()).unwrap();
            fs::copy(layout.root.join("batch-1/prepared.json"), &prepared).unwrap();
        }
        // This is synthetic current-protocol outcome evidence, not an agent
        // execution. Use the real fingerprint constructor instead of disabling
        // the dispatch/hash invariant or guessing a historical fingerprint.
        let mut config = layout.plan.configuration.clone();
        config
            .planning
            .get_or_insert_with(Default::default)
            .repetition = crate::planning::Repetition { index: 1, total: 2 };
        let policy = crate::planning::resolve(&config).unwrap();
        let effective = crate::planning::EffectiveBudgets {
            execution_seconds: policy
                .budgets
                .execution_seconds
                .or(policy.default_budgets.execution_seconds)
                .map(|value| value as f64),
            environment_preparation_seconds: policy
                .budgets
                .environment_preparation_seconds
                .or(policy.default_budgets.environment_preparation_seconds),
            evaluation_seconds: policy
                .budgets
                .evaluation_seconds
                .or(policy.default_budgets.evaluation_seconds),
            ..Default::default()
        };
        let fingerprint = crate::planning::protocol_fingerprint_with_dispatch(
            &config,
            &policy,
            effective,
            4,
            Some(layout.plan.admission),
            cohort.dispatch,
        )
        .unwrap();
        let mut outcome = source.clone();
        outcome["replica"] = json!({"index":replica,"total":4});
        outcome["outputs"] = json!([workspace, log]);
        let summary = &mut outcome["summary"];
        summary["task"] = json!(task);
        summary["execution"] = json!(layout.plan.execution(slot));
        summary["execution_cohort"] = json!(cohort);
        summary["replica_index"] = json!(replica);
        summary["planning"] = json!(policy);
        summary["effective_budgets"] = json!(effective);
        summary["execution_plan_fingerprint"] = json!(fingerprint.as_str());
        let bytes = serde_json::to_vec(&outcome).unwrap();
        let path = PathBuf::from(format!("batch-1/.asb-completions/new-{replica}.json"));
        fs::write(layout.root.join(&path), &bytes).unwrap();
        fs::write(layout.root.join(format!("batch-1/outcomes/{replica}.json")), serde_json::to_vec(&json!({
            "schema_version":1,"execution":layout.plan.execution(slot),"path":path,"bytes":bytes.len(),
            "sha256":crate::cassette::digest(&bytes),"task":task
        })).unwrap()).unwrap();
    }
    guard
        .inputs
        .seal_campaigns(&fixture.0, results, sweep)
        .unwrap();
    for (path, bytes) in old {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn generation_slots_seal_beside_historical_peers_without_replacing_their_evidence() {
    let (fixture, sweep, results, guard, workers) = published_mixed();
    settle_generation_replicas(&fixture, &sweep, &results, &guard, &workers, &[2, 4]);
    for id in [2, 4] {
        let value: Value = serde_json::from_slice(
            &fs::read(guard.inputs.campaign_completion_path(0, id)).unwrap(),
        )
        .unwrap();
        let roots = value["roots"].as_array().unwrap();
        assert!(roots.iter().any(|root| root["name"] == "generation-0"));
        assert!(roots.iter().any(|root| root["name"] == "generation-1"));
        assert!(roots.iter().any(|root| root["name"] == "prepared"
            && root["path"].as_str().unwrap().contains(".asb-generations")));
    }
    let Guard { inputs, owners } = guard;
    drop(owners);
    let reused = inputs
        .resume_campaign_outcomes(&fixture.0, &results, &sweep)
        .unwrap();
    assert_eq!(reused.groups.len(), 2);
    assert!(reused.groups.values().all(|rows| rows.len() == 8));
}

fn successor_inputs() -> (Fixture, SweepPlan, PathBuf, Inputs) {
    let (fixture, sweep, results, guard, workers) = published_mixed();
    settle_generation_replicas(&fixture, &sweep, &results, &guard, &workers, &[2]);
    drop(workers);
    let Guard { inputs, owners } = guard;
    drop(owners);
    (fixture, sweep, results, inputs)
}

fn staged_unlinked_child() -> (Fixture, SweepPlan, PathBuf, Inputs, PathBuf, Vec<u8>) {
    let (fixture, sweep, results, inputs) = successor_inputs();
    // Preserve the fixture's EXACT execution closure across the injected failed
    // publisher. Metadata repair itself uses the real separate recovery inlet.
    let retained = Inputs {
        directory: inputs.directory.clone(),
        snapshotter: crate::sweep::checkpoint::Snapshotter::open(
            &inputs.directory,
            "python3".into(),
        )
        .unwrap(),
        roots: inputs.roots.clone(),
        context: inputs.context.clone(),
        initial: inputs.initial.clone(),
        runtime: None,
    };
    let classified = inputs
        .classify_campaigns(&fixture.0, &results, &sweep)
        .unwrap();
    let prepared = Prepared::plan(inputs, classified, &fixture.0, &results, &sweep).unwrap();
    let child = prepared.guard.inputs.directory.join(&prepared.relative);
    let frozen = prepared.admission_bytes.clone();
    let failed = prepared.publish_with_confirmation(
        &results,
        &sweep,
        |_| Ok(()),
        |path| {
            if path == child {
                Err(Error::Io(
                    "injected fully staged prelink interruption".into(),
                ))
            } else {
                Ok(())
            }
        },
    );
    assert!(failed.is_err());
    let base = results.join(".checkpoint").join(ROOT);
    assert!(!base.join("next.json").exists());
    assert_eq!(fs::read(child.join("admitted.json")).unwrap(), frozen);
    (fixture, sweep, results, retained, child, frozen)
}

#[test]
fn fully_staged_unlinked_child_repairs_without_executions_and_reuses_exact_frozen_membership() {
    let (fixture, sweep, results, inputs, child, frozen) = staged_unlinked_child();
    assert!(
        inputs
            .classify_campaigns(&fixture.0, &results, &sweep)
            .is_err()
    );
    let untouched = sweep
        .read_campaigns(&results)
        .unwrap()
        .remove(0)
        .root
        .join("batch-1/starts/4.json");
    assert!(!untouched.exists());
    for linked in [true, false] {
        let report = Inputs::recover_continuation(&fixture.0, &results, &sweep).unwrap();
        assert_eq!(report["linked"], linked);
        assert_eq!(report["agent_executions"], 0);
        assert_eq!(report["evaluations"], 0);
        assert_eq!(fs::read(child.join("admitted.json")).unwrap(), frozen);
        assert!(!untouched.exists());
    }
    let classified = inputs
        .classify_campaigns(&fixture.0, &results, &sweep)
        .unwrap();
    let prepared = Prepared::plan(inputs, classified, &fixture.0, &results, &sweep).unwrap();
    assert!(prepared.reactivating);
    let (_guard, workers) = prepared.publish(&results, &sweep).unwrap();
    assert_eq!(workers[0].manifest.pending[0].cohort.admitted_slots, [4]);
    assert!(workers[1].manifest.pending.is_empty());
    assert_eq!(workers[0].admission_reference, Blob::of(&frozen));
    assert!(!untouched.exists());
}

#[test]
fn unlinked_child_recovery_keeps_candidates_out_of_authoritative_resolution() {
    let (_fixture, sweep, results, guard, workers) = published_mixed();
    let original = load(&guard, &sweep, &results)
        .unwrap()
        .reference(0)
        .unwrap();
    let child = append_metadata_node(&results, 1, &[2]);
    fs::remove_file(root(&guard.inputs).join("next.json")).unwrap();
    assert!(load(&guard, &sweep, &results).is_err());
    let layouts = sweep.read_campaigns(&results).unwrap();
    let inspection = guard
        .inputs
        .read_generation_with(&results, &sweep, &layouts, Catalog::read_for_recovery)
        .unwrap()
        .unwrap();
    assert_eq!(inspection.authoritative.reference(0).unwrap(), original);
    let candidate = inspection.candidate.unwrap();
    let snapshot = candidate.snapshot().unwrap();
    assert_eq!(
        snapshot.bytes,
        fs::read(child.join("admitted.json")).unwrap()
    );
    let forged_reference = crate::runner::campaign::generation::StartReference {
        generation: snapshot.admission.generation,
        configuration: 0,
        manifest: Blob::of(&snapshot.manifests[0].1),
        admission: Blob::of(&snapshot.bytes),
    };
    assert!(
        inspection
            .authoritative
            .resolve(
                Some(&forged_reference),
                0,
                &layouts[0].plan,
                &layouts[0].plan.slots[3],
                Some(&workers[0].manifest.pending[0].cohort),
            )
            .is_err()
    );
    assert!(!root(&guard.inputs).join("next.json").exists());
}

#[test]
fn unlinked_child_metadata_matrix_refuses_partial_foreign_or_rewritten_history() {
    for case in [
        "admission",
        "manifest",
        "foreign",
        "next",
        "lineage",
        "no-progress",
        "alias-copy",
    ] {
        let (_fixture, sweep, results, guard, _workers) = published_mixed();
        let child =
            append_metadata_node(&results, 1, if case == "no-progress" { &[] } else { &[2] });
        let base = root(&guard.inputs);
        if case == "lineage" {
            rewrite_child(&results, |admission, _| {
                admission.lineage.as_mut().unwrap().ordinal += 1
            });
        }
        fs::remove_file(base.join("next.json")).unwrap();
        match case {
            "admission" => fs::rename(
                child.join("admitted.json"),
                results.join("retained-admission"),
            )
            .unwrap(),
            "manifest" => {
                fs::rename(child.join("000001.json"), results.join("retained-manifest")).unwrap()
            }
            "foreign" => fs::create_dir(base.join("successors/999999")).unwrap(),
            "next" => fs::write(child.join("next.json"), b"{}").unwrap(),
            "alias-copy" => {
                fs::copy(child.join("admitted.json"), child.join(".aaaa.pending")).unwrap();
            }
            _ => (),
        }
        let layouts = sweep.read_campaigns(&results).unwrap();
        assert!(
            guard
                .inputs
                .read_generation_with(&results, &sweep, &layouts, Catalog::read_for_recovery)
                .is_err(),
            "{case}"
        );
        assert!(child.exists(), "{case}");
        assert!(!base.join("next.json").exists(), "{case}");
    }
}

#[test]
fn unlinked_child_owned_repair_refuses_late_configuration_or_receipt_conflicts_before_link() {
    for case in [
        "runtime",
        "missing-receipt",
        "changed-receipt",
        "pending-workspace",
        "forged-partition",
    ] {
        let (fixture, sweep, results, _inputs, child, frozen) = staged_unlinked_child();
        let base = results.join(".checkpoint").join(ROOT);
        let layouts = sweep.read_campaigns(&results).unwrap();
        let admission: Admission = serde_json::from_slice(&frozen).unwrap();
        let retained = match case {
            "runtime" => {
                let path = layouts[1]
                    .root
                    .join(".asb-generations")
                    .join(&admission.generation)
                    .join("finished.json");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"late configuration artifact").unwrap();
                path
            }
            "pending-workspace" => {
                let path = layouts[0].root.join(&layouts[0].plan.slots[3].workspace);
                fs::create_dir_all(&path).unwrap();
                path
            }
            "missing-receipt" => {
                let path = results.join("retained-original-receipt");
                fs::rename(
                    results.join(".checkpoint/campaign-completed/000001/000008.json"),
                    &path,
                )
                .unwrap();
                path
            }
            "changed-receipt" => {
                let path = results.join(".checkpoint/campaign-completed/000001/000008.json");
                fs::write(&path, b"changed receipt").unwrap();
                path
            }
            _ => {
                // Recompute the manifest/admission hash, but leave the claimed
                // receipt absent: self-consistent candidate metadata is not fact.
                let path = child.join("000000.json");
                let mut manifest: Manifest =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                manifest.completed.push(Completed {
                    slot: 4,
                    receipt: Blob::of(b"invented completion"),
                });
                manifest.completed.sort_by_key(|entry| entry.slot);
                manifest.pending.clear();
                let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
                fs::write(&path, &bytes).unwrap();
                let mut admission = admission;
                admission.manifests[0].content = Blob::of(&bytes);
                // Remove only the fixture's known duplicate before rewriting
                // its candidate, so the refusal tests population, not an alias.
                for name in fs::read_dir(&child)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                {
                    if name
                        .extension()
                        .is_some_and(|extension| extension == "pending")
                    {
                        fs::remove_file(name).unwrap();
                    }
                }
                fs::write(
                    child.join("admitted.json"),
                    serde_json::to_vec_pretty(&admission).unwrap(),
                )
                .unwrap();
                path
            }
        };
        assert!(
            Inputs::recover_continuation(&fixture.0, &results, &sweep).is_err(),
            "{case}"
        );
        assert!(!base.join("next.json").exists(), "{case}");
        assert!(retained.exists(), "{case}");
        assert!(
            !layouts[0].root.join("batch-1/starts/4.json").exists(),
            "{case}"
        );
    }
}

#[test]
fn unlinked_child_repair_failed_link_sync_is_confirmed_idempotently_without_execution() {
    let (fixture, sweep, results, _inputs, child, frozen) = staged_unlinked_child();
    let mut metadata = Inputs::recovery_inputs(&fixture.0, &results, &sweep).unwrap();
    let error = metadata
        .recover_continuation_link_after_link(&fixture.0, &results, &sweep, |_| {
            Err(Error::Io(
                "injected recovered link directory sync failure".into(),
            ))
        })
        .unwrap_err();
    assert!(error.to_string().contains("injected recovered link"));
    let link = results.join(".checkpoint").join(ROOT).join("next.json");
    let link_bytes = fs::read(&link).unwrap();
    for _ in 0..2 {
        let report = Inputs::recover_continuation(&fixture.0, &results, &sweep).unwrap();
        assert_eq!(report["linked"], false);
        assert_eq!(report["agent_executions"], 0);
        assert_eq!(fs::read(&link).unwrap(), link_bytes);
        assert_eq!(fs::read(child.join("admitted.json")).unwrap(), frozen);
    }
}

#[test]
fn unlinked_child_invalid_population_refuses_before_sources_or_receipts() {
    use crate::cassette::{
        CompatibilityShape, ImportedTrack, Limits, OriginPlan, RecordingReservation,
        RecordingSource,
    };
    let (fixture, sweep, results) =
        crate::sweep::checkpoint::campaigns::tests::published_concurrency(
            crate::tape::Mode::Record,
            4,
        );
    let state = super::inputs(&results, &sweep);
    state.publish("pending", None).unwrap();
    state.reserve_campaigns(&results, &sweep).unwrap();
    let classified = state
        .classify_campaigns(&fixture.0, &results, &sweep)
        .unwrap();
    let prepared = Prepared::plan(state, classified, &fixture.0, &results, &sweep).unwrap();
    let (guard, workers) = prepared.publish(&results, &sweep).unwrap();
    drop(workers);
    let base = root(&guard.inputs);
    let original = fs::read(base.join("admitted.json")).unwrap();
    let parent: Admission = serde_json::from_slice(&original).unwrap();
    let generation = "ffff-aaaa";
    let child = base.join("successors/000001");
    fs::create_dir_all(&child).unwrap();
    let layouts = sweep.read_campaigns(&results).unwrap();
    let mut references = Vec::new();
    for (configuration, layout) in layouts.iter().enumerate() {
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(base.join(manifest_path(configuration))).unwrap())
                .unwrap();
        manifest.generation = generation.into();
        manifest.completed = (1..=5)
            .map(|slot| Completed {
                slot,
                receipt: Blob::of(b"deliberately absent receipt"),
            })
            .collect();
        for batch in &mut manifest.pending {
            batch
                .cohort
                .admitted_slots
                .retain(|replica| (batch.batch - 1) * 4 + *replica as u64 > 5);
            batch.cohort.dispatch = None;
            batch.cohort.dispatch = batch
                .cohort
                .admitted_slots
                .first()
                .map(|replica| batch.cohort.dispatch_for(*replica, 4).unwrap());
        }
        manifest
            .pending
            .retain(|batch| !batch.cohort.admitted_slots.is_empty());
        let descriptor = manifest.recording.as_mut().unwrap();
        descriptor.physical_id = format!("{generation}-{configuration:x}");
        descriptor.destination = layout
            .plan
            .recording
            .as_ref()
            .unwrap()
            .parent()
            .unwrap()
            .join(format!("continuation-{}.asb.zip", descriptor.physical_id));
        descriptor.sources = vec![fixture.0.join(format!("absent-source-{configuration}.zip"))];
        descriptor.origins = OriginPlan {
            schema_version: 1,
            population: 8,
            sources: vec![RecordingSource {
                recording_id: "aaaa".into(),
                manifest_sha256: "a".repeat(64),
            }],
            imports: (1..=5)
                .map(|slot| ImportedTrack {
                    slot,
                    source: 0,
                    metadata_sha256: "b".repeat(64),
                    bytes: Limits::default().max_track_bytes,
                })
                .collect(),
        };
        let selection = layout.plan.recording_selection.as_ref().unwrap();
        descriptor.reservation = RecordingReservation::for_origin_plan(
            CompatibilityShape {
                agent: &layout.plan.configuration.agent,
                provider: &layout.plan.configuration.provider,
                model: &layout.plan.configuration.model,
                workload: &selection.workload,
                task: &selection.task_id,
            },
            &descriptor.physical_id,
            &descriptor.destination,
            4,
            &descriptor.origins,
            Limits::default(),
        )
        .unwrap();
        descriptor
            .validate(&layout.plan, generation, configuration, 1..=5)
            .unwrap();
        // Each declaration is internally admissible, but the staged child
        // does not match the classified original population.  Recovery must
        // reject that before touching any absent source or old receipt.
        crate::runner::recording::union::admit_population([&*descriptor]).unwrap();
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        fs::write(child.join(manifest_path(configuration)), &bytes).unwrap();
        references.push(ManifestReference {
            configuration,
            content: Blob::of(&bytes),
        });
    }
    let admission = Admission {
        schema_version: 1,
        scope: SCOPE.into(),
        generation: generation.into(),
        run_id: parent.run_id,
        input_digest: parent.input_digest,
        original_index: parent.original_index,
        manifests: references,
        lineage: Some(crate::runner::campaign::generation::Lineage {
            schema_version: 1,
            ordinal: 1,
            previous_generation: parent.generation,
            previous_admission: Blob::of(&original),
        }),
    };
    let bytes = serde_json::to_vec_pretty(&admission).unwrap();
    fs::write(child.join("admitted.json"), &bytes).unwrap();
    drop(guard);
    let error = Inputs::recover_continuation(&fixture.0, &results, &sweep).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("staged continuation differs from classified original population"),
        "{error}"
    );
    assert!(!base.join("next.json").exists());
    assert_eq!(fs::read(base.join("admitted.json")).unwrap(), original);
    assert_eq!(fs::read(child.join("admitted.json")).unwrap(), bytes);
    for configuration in 0..2 {
        assert!(
            !fixture
                .0
                .join(format!("absent-source-{configuration}.zip"))
                .exists()
        );
        assert!(
            !results
                .join(format!(
                    ".checkpoint/campaign-completed/{configuration:06}/000001.json"
                ))
                .exists()
        );
    }
}
