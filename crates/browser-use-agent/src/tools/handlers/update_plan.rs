//! `update_plan` tool: the lightweight planning tool the model calls to publish
//! and maintain a step-by-step task plan.
//!
//! This is the async re-implementation of codex's plan tool over our merged
//! [`ToolRuntime`](crate::tools::runtime::ToolRuntime) seam. It implements the
//! full trait stack ([`Approvable`] + [`Sandboxable`] + [`ToolRuntime`]) so it
//! can be driven by the [`ToolOrchestrator`](crate::tools::orchestrator::ToolOrchestrator),
//! mirroring the shell / view_image tools' structure
//! (`tools/handlers/shell.rs`, `tools/handlers/view_image.rs`).
//!
//! Unlike `shell` / `apply_patch` / `view_image`, this tool touches NO
//! filesystem and spawns NO process: it is a pure "accept + validate + echo the
//! plan" tool. It validates the proposed plan (codex's "at most one step
//! `in_progress`" rule; non-empty steps) and renders a human-readable summary
//! into [`ExecOutput::stdout`].
//!
//! # Parity grounding (file:line in `/home/exedev/repos/codex/codex-rs`)
//!
//! * **Request schema** — codex `UpdatePlanArgs { explanation: Option<String>,
//!   plan: Vec<PlanItemArg> }` and `PlanItemArg { step: String, status:
//!   StepStatus }` (`protocol/src/plan_tool.rs:15-29`). Our [`UpdatePlanRequest`]
//!   / [`PlanItem`] mirror these field-for-field.
//! * **Status enum + wire values** — codex `StepStatus` is
//!   `#[serde(rename_all = "snake_case")]` over `Pending` / `InProgress` /
//!   `Completed` (`protocol/src/plan_tool.rs:7-13`), i.e. the JSON wire strings
//!   `"pending"` / `"in_progress"` / `"completed"`. Our [`PlanStatus`] uses the
//!   identical rename so it round-trips to the exact codex wire strings.
//! * **"at most one in_progress" rule** — codex's plan tool spec states
//!   *"At most one step can be in_progress at a time."*
//!   (`core/src/tools/handlers/plan_spec.rs:37`, the tool `description`). We
//!   enforce this as a hard validation: two-or-more `in_progress` items are
//!   [`ToolError::Rejected`].
//! * **non-empty plan** — codex marks `plan` as a required field
//!   (`plan_spec.rs:44`) and the legacy impl rejected an empty plan
//!   (legacy `browser-use-core/src/tools/mod.rs`, the update_plan arm). An empty
//!   plan, or any item with an empty/blank `step`, is rejected.
//! * **no approval / benign** — codex's plan handler is a pure state echo
//!   (`core/src/tools/handlers/plan.rs:58-91`: it just emits a `PlanUpdate`
//!   event and returns `"Plan updated"`); it needs no approval and touches no
//!   sandbox. We express "no approval needed" by leaving
//!   [`exec_approval_requirement`](Approvable::exec_approval_requirement) at its
//!   default `None`, so the orchestrator's policy-driven
//!   [`default_exec_approval_requirement`](crate::tools::runtime::default_exec_approval_requirement)
//!   applies (which yields `Skip` under any non-prompting policy). See the
//!   `parallel_safe` note below for the codex parity on parallelism.

use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

/// The status of a single plan step.
///
/// Codex parity: `StepStatus` (`protocol/src/plan_tool.rs:7-13`), which is
/// `#[serde(rename_all = "snake_case")]`. The serde rename is reproduced here so
/// the wire strings are byte-identical to codex: `"pending"`, `"in_progress"`,
/// `"completed"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    /// The step has not been started.
    Pending,
    /// The step is currently being worked on. Codex's spec allows at most one of
    /// these at a time (see [`UpdatePlanTool`]).
    InProgress,
    /// The step is finished.
    Completed,
}

/// A single step in the plan.
///
/// Codex parity: `PlanItemArg { step: String, status: StepStatus }`
/// (`protocol/src/plan_tool.rs:15-20`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanItem {
    /// The human-readable description of the step.
    pub step: String,
    /// The step's status.
    pub status: PlanStatus,
}

/// Typed request for the `update_plan` tool.
///
/// Codex parity: `UpdatePlanArgs { explanation: Option<String>, plan:
/// Vec<PlanItemArg> }` (`protocol/src/plan_tool.rs:22-29`). `explanation` is
/// `#[serde(default)]` (so it may be omitted on the wire); we additionally skip
/// it on serialize when `None` to keep our echoed JSON tidy.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdatePlanRequest {
    /// An optional free-text explanation accompanying the plan update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// The ordered list of plan steps.
    pub plan: Vec<PlanItem>,
}

impl UpdatePlanRequest {
    /// Convenience constructor from a list of `(step, status)` pairs, with no
    /// explanation.
    pub fn from_items<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = (S, PlanStatus)>,
        S: Into<String>,
    {
        Self {
            explanation: None,
            plan: items
                .into_iter()
                .map(|(step, status)| PlanItem {
                    step: step.into(),
                    status,
                })
                .collect(),
        }
    }
}

/// Validate a proposed plan against codex's rules.
///
/// Codex parity:
/// * *"At most one step can be in_progress at a time."*
///   (`core/src/tools/handlers/plan_spec.rs:37`, the tool description). Two or
///   more `in_progress` items are a hard [`ToolError::Rejected`].
/// * non-empty plan + non-empty step text — codex marks `plan` as required
///   (`plan_spec.rs:44`); legacy rejected an empty plan
///   (`browser-use-core/src/tools/mod.rs`, the update_plan arm).
///
/// Returns the number of `in_progress` items on success (always 0 or 1).
pub fn validate_plan(plan: &[PlanItem]) -> Result<usize, ToolError> {
    if plan.is_empty() {
        return Err(ToolError::Rejected(
            "update_plan requires a plan with at least one step".to_string(),
        ));
    }

    for (idx, item) in plan.iter().enumerate() {
        if item.step.trim().is_empty() {
            return Err(ToolError::Rejected(format!(
                "update_plan: plan step {} has empty step text",
                idx + 1
            )));
        }
    }

    let in_progress = plan
        .iter()
        .filter(|i| i.status == PlanStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(ToolError::Rejected(format!(
            "update_plan: at most one step may be in_progress at a time (found {in_progress})"
        )));
    }

    Ok(in_progress)
}

/// Render a plan into a human-readable summary for [`ExecOutput::stdout`].
///
/// This is a presentation detail of our [`ExecOutput`] seam (codex emits a
/// structured `PlanUpdate` event and a `"Plan updated"` acknowledgement in
/// `core/src/tools/handlers/plan.rs:22-91`; this crate's `Out` seam exposes only
/// [`ExecOutput`], so we render a textual summary). The leading `"Plan updated"`
/// line mirrors codex's acknowledgement string. Each step is shown with a status
/// glyph; an optional `explanation` is prepended.
pub fn render_plan(req: &UpdatePlanRequest) -> String {
    let mut out = String::new();
    if let Some(explanation) = req.explanation.as_ref() {
        if !explanation.trim().is_empty() {
            out.push_str(explanation.trim());
            out.push('\n');
        }
    }
    out.push_str("Plan updated:\n");
    for item in &req.plan {
        let marker = match item.status {
            PlanStatus::Completed => "[x]",
            PlanStatus::InProgress => "[~]",
            PlanStatus::Pending => "[ ]",
        };
        out.push_str(marker);
        out.push(' ');
        out.push_str(item.step.trim());
        out.push('\n');
    }
    out
}

/// The async `update_plan` tool.
///
/// Stateless; cheap to clone/construct. Performs no I/O and spawns no process.
#[derive(Clone, Debug, Default)]
pub struct UpdatePlanTool;

impl UpdatePlanTool {
    /// Construct a new `update_plan` tool.
    pub fn new() -> Self {
        Self
    }
}

/// Approval key: the full plan identifies a call for session caching, mirroring
/// the shape the shell tool uses for command + cwd (`shell.rs:222-226`) and the
/// view_image tool uses for path + cwd (`view_image.rs:174-178`). In practice
/// this tool never prompts (see the module doc), so the key is rarely consulted;
/// it exists to satisfy the [`Approvable`] contract uniformly.
#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UpdatePlanApprovalKey {
    plan: Vec<(String, PlanStatus)>,
}

// `PlanStatus` needs `Hash` to live in the approval key; hashing the
// discriminant keeps the key cheap and order-stable.
impl std::hash::Hash for PlanStatus {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
    }
}

impl Approvable<UpdatePlanRequest> for UpdatePlanTool {
    type ApprovalKey = UpdatePlanApprovalKey;

    fn approval_keys(&self, req: &UpdatePlanRequest) -> Vec<Self::ApprovalKey> {
        vec![UpdatePlanApprovalKey {
            plan: req
                .plan
                .iter()
                .map(|i| (i.step.clone(), i.status))
                .collect(),
        }]
    }

    /// `update_plan` touches no filesystem; request the default sandbox
    /// permissions (no escalation), mirroring the shell / view_image tools
    /// (`shell.rs:242-244`, `view_image.rs:193-195`).
    fn sandbox_permissions(&self, _req: &UpdatePlanRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }

    // `exec_approval_requirement` is intentionally left at its trait default
    // (`None`): codex's plan handler needs no approval and is not exec-policy
    // governed (`core/src/tools/handlers/plan.rs`). Returning `None` lets the
    // orchestrator apply `default_exec_approval_requirement`, which yields `Skip`
    // under any non-prompting policy (e.g. `AskForApproval::Never`). See the
    // module doc.
}

impl Sandboxable for UpdatePlanTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        // Let the provider decide (today everything resolves to
        // `SandboxType::None`). Matches the shell / view_image tools
        // (`shell.rs:261-267`, `view_image.rs:199-204`). The tool does no I/O, so
        // the sandbox is moot, but `Auto` keeps the seam uniform.
        SandboxPreference::Auto
    }

    fn escalate_on_failure(&self) -> bool {
        // The tool never produces a sandbox denial (it does no I/O), so this is
        // moot; `true` keeps it uniform with the other tools
        // (`shell.rs:269-273`, `view_image.rs:206-210`).
        true
    }
}

#[async_trait::async_trait]
impl ToolRuntime<UpdatePlanRequest, ExecOutput> for UpdatePlanTool {
    fn parallel_safe(&self, _req: &UpdatePlanRequest) -> bool {
        // Match codex. Codex's plan handler does NOT override
        // `supports_parallel_tool_calls` (`core/src/tools/handlers/plan.rs` has
        // no such method), so it inherits the `ToolExecutor` trait default of
        // `false` (`codex-rs/tools/src/tool_executor.rs:51-53`); therefore
        // codex's update_plan is NOT parallel-safe and runs on the serial /
        // write-lock path. We follow that exactly: `false`.
        false
    }

    async fn run(
        &self,
        req: &UpdatePlanRequest,
        attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        // No sandbox is exercised (the tool does no I/O); acknowledge the attempt
        // to make the seam explicit, matching the other tools.
        let _ = attempt;

        // Validate per codex's rules (one in_progress, non-empty steps). A
        // violation is a clean `Rejected` rather than a panic.
        validate_plan(&req.plan)?;

        Ok(ExecOutput {
            exit_code: 0,
            stdout: render_plan(req),
            stderr: String::new(),
        })
    }
}
