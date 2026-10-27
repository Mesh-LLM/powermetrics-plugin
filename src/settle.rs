//! Feeds the meter power readings and observation events in time order.
//!
//! Readings come from this process's clock and events from the host's, both
//! Unix nanoseconds on the same machine. An interval is only settled once the
//! feed's watermark has passed it: an event that arrives late can then still
//! land in the interval it belongs to, and nothing already counted changes.

use crate::meter::EnergyMeter;
use mesh_llm_plugin::proto;
use proto::inference_observation_event::Kind;
use std::collections::VecDeque;

#[derive(Default)]
pub struct Settler {
    pub meter: EnergyMeter,
    readings: VecDeque<(u64, f64)>,
    events: VecDeque<proto::InferenceObservationEvent>,
    /// Every event at or before this has arrived.
    watermark: u64,
    /// The time zero of the meter's seconds, from the first timestamp seen.
    epoch: Option<u64>,
    /// Local models are being served: readings before that are not the node's
    /// serving draw.
    serving: bool,
}

impl Settler {
    /// Where the feed starts, or starts again after a loss.
    pub fn start_from(&mut self, snapshot: &proto::InferenceObservationSnapshot) {
        self.serving |= !snapshot.ready_models.is_empty();
        self.meter.running_since_unknown(
            snapshot
                .active
                .iter()
                .map(|execution| (execution.execution_id, !execution.compute_finished)),
        );
        self.watermark = self.watermark.max(snapshot.taken_at_unix_ns);
        self.settle();
    }

    /// One power reading, summed over the GPUs, at `at` Unix nanoseconds.
    pub fn reading(&mut self, at: u64, watts: f64) {
        self.readings.push_back((at, watts));
        self.settle();
    }

    pub fn batch(&mut self, batch: proto::InferenceObservationBatch) {
        if batch.loss.is_some() {
            // Settle what is known, then nothing running since can be rated.
            self.settle();
            self.events.clear();
            self.meter.lose_track_of_running();
        }
        if let Some(resync) = &batch.resync {
            self.start_from(resync);
        }
        self.events.extend(batch.events);
        self.watermark = self.watermark.max(batch.watermark_unix_ns);
        self.settle();
    }

    fn seconds(&mut self, at: u64) -> f64 {
        let epoch = *self.epoch.get_or_insert(at);
        (at as f64 - epoch as f64) / 1e9
    }

    fn settle(&mut self) {
        loop {
            let reading = self.readings.front().map(|(at, _)| *at);
            let event = self.events.front().map(|event| event.at_unix_ns);
            // An event first when both happened at the same instant: the
            // reading then closes an interval the event already touched.
            let take_event = match (event, reading) {
                (Some(event), Some(reading)) => event <= reading,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => return,
            };
            if take_event {
                let event = self.events.pop_front().expect("checked");
                self.apply(event);
            } else if reading.is_some_and(|at| at <= self.watermark) {
                let (at, watts) = self.readings.pop_front().expect("checked");
                if self.serving {
                    let seconds = self.seconds(at);
                    self.meter.record_power(seconds, watts);
                }
            } else {
                // A reading the feed has not caught up with yet.
                return;
            }
        }
    }

    fn apply(&mut self, event: proto::InferenceObservationEvent) {
        let execution = event.execution_id;
        let seconds = self.seconds(event.at_unix_ns);
        match event.kind {
            Some(Kind::Started(_)) => self.meter.request_started(execution, seconds),
            Some(Kind::ComputeFinished(_)) => self.meter.compute_finished(&execution),
            Some(Kind::Usage(usage)) => {
                self.meter
                    .tokens_completed(execution, u64::from(usage.completion_tokens));
            }
            Some(Kind::Ended(_)) => self.meter.request_closed(&execution, seconds),
            Some(Kind::ModelReady(_)) => self.serving = true,
            Some(Kind::FirstOutput(_) | Kind::ModelUnloaded(_)) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;
    const T0: u64 = 1_800_000_000 * S;

    fn event(
        sequence: u64,
        at: u64,
        execution_id: u64,
        kind: Kind,
    ) -> proto::InferenceObservationEvent {
        proto::InferenceObservationEvent {
            sequence,
            at_unix_ns: at,
            execution_id,
            kind: Some(kind),
        }
    }

    fn batch(
        events: Vec<proto::InferenceObservationEvent>,
        watermark: u64,
    ) -> proto::InferenceObservationBatch {
        proto::InferenceObservationBatch {
            host_generation: 1,
            events,
            watermark_unix_ns: watermark,
            loss: None,
            resync: None,
        }
    }

    fn serving() -> Settler {
        let mut settler = Settler::default();
        settler.start_from(&proto::InferenceObservationSnapshot {
            host_generation: 1,
            next_sequence: 1,
            taken_at_unix_ns: T0,
            ready_models: vec!["qwen".into()],
            active: Vec::new(),
        });
        settler
    }

    fn started() -> Kind {
        Kind::Started(proto::InferenceExecutionStarted {
            model: "qwen".into(),
            operation: proto::InferenceOperation::ChatCompletion as i32,
            streaming: false,
        })
    }

    fn ended() -> Kind {
        Kind::Ended(proto::InferenceExecutionEnded {
            outcome: proto::InferenceExecutionOutcome::Completed as i32,
        })
    }

    fn usage(completion_tokens: u32) -> Kind {
        Kind::Usage(proto::InferenceOutputUsage {
            prompt_tokens: 10,
            cached_prompt_tokens: None,
            completion_tokens,
        })
    }

    #[test]
    fn a_reading_waits_for_the_watermark_so_a_late_event_lands_in_its_interval() {
        let mut settler = serving();
        for second in 0..=10 {
            settler.reading(T0 + second * S, 10.0);
        }
        settler.batch(batch(Vec::new(), T0 + 10 * S));
        // The next readings arrive before the event that made them busy.
        for second in 11..=20 {
            settler.reading(T0 + second * S, 110.0);
        }
        assert_eq!(
            settler.meter.status("t").busy_seconds,
            0.0,
            "not settled yet"
        );
        settler.batch(batch(
            vec![
                event(1, T0 + 10 * S, 1, started()),
                event(2, T0 + 19 * S + S / 2, 1, usage(1_000)),
                event(
                    3,
                    T0 + 19 * S + S / 2,
                    1,
                    Kind::ComputeFinished(proto::Empty {}),
                ),
                event(4, T0 + 19 * S + S / 2, 1, ended()),
            ],
            T0 + 20 * S,
        ));
        let status = settler.meter.status("t");
        assert_eq!(status.idle_seconds, 10.0);
        assert_eq!(status.busy_seconds, 10.0);
        assert_eq!(status.output_tokens, 1_000);
    }

    #[test]
    fn readings_before_any_model_is_served_are_not_counted() {
        let mut settler = Settler::default();
        settler.batch(batch(Vec::new(), T0 + 5 * S));
        for second in 0..=5 {
            settler.reading(T0 + second * S, 10.0);
        }
        assert_eq!(settler.meter.status("t").idle_seconds, 0.0);
        settler.batch(batch(
            vec![event(1, T0 + 6 * S, 0, Kind::ModelReady("qwen".into()))],
            T0 + 12 * S,
        ));
        for second in 6..=12 {
            settler.reading(T0 + second * S, 10.0);
        }
        assert_eq!(settler.meter.status("t").idle_seconds, 6.0);
    }

    #[test]
    fn work_running_through_a_loss_is_never_rated() {
        let mut settler = serving();
        for second in 0..=10 {
            settler.reading(T0 + second * S, 10.0);
        }
        settler.batch(batch(
            vec![event(1, T0 + 10 * S, 1, started())],
            T0 + 10 * S,
        ));
        // The host dropped events, then resyncs: execution 1 still runs.
        settler.batch(proto::InferenceObservationBatch {
            host_generation: 1,
            events: Vec::new(),
            watermark_unix_ns: T0 + 11 * S,
            loss: Some(proto::InferenceObservationLoss {
                first_lost_sequence: 2,
                lost_events: 40,
            }),
            resync: Some(proto::InferenceObservationSnapshot {
                host_generation: 1,
                next_sequence: 42,
                taken_at_unix_ns: T0 + 11 * S,
                ready_models: vec!["qwen".into()],
                active: vec![proto::ActiveInferenceExecution {
                    execution_id: 1,
                    model: "qwen".into(),
                    operation: proto::InferenceOperation::ChatCompletion as i32,
                    streaming: false,
                    started_at_unix_ns: T0 + 10 * S,
                    compute_finished: false,
                }],
            }),
        });
        for second in 11..=20 {
            settler.reading(T0 + second * S, 110.0);
        }
        settler.batch(batch(
            vec![
                event(42, T0 + 19 * S, 1, usage(1_000)),
                event(43, T0 + 19 * S, 1, ended()),
            ],
            T0 + 20 * S,
        ));
        let status = settler.meter.status("t");
        assert!(status.busy_seconds > 0.0, "the node was still busy");
        assert_eq!(
            status.output_tokens, 0,
            "but nothing through the loss is rated"
        );
    }
}
