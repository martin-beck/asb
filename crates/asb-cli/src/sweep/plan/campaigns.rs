//! Shared index-to-campaign binding for reporting and continuation inputs.
use super::*;
use crate::runner::campaign::reading::Layout;

impl SweepPlan {
    /// Validate every immutable campaign reference before reading observations.
    pub(crate) fn read_campaigns(&self, results: &Path) -> Result<Vec<Layout>, Error> {
        self.validate_campaign_references()?;
        let intent = self.execution.as_ref().ok_or_else(|| {
            Error::Input("campaign input declaration requires a modern sweep".into())
        })?;
        // First bind every retained plan to its index entry. Do not read an outcome
        // or recording payload from an earlier configuration before this succeeds.
        let layouts = self
            .entries
            .iter()
            .map(|entry| {
                let layout = Layout::read_reference(
                    results,
                    entry
                        .campaign
                        .as_ref()
                        .expect("validated campaign reference"),
                )?;
                if layout.plan.configuration != entry.configuration
                    || layout
                        .plan
                        .recording_selection
                        .as_ref()
                        .is_some_and(|selection| selection != &entry.selection)
                    || layout.plan.repeat != intent.repeat
                    || layout.plan.mode != self.execution_mode
                    || layout.plan.respect_record_timings != self.respect_record_timings
                    || layout.plan.ignore_evaluation != self.ignore_evaluation
                    || layout.plan.ignore_agent != self.ignore_agent
                    || layout.plan.ignore_collector != self.ignore_collector
                    || layout.plan.ignore_provider_mismatch != self.ignore_provider_mismatch
                    || layout.plan.ignore_provider_unconsumed != self.ignore_provider_unconsumed
                    || layout.plan.ignore_tool_mismatch != self.ignore_tool_mismatch
                    || !intent.matches_replay(&layout.plan)
                {
                    return Err(Error::Input(
                        "campaign plan differs from its saved sweep configuration or execution intent"
                            .into(),
                    ));
                }
                Ok(layout)
            })
            .collect::<Result<Vec<_>, Error>>()?;
        if intent.seed.is_some()
            && layouts
                .iter()
                .all(|layout| layout.plan.replay_selection.is_none())
        {
            return Err(Error::Input(
                "sweep seed has no bundled replay configuration".into(),
            ));
        }
        if let Some(expected) = &intent.recording_reservation {
            let mut actual = crate::cassette::RecordingReservation::default();
            for layout in &layouts {
                let capacity = layout.plan.recording_reservation.as_ref().ok_or_else(|| {
                    Error::Input(
                        "sweep recording capacity has no matching campaign reservation".into(),
                    )
                })?;
                actual = actual
                    .checked_add(capacity)
                    .map_err(|error| Error::Input(error.to_string()))?;
            }
            if &actual != expected {
                return Err(Error::Input(
                    "sweep recording capacity differs from its retained campaign plans".into(),
                ));
            }
        }
        Ok(layouts)
    }
}
