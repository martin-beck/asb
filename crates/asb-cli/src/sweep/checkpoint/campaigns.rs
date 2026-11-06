//! Immutable campaign inputs only; not attempt settlement or resume authority.
use super::Root;
use crate::{Error, runner::repeated::InputCost, sweep::plan::SweepPlan};
use std::path::Path;

mod reservations;
mod settled;
pub(in crate::sweep) use settled::continuation::{
    AdmittedCampaign, CampaignContinuation, Guard, PreparedContinuation,
};

pub(super) fn input_roots(results: &Path, plan: &SweepPlan) -> Result<Vec<Root>, Error> {
    // Share the exact index/configuration/selection/capacity binding used by
    // comparison. No outcome, growing journal or recording payload is read.
    let layouts = plan.read_campaigns(results)?;
    admit_cost(&layouts)?;
    let mut roots = Vec::new();
    for (index, layout) in layouts.iter().enumerate() {
        roots.push(Root::path(
            format!("campaign-{index}-plan"),
            layout.root.join("plan.json"),
        ));
        roots.push(Root::path(
            format!("campaign-{index}-configuration"),
            layout.plan.source.clone(),
        ));
        if layout.plan.normalization_policy.is_some() {
            roots.push(Root::path(
                format!("campaign-{index}-normalization"),
                layout.root.join(".normalization"),
            ));
        }
        for (input_index, input) in layout.plan.replay_inputs.iter().enumerate() {
            roots.push(Root::path(
                format!("campaign-{index}-replay-{input_index}"),
                layout.root.join(&input.private),
            ));
        }
    }
    Ok(roots)
}

fn admit_cost(layouts: &[crate::runner::campaign::reading::Layout]) -> Result<(), Error> {
    let mut cost = InputCost::default();
    for layout in layouts {
        cost = cost.checked_add(InputCost {
            inputs: layout.plan.replay_inputs.len(),
            bytes: layout
                .plan
                .replay_inputs
                .iter()
                .map(|input| input.bytes)
                .sum(),
            ..Default::default()
        })?;
    }
    Ok(())
}

/// Original private payload declarations must agree before a modern input seal
/// can authorize further continuation checks. No directory or receipt is made.
pub(super) fn verify_private_inputs(results: &Path, plan: &SweepPlan) -> Result<(), Error> {
    let layouts = plan.read_campaigns(results)?;
    admit_cost(&layouts)?;
    for layout in &layouts {
        layout.inspect_private_replay()?;
        verify_normalization(&layout.root, layout.plan.normalization_policy.as_ref())?;
    }
    for layout in &layouts {
        layout.verify_private_replay()?;
    }
    Ok(())
}

/// Completed-only resume can bypass per-campaign activation. Compare each
/// currently selected project policy with the frozen plan before reusing any
/// completed slot, including the transition from no policy to a new one.
pub(super) fn verify_current_normalization(
    parent: &Path,
    results: &Path,
    plan: &SweepPlan,
    invocation_dir: &Path,
) -> Result<(), Error> {
    let layouts = plan.read_campaigns(results)?;
    if layouts.len() != plan.entries.len() {
        return Err(Error::Input(
            "sweep normalization campaign population differs".into(),
        ));
    }
    for (entry, layout) in plan.entries.iter().zip(&layouts) {
        if layout.plan.mode == crate::planning::ExecutionMode::Live
            || (layout.plan.mode == crate::planning::ExecutionMode::Record
                && layout.plan.normalization_policy.is_none())
        {
            continue;
        }
        let (root, config) = crate::project::load_for_invocation(
            &parent.join(&entry.reference),
            Some(invocation_dir),
        )?;
        if config.replay.is_empty() && layout.plan.normalization_policy.is_none() {
            continue;
        }
        let current = config
            .replay
            .freeze_for_task(&root, &entry.configuration.agent, &entry.selection)
            .map_err(Error::Input)?
            .descriptor()
            .map_err(Error::Input)?;
        if current != layout.plan.normalization_policy {
            return Err(Error::Input("configured replay normalizer policy changed since the sweep was planned; start a new recording or restore the original policy".into()));
        }
    }
    Ok(())
}

fn verify_normalization(
    root: &Path,
    descriptor: Option<&crate::project::replay_normalization::PolicyDescriptor>,
) -> Result<(), Error> {
    if let Some(descriptor) = descriptor {
        crate::project::replay_normalization::FrozenPolicy::restore(
            &root.join(".normalization"),
            descriptor,
        )
        .map_err(|error| Error::Input(format!("cannot verify saved normalizer policy: {error}")))?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
pub(in crate::sweep::checkpoint) mod tests;
