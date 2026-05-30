//! Pure stream-retry decision core (codex `turn.rs:924-1021`).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetryAction {
    Fail,
    SwitchTransport,
    Backoff { delay_ms: u64 },
}

pub fn retry_decision(
    retries: u32,
    max: u32,
    retryable: bool,
    can_switch_transport: bool,
    requested_delay_ms: Option<u64>,
) -> RetryAction {
    if !retryable {
        return RetryAction::Fail;
    }
    if retries >= max {
        return if can_switch_transport {
            RetryAction::SwitchTransport
        } else {
            RetryAction::Fail
        };
    }
    RetryAction::Backoff {
        delay_ms: requested_delay_ms.unwrap_or(backoff_ms(retries)),
    }
}

/// Deterministic, pure exponential backoff.
pub fn backoff_ms(retries: u32) -> u64 {
    200u64.saturating_mul(1u64 << retries.min(6))
}
