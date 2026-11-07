//! Dispatch provenance, not a claim that admitted replicas overlapped in time.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCohort {
    pub id: String,
    pub selected_replicas: usize,
    pub admitted_slots: Vec<usize>,
    /// Absent in historical records; never inferred for cached observations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) dispatch: Option<crate::planning::Dispatch>,
}

impl ExecutionCohort {
    pub(crate) fn admits(&self, index: usize, total: usize) -> bool {
        self.selected_replicas == total
            && (1..=crate::benchmark::MAX_CONCURRENCY).contains(&total)
            && !self.id.is_empty()
            && self.id.len() <= 128
            && self
                .id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
            && !self.admitted_slots.is_empty()
            && self.admitted_slots.len() <= total
            && self
                .admitted_slots
                .iter()
                .all(|slot| (1..=total).contains(slot))
            && self.admitted_slots.windows(2).all(|pair| pair[0] < pair[1])
            && self.admitted_slots.contains(&index)
            && self.dispatch.is_none_or(|dispatch| {
                dispatch.configured() == total && dispatch.admitted() == self.admitted_slots.len()
            })
    }

    /// Derive only from an actual validated admission; the per-replica config
    /// may have concurrency one even when the original selected total is four.
    pub(crate) fn dispatch_for(
        &self,
        index: usize,
        total: usize,
    ) -> Result<crate::planning::Dispatch, crate::Error> {
        if !self.admits(index, total) {
            return Err(crate::Error::Input(
                "dispatch differs from actual cohort membership".into(),
            ));
        }
        crate::planning::Dispatch::new(total, self.admitted_slots.len())
            .map_err(|error| crate::Error::Input(error.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::ExecutionCohort;

    #[test]
    fn declared_dispatch_must_match_actual_original_membership_and_stays_optional() {
        let mut cohort = ExecutionCohort {
            id: "abc".into(),
            selected_replicas: 4,
            admitted_slots: vec![2, 4],
            dispatch: None,
        };
        let before = serde_json::to_value(&cohort).unwrap();
        assert!(before.get("dispatch").is_none());
        assert_eq!(
            serde_json::from_value::<ExecutionCohort>(before).unwrap(),
            cohort
        );
        let dispatch = cohort.dispatch_for(2, 4).unwrap();
        assert_eq!((dispatch.configured(), dispatch.admitted()), (4, 2));
        assert!(cohort.dispatch_for(1, 4).is_err());
        assert!(cohort.dispatch_for(2, 2).is_err());
        cohort.dispatch = Some(dispatch);
        assert!(cohort.admits(4, 4));
        for (configured, admitted) in [(4, 4), (2, 2), (4, 1)] {
            cohort.dispatch = Some(crate::planning::Dispatch::new(configured, admitted).unwrap());
            assert!(!cohort.admits(2, 4));
            assert!(cohort.dispatch_for(2, 4).is_err());
        }
    }

    #[test]
    fn dispatch_membership_is_bounded_ordered_and_explicit() {
        let mut cohort = ExecutionCohort {
            id: "a-123".into(),
            selected_replicas: 3,
            admitted_slots: vec![2],
            dispatch: None,
        };
        assert!(cohort.admits(2, 3));
        assert!(!cohort.admits(1, 3));
        assert!(!cohort.admits(2, 2));
        for slots in [vec![], vec![0], vec![4], vec![2, 2], vec![3, 2]] {
            cohort.admitted_slots = slots;
            assert!(!cohort.admits(2, 3));
        }
        cohort.admitted_slots = vec![1, 2, 3];
        assert!(cohort.admits(1, 3));
        cohort.id = "../invalid".into();
        assert!(!cohort.admits(1, 3));
    }
}
