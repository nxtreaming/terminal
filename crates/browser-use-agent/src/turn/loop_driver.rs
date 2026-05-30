//! Unbounded turn-loop driver (codex `turn.rs:214-397`). No max-turns counter.

use super::{SamplingDriver, TurnObserver, TurnState};
use crate::events::TurnCtx;
use tokio_util::sync::CancellationToken;

pub struct TurnLoop<St, Sd, Ob> {
    _s: std::marker::PhantomData<(St, Sd, Ob)>,
}

impl<St: TurnState, Sd: SamplingDriver, Ob: TurnObserver> TurnLoop<St, Sd, Ob> {
    pub fn new(_state: St, _sampler: Sd, _observer: Ob) -> Self {
        unimplemented!()
    }

    /// Unbounded driver (`turn.rs:214-397`). Returns the last agent message.
    pub async fn run(
        &self,
        _ctx: TurnCtx,
        _turn_has_fresh_input: bool,
        _cancel: CancellationToken,
    ) -> Result<Option<String>, crate::AgentError> {
        unimplemented!()
    }
}
