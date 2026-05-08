use ahash::HashSet;
use std::sync::Arc;

/// Default HTTP status codes treated as health failures (used when `passive_health` is
/// configured without an explicit `unhealthy_statuses` list).
const DEFAULT_UNHEALTHY_STATUSES: &[u16] = &[502, 503, 504];

/// Precompute the union of trigger statuses across both sub-configs. Cheap-clone case
/// (one sub-config set): reuse the existing `Arc<HashSet<u16>>`. Both-set: allocate the
/// union once. `(None, None)` is unreachable from `build`, but we return an empty Arc
/// defensively so the method is total.
fn compute_retry_status_union(
  passive_health: &Option<PassiveHealthConfig>,
  app_fallback: &Option<AppFallbackConfig>,
) -> Arc<HashSet<u16>> {
  match (passive_health, app_fallback) {
    (Some(ph), None) => ph.unhealthy_statuses.clone(),
    (None, Some(af)) => af.fallback_on_statuses.clone(),
    (Some(ph), Some(af)) => {
      let merged: HashSet<u16> = ph
        .unhealthy_statuses
        .iter()
        .chain(af.fallback_on_statuses.iter())
        .copied()
        .collect();
      Arc::new(merged)
    }
    (None, None) => Arc::new(HashSet::default()),
  }
}

/// Health-related failover triggers. Failures observed on real traffic update the
/// upstream's `UpstreamHealth` state via `record(false)` (the same state the active
/// `health-check` task drives) AND retry the current request against the next upstream.
/// Requires `health-check` to be configured on the same `[[reverse_proxy]]` block;
/// without `health-check` the observation has nowhere to land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassiveHealthConfig {
  /// HTTP status codes treated as health failures (default `[502, 503, 504]`).
  pub unhealthy_statuses: Arc<HashSet<u16>>,
  /// Whether to treat connection errors (timeout, refused, etc.) as health failures.
  pub on_connection_failure: bool,
}

/// Application-level routing fallback for migration / canary scenarios. Triggers retry
/// against the next upstream WITHOUT touching upstream health state — these statuses
/// indicate the application doesn't have a route, not that the upstream is sick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFallbackConfig {
  /// HTTP status codes that trigger a routing retry (e.g. `[404, 501]` for migration).
  pub fallback_on_statuses: Arc<HashSet<u16>>,
}

/// Combined failover behavior. At least one of `passive_health` or `app_fallback` must
/// be set for the config to exist on an upstream group; if both are `None`, the route
/// has no failover configured and `UpstreamCandidates::failover_config` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailoverConfig {
  pub passive_health: Option<PassiveHealthConfig>,
  pub app_fallback: Option<AppFallbackConfig>,
  /// Maximum retry attempts (default: number of upstreams - 1).
  pub max_retries: usize,
  /// Opt-in to retry non-idempotent methods (POST/PATCH). Default `false`.
  /// RFC 9110 §9.2.2 only lists GET/HEAD/PUT/DELETE/OPTIONS/TRACE as idempotent;
  /// retrying others risks double-write side effects.
  pub retry_non_idempotent: bool,
  /// Precomputed union of every status that triggers retry — built once at config-build
  /// time so the request hot path returns a cheap `Arc::clone` instead of allocating a
  /// fresh `HashSet` per request.
  retry_status_union: Arc<HashSet<u16>>,
}

impl FailoverConfig {
  /// Construct from raw TOML inputs. Returns `None` if neither passive_health nor
  /// app_fallback is requested (caller treats as "no failover configured").
  pub fn build(
    passive_health: Option<PassiveHealthInput>,
    app_fallback: Option<AppFallbackInput>,
    max_retries: Option<usize>,
    retry_non_idempotent: Option<bool>,
    num_upstreams: usize,
  ) -> Option<Self> {
    if passive_health.is_none() && app_fallback.is_none() {
      return None;
    }
    let passive_health = passive_health.map(|p| PassiveHealthConfig {
      unhealthy_statuses: Arc::new(
        p.unhealthy_statuses
          .map(|v| v.into_iter().collect())
          .unwrap_or_else(|| DEFAULT_UNHEALTHY_STATUSES.iter().copied().collect()),
      ),
      on_connection_failure: p.on_connection_failure.unwrap_or(true),
    });
    let app_fallback = app_fallback.map(|a| AppFallbackConfig {
      fallback_on_statuses: Arc::new(a.fallback_on_statuses.into_iter().collect()),
    });
    let retry_status_union = compute_retry_status_union(&passive_health, &app_fallback);
    Some(Self {
      passive_health,
      app_fallback,
      max_retries: max_retries.unwrap_or_else(|| num_upstreams.saturating_sub(1)),
      retry_non_idempotent: retry_non_idempotent.unwrap_or(false),
      retry_status_union,
    })
  }

  /// Validate that all configured status codes are in the 4xx/5xx range.
  pub fn validate(&self) -> Result<(), String> {
    let in_range = |s: u16| (400..600).contains(&s);
    if let Some(ph) = &self.passive_health {
      for &status in ph.unhealthy_statuses.iter() {
        if !in_range(status) {
          return Err(format!(
            "passive_health.unhealthy_statuses contains {status} (must be 400-599)"
          ));
        }
      }
    }
    if let Some(af) = &self.app_fallback {
      for &status in af.fallback_on_statuses.iter() {
        if !in_range(status) {
          return Err(format!(
            "app_fallback.fallback_on_statuses contains {status} (must be 400-599)"
          ));
        }
      }
    }
    Ok(())
  }

  /// Union of every status code that should trigger retry, regardless of whether the
  /// trigger came from passive_health or app_fallback. Used for the cache-skip extension
  /// so triggering responses don't poison the response cache. Cheap `Arc::clone` —
  /// the union is precomputed once in `build` and never mutated.
  pub fn all_retry_statuses(&self) -> Arc<HashSet<u16>> {
    self.retry_status_union.clone()
  }

  /// Convenience predicate: returns true if `status` is a passive_health failure trigger.
  /// Equivalent to `matches!(self.classify_status(status), StatusClassification::HealthFailure)`
  /// but avoids walking app_fallback when only the health verdict matters.
  pub(crate) fn is_health_failure_status(&self, status: u16) -> bool {
    self
      .passive_health
      .as_ref()
      .is_some_and(|ph| ph.unhealthy_statuses.contains(&status))
  }

  /// Classify a response status against the configured triggers. Used by the retry loop
  /// to decide whether to retry, record health failure, or pass through.
  pub(crate) fn classify_status(&self, status: u16) -> StatusClassification {
    let passive_match = self
      .passive_health
      .as_ref()
      .is_some_and(|ph| ph.unhealthy_statuses.contains(&status));
    if passive_match {
      return StatusClassification::HealthFailure;
    }
    let app_match = self
      .app_fallback
      .as_ref()
      .is_some_and(|af| af.fallback_on_statuses.contains(&status));
    if app_match {
      return StatusClassification::AppFallback;
    }
    StatusClassification::Pass
  }
}

/// Raw TOML input for passive health (every field optional, defaults applied at build).
#[derive(Debug, Clone, Default)]
pub struct PassiveHealthInput {
  pub unhealthy_statuses: Option<Vec<u16>>,
  pub on_connection_failure: Option<bool>,
}

/// Raw TOML input for application-level fallback. `fallback_on_statuses` is required —
/// there's no sensible default since this is a routing decision, not a health one.
#[derive(Debug, Clone)]
pub struct AppFallbackInput {
  pub fallback_on_statuses: Vec<u16>,
}

impl From<&crate::globals::PassiveHealthRoute> for PassiveHealthInput {
  fn from(r: &crate::globals::PassiveHealthRoute) -> Self {
    Self {
      unhealthy_statuses: r.unhealthy_statuses.clone(),
      on_connection_failure: r.on_connection_failure,
    }
  }
}

impl From<&crate::globals::AppFallbackRoute> for AppFallbackInput {
  fn from(r: &crate::globals::AppFallbackRoute) -> Self {
    Self {
      fallback_on_statuses: r.fallback_on_statuses.clone(),
    }
  }
}

/// Outcome of classifying a response status against the failover config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusClassification {
  /// Status indicates upstream health failure: record it and retry.
  HealthFailure,
  /// Status indicates application-level routing fallback: retry without touching health.
  AppFallback,
  /// Status doesn't match any configured trigger: pass response through.
  Pass,
}

/// Context tracking state during failover retries
#[derive(Debug, Clone)]
pub struct FailoverContext {
  /// Set of upstream indices that have been tried
  tried_upstreams: HashSet<usize>,
  /// Current retry count
  pub retry_count: usize,
  /// Index of the initial upstream selected by load balancer
  pub initial_upstream_idx: usize,
}

impl FailoverContext {
  /// Create a new failover context starting from the given upstream index
  pub fn new(initial_upstream_idx: usize) -> Self {
    Self {
      tried_upstreams: HashSet::default(),
      retry_count: 0,
      initial_upstream_idx,
    }
  }

  /// Check if an upstream has already been tried
  pub fn has_tried(&self, upstream_idx: usize) -> bool {
    self.tried_upstreams.contains(&upstream_idx)
  }

  /// Mark an upstream as tried
  pub fn mark_tried(&mut self, upstream_idx: usize) {
    self.tried_upstreams.insert(upstream_idx);
  }

  /// Check if we can retry based on max_retries limit
  pub fn can_retry(&self, max_retries: usize) -> bool {
    self.retry_count < max_retries
  }

  /// Increment retry counter
  pub fn increment_retry(&mut self) {
    self.retry_count += 1;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn ph(statuses: Option<Vec<u16>>, on_connection_failure: Option<bool>) -> PassiveHealthInput {
    PassiveHealthInput {
      unhealthy_statuses: statuses,
      on_connection_failure,
    }
  }

  fn af(statuses: Vec<u16>) -> AppFallbackInput {
    AppFallbackInput {
      fallback_on_statuses: statuses,
    }
  }

  #[test]
  fn build_returns_none_when_neither_set() {
    assert!(FailoverConfig::build(None, None, None, None, 3).is_none());
  }

  #[test]
  fn build_passive_health_defaults() {
    let cfg = FailoverConfig::build(Some(ph(None, None)), None, None, None, 3).unwrap();
    let p = cfg.passive_health.unwrap();
    assert_eq!(p.unhealthy_statuses.len(), 3);
    assert!(p.unhealthy_statuses.contains(&502));
    assert!(p.unhealthy_statuses.contains(&503));
    assert!(p.unhealthy_statuses.contains(&504));
    assert!(p.on_connection_failure);
    assert!(cfg.app_fallback.is_none());
    assert_eq!(cfg.max_retries, 2);
    assert!(!cfg.retry_non_idempotent);
  }

  #[test]
  fn build_passive_health_overrides() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![500]), Some(false))), None, Some(5), Some(true), 3).unwrap();
    let p = cfg.passive_health.unwrap();
    assert!(p.unhealthy_statuses.contains(&500));
    assert!(!p.on_connection_failure);
    assert_eq!(cfg.max_retries, 5);
    assert!(cfg.retry_non_idempotent);
  }

  #[test]
  fn build_app_fallback_only() {
    let cfg = FailoverConfig::build(None, Some(af(vec![404, 501])), None, None, 2).unwrap();
    assert!(cfg.passive_health.is_none());
    let a = cfg.app_fallback.unwrap();
    assert_eq!(a.fallback_on_statuses.len(), 2);
    assert!(a.fallback_on_statuses.contains(&404));
  }

  #[test]
  fn build_both_sections() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502]), None)), Some(af(vec![404])), None, None, 3).unwrap();
    assert!(cfg.passive_health.is_some());
    assert!(cfg.app_fallback.is_some());
  }

  #[test]
  fn validate_rejects_out_of_range() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![200]), None)), None, None, None, 2).unwrap();
    assert!(cfg.validate().is_err());

    let cfg = FailoverConfig::build(None, Some(af(vec![600])), None, None, 2).unwrap();
    assert!(cfg.validate().is_err());

    let cfg = FailoverConfig::build(Some(ph(Some(vec![502]), None)), Some(af(vec![404])), None, None, 2).unwrap();
    assert!(cfg.validate().is_ok());
  }

  #[test]
  fn classify_status_health_failure_takes_priority() {
    // Same status in both lists — passive_health wins because health observation is the
    // stronger signal (we want it recorded even if it's also in app_fallback).
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502]), None)), Some(af(vec![502])), None, None, 2).unwrap();
    assert_eq!(cfg.classify_status(502), StatusClassification::HealthFailure);
  }

  #[test]
  fn classify_status_routes_correctly() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502]), None)), Some(af(vec![404])), None, None, 2).unwrap();
    assert_eq!(cfg.classify_status(502), StatusClassification::HealthFailure);
    assert_eq!(cfg.classify_status(404), StatusClassification::AppFallback);
    assert_eq!(cfg.classify_status(200), StatusClassification::Pass);
    assert_eq!(cfg.classify_status(500), StatusClassification::Pass);
  }

  #[test]
  fn all_retry_statuses_unions_both_sets() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502, 503]), None)), Some(af(vec![404])), None, None, 3).unwrap();
    let union = cfg.all_retry_statuses();
    assert_eq!(union.len(), 3);
    assert!(union.contains(&502));
    assert!(union.contains(&503));
    assert!(union.contains(&404));
  }

  #[test]
  fn all_retry_statuses_passive_only_reuses_arc() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502, 503]), None)), None, None, None, 2).unwrap();
    let union = cfg.all_retry_statuses();
    assert_eq!(union.len(), 2);
    assert!(union.contains(&502));
    assert!(union.contains(&503));
    // No allocation: the Arc returned shares the same allocation as passive_health.unhealthy_statuses.
    assert!(Arc::ptr_eq(&union, &cfg.passive_health.as_ref().unwrap().unhealthy_statuses));
  }

  #[test]
  fn all_retry_statuses_app_fallback_only_reuses_arc() {
    let cfg = FailoverConfig::build(None, Some(af(vec![404])), None, None, 2).unwrap();
    let union = cfg.all_retry_statuses();
    assert_eq!(union.len(), 1);
    assert!(union.contains(&404));
    assert!(Arc::ptr_eq(&union, &cfg.app_fallback.as_ref().unwrap().fallback_on_statuses));
  }

  #[test]
  fn is_health_failure_status_only_matches_passive_health() {
    let cfg = FailoverConfig::build(Some(ph(Some(vec![502]), None)), Some(af(vec![404])), None, None, 2).unwrap();
    assert!(cfg.is_health_failure_status(502));
    assert!(!cfg.is_health_failure_status(404));
    assert!(!cfg.is_health_failure_status(200));

    // App-fallback-only config: every status returns false (no health observation).
    let cfg = FailoverConfig::build(None, Some(af(vec![404])), None, None, 2).unwrap();
    assert!(!cfg.is_health_failure_status(404));
    assert!(!cfg.is_health_failure_status(502));
  }

  #[test]
  fn failover_context_get_next_iterates_from_initial_index() {
    let mut ctx = FailoverContext::new(2);
    assert!(!ctx.has_tried(2));
    ctx.mark_tried(2);
    assert!(ctx.has_tried(2));
    assert!(!ctx.has_tried(0));

    ctx.increment_retry();
    assert_eq!(ctx.retry_count, 1);
    assert!(ctx.can_retry(2));
    ctx.increment_retry();
    assert!(!ctx.can_retry(2));
  }
}
