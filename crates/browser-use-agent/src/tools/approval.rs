//! Approval policy + decision types (codex `sandboxing.rs` parity).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskForApproval {
    Never,
    OnFailure,
    OnRequest,
    UnlessTrusted,
    Granular(GranularApprovalConfig),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GranularApprovalConfig {
    pub sandbox_approval: bool,
}

impl GranularApprovalConfig {
    pub fn allows_sandbox_approval(&self) -> bool {
        self.sandbox_approval
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ApprovedForSession,
    Denied,
    Abort,
    TimedOut,
}

impl ReviewDecision {
    pub fn is_approved(self) -> bool {
        matches!(self, Self::Approved | Self::ApprovedForSession)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecApprovalRequirement {
    Skip { bypass_sandbox: bool },
    NeedsApproval { reason: Option<String> },
    Forbidden { reason: String },
}

#[derive(Default)]
pub struct ApprovalStore {
    // HashMap<String, ReviewDecision>
}

impl ApprovalStore {
    pub fn get<K: serde::Serialize>(&self, _k: &K) -> Option<ReviewDecision> {
        unimplemented!()
    }

    pub fn put<K: serde::Serialize>(&mut self, _k: K, _v: ReviewDecision) {
        unimplemented!()
    }
}
