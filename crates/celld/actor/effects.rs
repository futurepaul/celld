// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! Effect dispatch keeps ordinary work off the core's lease timer thread.

use super::{CompletedEffect, EffectFuture, Ownership, TimerArm, TimerSlots};
use crate::ownership_store::now_ms;
use celld_logic::{CasGuard, Event, Failure, LeaseCasOutcome, NodeLeaseRecord, OpId};
use futures_util::stream::FuturesUnordered;
use std::future::Future;
use tokio_util::time::{delay_queue, DelayQueue};

enum EffectJob {
    Spawn(EffectFuture),
    // Lease authority must progress even when the host workers are occupied.
    Lease(EffectFuture),
}

#[cfg(celld_internal_tests)]
impl EffectJob {
    fn into_future(self) -> EffectFuture {
        match self {
            Self::Spawn(future) | Self::Lease(future) => future,
        }
    }
}

pub(crate) enum LeaseRequest {
    ReadSelf {
        op: OpId,
        node: String,
    },
    Cas {
        op: OpId,
        guard: CasGuard,
        record: NodeLeaseRecord,
        authority_expires_ms: Option<u64>,
    },
}

/// Futures and timer arms emitted by one Actor transition.
#[derive(Default)]
pub struct StepOutput {
    effects: Vec<EffectJob>,
    pub timers: Vec<TimerArm>,
}

impl StepOutput {
    /// Queue ordinary effect work. In production the drain spawns it on the host
    /// runtime immediately after the Actor step, so it runs to completion and
    /// the core polls only its join handle.
    ///
    /// The difference from `spawn_detached` is only visible when a deterministic
    /// driver takes the queue: dropping an unpolled future discards this work
    /// before it starts.
    pub(crate) fn spawn_scoped(
        &mut self,
        future: impl Future<Output = CompletedEffect> + Send + 'static,
    ) {
        self.effects.push(EffectJob::Spawn(Box::pin(future)));
    }

    /// Production behavior is identical to `spawn_scoped`: the work runs to
    /// completion on the host runtime and the core polls only a join handle.
    ///
    /// This method exists because deterministic worlds drop queued futures and
    /// name these tasks by their `actor.rs` spawn site. Starting the work at the
    /// call keeps seeded stop, restore, and durability regressions on their
    /// existing schedules.
    ///
    /// `#[track_caller]` is load-bearing: the simulated spawn records the caller
    /// location as the task's spawn site.
    #[track_caller]
    pub(crate) fn spawn_detached(
        &mut self,
        future: impl Future<Output = CompletedEffect> + Send + 'static,
    ) {
        let task = crate::asyncrt::spawn(future);
        self.spawn_scoped(async move { task.await.expect("actor effect task panicked") });
    }

    pub(crate) fn lease(&mut self, request: LeaseRequest, ownership: &Ownership) {
        let ownership = ownership.clone();
        let future: EffectFuture = match request {
            LeaseRequest::ReadSelf { op, node } => Box::pin(async move {
                let result = ownership.read_self_node_lease(&node).await;
                CompletedEffect::plain(Event::SelfNodeLeaseRead {
                    op,
                    now_ms: now_ms(),
                    now_mono_ms: crate::asyncrt::mono_ms(),
                    result,
                })
            }),
            LeaseRequest::Cas {
                op,
                guard,
                record,
                authority_expires_ms,
            } => Box::pin(async move {
                let attempt_started_mono_ms = crate::asyncrt::mono_ms();
                let node = record.node.clone();
                let candidate_expires_ms = record.expires_ms;
                // Logged before the CAS: with only the completion line, a
                // renewal hung on a storage tail is indistinguishable from
                // a timer that never fired (the n6 fence, 2026-08-11).
                tracing::info!(
                    event = "node_lease_attempt_started",
                    %node,
                    attempt = if authority_expires_ms.is_some() {
                        "renew"
                    } else {
                        "acquire"
                    },
                    prior_authority_headroom_ms = authority_expires_ms
                        .map(|expires_ms| expires_ms.saturating_sub(now_ms()))
                        .unwrap_or(0),
                    "node lease attempt started"
                );
                // A renewal must return while proven authority remains,
                // because only a returned attempt lets the ambiguity
                // read-back run before the watchdog. The 10:15Z R2
                // brownout fenced 9 nodes whose sole hung attempt was
                // still inside the transport's 15 s timeout when the
                // 10 s TTL expired. Bound the attempt to half the
                // remaining authority (capped, floored) and map timeout
                // to Ambiguous — the same conservative outcome a lost
                // response already produces, so safety is unchanged.
                // The stamp survives a timed-out attempt: the backend
                // writes it through the out-parameter synchronously at
                // serialization, before the transport await, so even a
                // dropped future has reported what the possibly-landed
                // body carried.
                let mut stamped_log_state = None;
                let result = match authority_expires_ms {
                    Some(expires_ms) => {
                        let remaining = expires_ms.saturating_sub(now_ms());
                        let bound =
                            std::time::Duration::from_millis((remaining / 2).clamp(250, 2_500));
                        match crate::asyncrt::timeout(
                            bound,
                            ownership.cas_node_lease(guard, record, &mut stamped_log_state),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(Failure::Ambiguous),
                        }
                    }
                    None => {
                        ownership
                            .cas_node_lease(guard, record, &mut stamped_log_state)
                            .await
                    }
                };
                let completed_ms = now_ms();
                let elapsed_ms = crate::asyncrt::mono_ms().saturating_sub(attempt_started_mono_ms);
                let prior_authority_headroom_ms = authority_expires_ms
                    .map(|expires_ms| expires_ms.saturating_sub(completed_ms))
                    .unwrap_or(0);
                let candidate_headroom_ms = candidate_expires_ms.saturating_sub(completed_ms);
                let attempt = if authority_expires_ms.is_some() {
                    "renew"
                } else {
                    "acquire"
                };
                let outcome = match &result {
                    Ok(LeaseCasOutcome::Applied { .. }) => "applied",
                    Ok(LeaseCasOutcome::Rejected) => "rejected",
                    Err(Failure::Ambiguous) => "ambiguous",
                    Err(Failure::Definite) => "definite_failure",
                };
                if matches!(&result, Ok(LeaseCasOutcome::Applied { .. })) {
                    tracing::info!(
                        event = "node_lease_attempt",
                        %node,
                        attempt,
                        outcome,
                        elapsed_ms,
                        prior_authority_headroom_ms,
                        candidate_headroom_ms,
                        "node lease attempt completed"
                    );
                } else {
                    tracing::warn!(
                        event = "node_lease_attempt",
                        %node,
                        attempt,
                        outcome,
                        elapsed_ms,
                        prior_authority_headroom_ms,
                        candidate_headroom_ms,
                        "node lease attempt did not apply"
                    );
                }
                CompletedEffect::plain(Event::NodeLeaseCasCompleted {
                    op,
                    now_mono_ms: crate::asyncrt::mono_ms(),
                    result,
                    stamped_log_state,
                })
            }),
        };
        self.effects.push(EffectJob::Lease(future));
    }

    // Binary test harnesses compile the library without cfg(test).
    // celld_internal_tests is never set in a shipped build.
    #[cfg(celld_internal_tests)]
    pub fn take_effects(&mut self) -> Vec<EffectFuture> {
        self.effects.drain(..).map(EffectJob::into_future).collect()
    }

    #[cfg(celld_internal_tests)]
    pub fn effects_is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    // A driver that runs only the newest effect must retain every older job.
    #[cfg(celld_internal_tests)]
    pub fn pop_effect(&mut self) -> Option<EffectFuture> {
        self.effects.pop().map(EffectJob::into_future)
    }

    #[cfg(celld_internal_tests)]
    pub fn effects_len(&self) -> usize {
        self.effects.len()
    }
}

pub(crate) fn drain_step_output(
    out: &mut StepOutput,
    effects: &mut FuturesUnordered<EffectFuture>,
    delays: &mut DelayQueue<TimerArm>,
    timers: &mut TimerSlots<delay_queue::Key>,
) {
    for job in out.effects.drain(..) {
        match job {
            EffectJob::Spawn(future) => {
                // Spawn only at the production drain so deterministic drivers
                // can take the futures and retain their existing schedule.
                let task = crate::asyncrt::spawn(future);
                effects.push(Box::pin(async move {
                    task.await.expect("actor effect task panicked")
                }));
            }
            EffectJob::Lease(future) => effects.push(future),
        }
    }
    for arm in out.timers.drain(..) {
        let key = queue_arm(delays, arm.clone(), crate::asyncrt::mono_ms());
        if let Some(displaced) = timers.install(&arm, key) {
            delays.remove(&displaced);
        }
    }
}

/// The longest wait one arm puts in the timer queue.
///
/// tokio-util's `DelayQueue` panics on a deadline past its wheel's span,
/// 2^36 - 1 ms (about 2.2 years), counted from the queue's own clock, which
/// lags until the queue is next polled. A timer due later than this (an
/// object's alarm in 2100) is queued at this bound and queued again for the
/// rest when that expires ([`requeue_early`]). A year leaves the other half
/// of the span for the lag.
pub(crate) const MAX_QUEUED_MS: u64 = 365 * 24 * 60 * 60 * 1000;
const _: () = assert!(MAX_QUEUED_MS < (1 << 36) - 1);

fn queue_arm(delays: &mut DelayQueue<TimerArm>, arm: TimerArm, now_mono_ms: u64) -> delay_queue::Key {
    let wait = arm.at_mono_ms.saturating_sub(now_mono_ms).min(MAX_QUEUED_MS);
    delays.insert(arm, std::time::Duration::from_millis(wait))
}

/// Queues the rest of an arm that expired before its time, because it was
/// queued at [`MAX_QUEUED_MS`]. Answers the arm back when it is due, to fire.
pub(crate) fn requeue_early(
    arm: TimerArm,
    now_mono_ms: u64,
    delays: &mut DelayQueue<TimerArm>,
    timers: &mut TimerSlots<delay_queue::Key>,
) -> Option<TimerArm> {
    if arm.at_mono_ms <= now_mono_ms || !timers.is_armed(&arm.slot, arm.ordinal) {
        return Some(arm);
    }
    let key = queue_arm(delays, arm.clone(), now_mono_ms);
    // The key it displaces is the one that just expired: the queue holds it
    // no longer, so there is nothing to remove.
    timers.install(&arm, key);
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use celld_logic::Timer;

    const LATER_MS: u64 = 5_000;
    // The queue rounds a deadline up to its next millisecond.

    /// An alarm past the queue's span is queued at its bound, queued again
    /// for the rest when that expires, and answered to fire only when due.
    #[tokio::test]
    async fn a_timer_past_the_queues_span_waits_out_its_whole_time() {
        let mut delays = DelayQueue::new();
        let mut timers = TimerSlots::<delay_queue::Key>::default();
        // The Actor numbers each arm; the loop holds its queue key.
        let mut actor = TimerSlots::<()>::default();
        let now = 1_000;
        let at = now + MAX_QUEUED_MS + LATER_MS;
        let timer = Timer::CellAlarm {
            cell: "far".to_string(),
            generation: 1,
        };
        let (arm, _) = actor.arm(timer, at, ());
        let key = queue_arm(&mut delays, arm.clone(), now);
        assert!(timers.install(&arm, key).is_none());
        let queued = delays.deadline(&key) - tokio::time::Instant::now();
        assert!(queued <= std::time::Duration::from_millis(MAX_QUEUED_MS + 1), "{queued:?}");
        delays.remove(&key);

        // The bound expired, short of the alarm's time: queued again, not fired.
        let bound = now + MAX_QUEUED_MS;
        assert_eq!(requeue_early(arm.clone(), bound, &mut delays, &mut timers), None);
        assert_eq!(delays.len(), 1);
        assert!(timers.is_armed(&arm.slot, arm.ordinal));
        let key = delays.peek().expect("queued again");
        let queued = delays.deadline(&key) - tokio::time::Instant::now();
        assert!(queued <= std::time::Duration::from_millis(LATER_MS + 1), "{queued:?}");
        delays.remove(&key);

        // Due: answered back to fire.
        assert_eq!(requeue_early(arm.clone(), at, &mut delays, &mut timers), Some(arm.clone()));
        assert!(delays.is_empty());

        // An arm another displaced is never queued again.
        let (newer, _) = actor.arm(arm.timer.clone(), at, ());
        assert_ne!(newer.ordinal, arm.ordinal);
        timers.install(&newer, delays.insert(newer.clone(), std::time::Duration::ZERO));
        assert_eq!(requeue_early(arm.clone(), bound, &mut delays, &mut timers), Some(arm));
        assert_eq!(delays.len(), 1, "only the newer arm is queued");
    }
}
