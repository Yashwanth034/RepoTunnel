use std::time::{Duration, Instant};

use serde::Serialize;

#[derive(Clone, Copy, Debug)]
pub(crate) enum LatencyPhase {
    Preflight,
    Observation,
    Mutation,
    Wait,
    FinalObservation,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LatencyTimings {
    pub(crate) preflight_ms: u64,
    pub(crate) observation_ms: u64,
    pub(crate) mutation_ms: u64,
    pub(crate) wait_ms: u64,
    pub(crate) final_observation_ms: u64,
    pub(crate) repo_tunnel_internal_ms: u64,
    pub(crate) external_mcp_round_trip_included: bool,
}

impl LatencyTimings {
    pub(crate) fn add_preflight(&mut self, duration: Duration) {
        let value = millis(duration);
        self.preflight_ms = self.preflight_ms.saturating_add(value);
        self.repo_tunnel_internal_ms = self.repo_tunnel_internal_ms.saturating_add(value);
    }
}

#[derive(Debug)]
pub(crate) struct LatencyTracker {
    started: Instant,
    timings: LatencyTimings,
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

impl LatencyTracker {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            timings: LatencyTimings::default(),
        }
    }

    pub(crate) fn record(&mut self, phase: LatencyPhase, duration: Duration) {
        let value = millis(duration);
        let slot = match phase {
            LatencyPhase::Preflight => &mut self.timings.preflight_ms,
            LatencyPhase::Observation => &mut self.timings.observation_ms,
            LatencyPhase::Mutation => &mut self.timings.mutation_ms,
            LatencyPhase::Wait => &mut self.timings.wait_ms,
            LatencyPhase::FinalObservation => &mut self.timings.final_observation_ms,
        };
        *slot = slot.saturating_add(value);
    }

    pub(crate) fn snapshot(&self) -> LatencyTimings {
        let mut timings = self.timings.clone();
        timings.repo_tunnel_internal_ms = millis(self.started.elapsed());
        timings.external_mcp_round_trip_included = false;
        timings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timings_only_serialize_fixed_numeric_phases_and_transport_flag() {
        let mut tracker = LatencyTracker::new();
        tracker.record(LatencyPhase::Observation, Duration::from_millis(7));
        tracker.record(LatencyPhase::Mutation, Duration::from_millis(3));
        let value = serde_json::to_value(tracker.snapshot()).expect("serialize latency timings");
        let object = value.as_object().expect("timings object");

        assert_eq!(
            object.get("observationMs").and_then(|v| v.as_u64()),
            Some(7)
        );
        assert_eq!(object.get("mutationMs").and_then(|v| v.as_u64()), Some(3));
        assert_eq!(
            object
                .get("externalMcpRoundTripIncluded")
                .and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(object
            .values()
            .all(|value| value.is_number() || value.is_boolean()));
        assert!(!serde_json::to_string(&value).unwrap().contains("secret"));
    }

    #[test]
    fn repeated_phase_recording_saturates_into_one_counter() {
        let mut tracker = LatencyTracker::new();
        tracker.record(LatencyPhase::Observation, Duration::from_millis(4));
        tracker.record(LatencyPhase::Observation, Duration::from_millis(6));
        assert_eq!(tracker.snapshot().observation_ms, 10);
    }
}
