use super::{AnalysisRule, Assessment, Finding, Observation};

struct PositiveTime {
    id: &'static str,
    metric: &'static str,
    summary: &'static str,
    limitations: &'static str,
}

impl AnalysisRule for PositiveTime {
    fn evaluate(&self, observations: &[Observation]) -> Finding {
        let observed = observations.iter().find(|item| item.id == self.metric);
        let value = observed.and_then(|item| item.value);
        Finding {
            rule: self.id,
            rule_version: 1,
            assessment: match value {
                Some(v) if v > 0.0 => Assessment::Observed,
                Some(_) => Assessment::NotObserved,
                None => Assessment::InsufficientEvidence,
            },
            summary: self.summary,
            evidence: value.map(|_| vec![self.metric]).unwrap_or_default(),
            limitations: self.limitations,
        }
    }
}

static SCHEDULER: PositiveTime = PositiveTime {
    id: "kernel.scheduler_wait",
    metric: "scheduler.wait_ms",
    summary: "Execution has accumulated runnable-thread wait observations.",
    limitations: "Accumulated thread time is not elapsed-wall delay. Sampling coverage and competing CPU consumers require scheduler trace evidence; no cause or lost throughput is inferred.",
};
static IO: PositiveTime = PositiveTime {
    id: "kernel.block_io_delay",
    metric: "io.wait_ms",
    summary: "Execution has accumulated block-I/O delay observations.",
    limitations: "This does not separate device service, queueing, filesystem locks or asynchronous writeback. It is not the critical-path storage delay.",
};
static SYSTEM_CPU: PositiveTime = PositiveTime {
    id: "kernel.system_cpu",
    metric: "cpu.system_ms",
    summary: "Execution has observed system CPU accounting.",
    limitations: "System CPU includes ordinary kernel services. Positive time does not establish kernel inefficiency or a bottleneck; accounting scope is retained in the observation.",
};

pub(super) static REGISTRY: &[&dyn AnalysisRule] = &[&SCHEDULER, &IO, &SYSTEM_CPU];
