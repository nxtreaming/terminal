//! Circuit breaker for the guardian reviewer.
//!
//! browser-use addition (NOT codex parity): codex has no circuit breaker
//! around its review flow. The user explicitly asked for a fail-closed
//! guardian with a circuit breaker, so this is a sanctioned addition.
//!
//! Behaviour (fail-CLOSED by construction):
//! - Start `Closed`. Each consecutive reviewer failure increments a
//!   counter. After [`FAILURE_THRESHOLD`] consecutive failures the circuit
//!   trips to `Open`.
//! - While `Open` the circuit *denies* (the caller must NOT invoke the
//!   reviewer and must NOT allow) until [`COOLDOWN`] has elapsed.
//! - After the cooldown the circuit becomes `HalfOpen`: it permits a single
//!   trial call. A success on the trial closes the circuit (resets the
//!   counter); a failure re-opens it for another cooldown.
//! - Any reviewer *success* in `Closed`/`HalfOpen` resets the failure
//!   counter.
//!
//! This is a pure state machine: time is passed in (an injected `now`), so
//! it is fully deterministic and unit-testable with no real clock and no
//! network.

use std::time::Duration;
use std::time::Instant;

/// browser-use-chosen constant: consecutive reviewer failures that trip the
/// breaker. Chosen at 3 — low enough to stop a flapping/erroring reviewer
/// fast (we fail closed in the meantime), high enough to tolerate a single
/// transient hiccup.
pub const FAILURE_THRESHOLD: u32 = 3;

/// browser-use-chosen constant: how long the breaker stays `Open` (denying)
/// before allowing a half-open trial. 30s balances "recover quickly once the
/// reviewer is healthy" against "don't hammer a broken reviewer".
pub const COOLDOWN: Duration = Duration::from_secs(30);

/// Observable state of the breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation; reviewer calls are permitted.
    Closed,
    /// Tripped; reviewer calls are blocked (caller must deny) until cooldown.
    Open,
    /// Cooldown elapsed; a single trial reviewer call is permitted.
    HalfOpen,
}

/// Pure circuit-breaker state machine.
///
/// `Instant` is supplied by the caller (`*_at(now)` methods) so the machine
/// is deterministic in tests.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    threshold: u32,
    cooldown: Duration,
    consecutive_failures: u32,
    /// `Some(when)` while the breaker is tripped; `when` is the instant it
    /// opened. `None` means closed (or has been reset to closed).
    opened_at: Option<Instant>,
    /// Set once a half-open trial has been handed out, so we don't hand out
    /// more than one trial per cooldown window.
    trial_outstanding: bool,
}

impl CircuitBreaker {
    /// Create a breaker with the default browser-use constants.
    pub fn new() -> Self {
        Self::with_config(FAILURE_THRESHOLD, COOLDOWN)
    }

    /// Create a breaker with explicit config (used by unit tests).
    pub fn with_config(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold,
            cooldown,
            consecutive_failures: 0,
            opened_at: None,
            trial_outstanding: false,
        }
    }

    /// Current state given `now`.
    pub fn state_at(&self, now: Instant) -> CircuitState {
        match self.opened_at {
            None => CircuitState::Closed,
            Some(opened) => {
                if now.duration_since(opened) >= self.cooldown {
                    CircuitState::HalfOpen
                } else {
                    CircuitState::Open
                }
            }
        }
    }

    /// Whether the caller is permitted to invoke the reviewer right now.
    ///
    /// FAIL-CLOSED: returns `false` while `Open`, and only `true` for a
    /// single trial while `HalfOpen`. When it returns `false` the caller
    /// MUST deny (never allow).
    pub fn allows_call_at(&mut self, now: Instant) -> bool {
        match self.state_at(now) {
            CircuitState::Closed => true,
            CircuitState::Open => false,
            CircuitState::HalfOpen => {
                if self.trial_outstanding {
                    // A trial is already in flight; do not permit another.
                    false
                } else {
                    self.trial_outstanding = true;
                    true
                }
            }
        }
    }

    /// Record a successful reviewer call. Resets the breaker to `Closed`.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.opened_at = None;
        self.trial_outstanding = false;
    }

    /// Record a failed reviewer call at `now`. May trip or re-open.
    pub fn record_failure_at(&mut self, now: Instant) {
        // A failure always clears any outstanding trial flag.
        self.trial_outstanding = false;
        match self.state_at(now) {
            CircuitState::HalfOpen => {
                // Trial failed: re-open for a fresh cooldown.
                self.opened_at = Some(now);
            }
            CircuitState::Open => {
                // Already open; refresh the window so we keep failing closed.
                self.opened_at = Some(now);
            }
            CircuitState::Closed => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                if self.consecutive_failures >= self.threshold {
                    self.opened_at = Some(now);
                }
            }
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn starts_closed_and_allows() {
        let mut cb = CircuitBreaker::with_config(3, Duration::from_secs(30));
        let now = t0();
        assert_eq!(cb.state_at(now), CircuitState::Closed);
        assert!(cb.allows_call_at(now));
    }

    #[test]
    fn trips_open_after_threshold_consecutive_failures() {
        let mut cb = CircuitBreaker::with_config(3, Duration::from_secs(30));
        let now = t0();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        assert_eq!(cb.state_at(now), CircuitState::Closed);
        cb.record_failure_at(now);
        assert_eq!(cb.state_at(now), CircuitState::Open);
        // While open, calls are NOT permitted (fail closed).
        assert!(!cb.allows_call_at(now));
    }

    #[test]
    fn success_resets_failure_counter() {
        let mut cb = CircuitBreaker::with_config(3, Duration::from_secs(30));
        let now = t0();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        cb.record_success();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        // Only two failures since reset -> still closed.
        assert_eq!(cb.state_at(now), CircuitState::Closed);
    }

    #[test]
    fn open_transitions_to_half_open_after_cooldown() {
        let cooldown = Duration::from_secs(30);
        let mut cb = CircuitBreaker::with_config(2, cooldown);
        let now = t0();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        assert_eq!(cb.state_at(now), CircuitState::Open);
        let later = now + cooldown + Duration::from_secs(1);
        assert_eq!(cb.state_at(later), CircuitState::HalfOpen);
        // Half-open permits exactly one trial.
        assert!(cb.allows_call_at(later));
        assert!(!cb.allows_call_at(later));
    }

    #[test]
    fn half_open_success_closes_circuit() {
        let cooldown = Duration::from_secs(30);
        let mut cb = CircuitBreaker::with_config(2, cooldown);
        let now = t0();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        let later = now + cooldown + Duration::from_secs(1);
        assert!(cb.allows_call_at(later)); // take the trial
        cb.record_success();
        assert_eq!(cb.state_at(later), CircuitState::Closed);
        assert!(cb.allows_call_at(later));
    }

    #[test]
    fn half_open_failure_reopens() {
        let cooldown = Duration::from_secs(30);
        let mut cb = CircuitBreaker::with_config(2, cooldown);
        let now = t0();
        cb.record_failure_at(now);
        cb.record_failure_at(now);
        let later = now + cooldown + Duration::from_secs(1);
        assert!(cb.allows_call_at(later)); // take the trial
        cb.record_failure_at(later);
        assert_eq!(cb.state_at(later), CircuitState::Open);
        // And it stays closed-to-callers (denies) for another cooldown.
        assert!(!cb.allows_call_at(later));
    }
}
