use super::counter::ConsecutiveCounter;
use std::sync::{
  Mutex,
  atomic::{AtomicBool, Ordering},
};

/// Shared health state for a single upstream. Accessed by both the active health checker
/// task and (when configured) the request handler for passive observation. Both producers
/// feed the same `ConsecutiveCounter` so a single `unhealthy_threshold` governs the
/// transition regardless of where the failure was observed.
#[derive(Debug)]
pub struct UpstreamHealth {
  healthy: AtomicBool,
  counter: Mutex<ConsecutiveCounter>,
}

impl UpstreamHealth {
  /// Create a new health state, initialized as healthy (optimistic boot). The thresholds
  /// govern how many consecutive failures (or successes) trigger a state transition.
  pub fn new(unhealthy_threshold: u32, healthy_threshold: u32) -> Self {
    Self {
      healthy: AtomicBool::new(true),
      counter: Mutex::new(ConsecutiveCounter::new(unhealthy_threshold, healthy_threshold)),
    }
  }

  /// Returns current health status.
  pub fn is_healthy(&self) -> bool {
    self.healthy.load(Ordering::Relaxed)
  }

  /// Record an observation (active probe result OR passive observation from a real
  /// request). Returns `Some(new_state)` if a transition occurred, `None` otherwise.
  /// Mutex is uncontended in practice because each upstream has its own counter.
  pub fn record(&self, ok: bool) -> Option<bool> {
    let new_state = self.counter.lock().expect("UpstreamHealth counter poisoned").record(ok);
    if let Some(state) = new_state {
      self.healthy.store(state, Ordering::Relaxed);
    }
    new_state
  }

  /// Direct state override for tests that need a deterministic starting point.
  #[cfg(test)]
  pub fn set(&self, healthy: bool) {
    self.healthy.store(healthy, Ordering::Relaxed);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn initial_state_is_healthy() {
    let h = UpstreamHealth::new(3, 2);
    assert!(h.is_healthy());
  }

  #[test]
  fn record_failures_eventually_marks_unhealthy() {
    let h = UpstreamHealth::new(3, 2);
    assert!(h.record(false).is_none());
    assert!(h.record(false).is_none());
    assert_eq!(h.record(false), Some(false));
    assert!(!h.is_healthy());
  }

  #[test]
  fn record_successes_recover_from_unhealthy() {
    let h = UpstreamHealth::new(3, 2);
    h.record(false);
    h.record(false);
    h.record(false);
    assert!(!h.is_healthy());
    assert!(h.record(true).is_none());
    assert_eq!(h.record(true), Some(true));
    assert!(h.is_healthy());
  }
}
