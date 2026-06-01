//! Capture curation tool for browser session recordings.

use anyhow::{anyhow, Context};

use crate::session::SharedStore;
use crate::tools::runtime::{
    Approvable, ExecOutput, SandboxAttempt, Sandboxable, ToolCtx, ToolError, ToolRuntime,
};
use crate::tools::sandbox::{SandboxPermissions, SandboxPreference};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureCurationFrame {
    pub seq: u32,
    pub caption: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaptureCurationRequest {
    #[serde(default)]
    pub frames: Vec<CaptureCurationFrame>,
    pub confirmation_seq: u32,
}

#[derive(Clone)]
pub struct CaptureCurationTool {
    store: Option<SharedStore>,
    session_id: Option<String>,
}

impl CaptureCurationTool {
    pub fn disabled() -> Self {
        Self {
            store: None,
            session_id: None,
        }
    }

    pub fn with_store(store: SharedStore, session_id: impl Into<String>) -> Self {
        Self {
            store: Some(store),
            session_id: Some(session_id.into()),
        }
    }
}

impl Default for CaptureCurationTool {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(serde::Serialize, Clone, Debug, Eq, PartialEq, Hash)]
pub struct CaptureCurationApprovalKey {
    frames: usize,
    confirmation_seq: u32,
}

impl Approvable<CaptureCurationRequest> for CaptureCurationTool {
    type ApprovalKey = CaptureCurationApprovalKey;

    fn approval_keys(&self, req: &CaptureCurationRequest) -> Vec<Self::ApprovalKey> {
        vec![CaptureCurationApprovalKey {
            frames: req.frames.len(),
            confirmation_seq: req.confirmation_seq,
        }]
    }

    fn sandbox_permissions(&self, _req: &CaptureCurationRequest) -> SandboxPermissions {
        SandboxPermissions::UseDefault
    }
}

impl Sandboxable for CaptureCurationTool {
    fn sandbox_preference(&self) -> SandboxPreference {
        SandboxPreference::Auto
    }
}

#[async_trait::async_trait]
impl ToolRuntime<CaptureCurationRequest, ExecOutput> for CaptureCurationTool {
    fn parallel_safe(&self, _req: &CaptureCurationRequest) -> bool {
        false
    }

    async fn run(
        &self,
        req: &CaptureCurationRequest,
        _attempt: &SandboxAttempt<'_>,
        _ctx: &ToolCtx,
    ) -> Result<ExecOutput, ToolError> {
        let store = self
            .store
            .clone()
            .ok_or_else(|| ToolError::Other(anyhow!("capture curation is not configured")))?;
        let session_id = self
            .session_id
            .clone()
            .ok_or_else(|| ToolError::Other(anyhow!("capture curation session is missing")))?;
        let req = req.clone();
        let message = tokio::task::spawn_blocking(move || run_curation(&store, &session_id, &req))
            .await
            .map_err(|e| ToolError::Other(anyhow!("capture curation join failed: {e}")))?
            .map_err(ToolError::Other)?;
        Ok(ExecOutput {
            exit_code: 0,
            stdout: message,
            stderr: String::new(),
        })
    }
}

fn run_curation(
    store: &SharedStore,
    session_id: &str,
    req: &CaptureCurationRequest,
) -> anyhow::Result<String> {
    let artifact_root = {
        let store = store.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
        let session = store
            .load_session(session_id)?
            .with_context(|| format!("unknown session id: {session_id}"))?;
        store.append_event(
            session_id,
            "tool.started",
            serde_json::json!({
                    "name": "submit_capture_curation",
                    "arguments": {
                    "frames": req.frames.clone(),
                    "confirmation_seq": req.confirmation_seq,
                },
            }),
        )?;
        session.artifact_root
    };

    let selection = req
        .frames
        .iter()
        .map(|frame| browser_use_browser::CurationSelection {
            seq: frame.seq,
            caption: frame.caption.clone(),
        })
        .collect::<Vec<_>>();

    let result = browser_use_browser::build_curated_gif(
        std::path::Path::new(&artifact_root),
        &selection,
        Some(req.confirmation_seq),
    );

    let store = store.lock().map_err(|_| anyhow!("store mutex poisoned"))?;
    match result {
        Ok(result) => {
            store.append_event(
                session_id,
                "capture.curation",
                serde_json::json!({
                    "frames": req.frames.clone(),
                    "confirmation_seq": req.confirmation_seq,
                    "gif_path": result.gif_path.display().to_string(),
                    "confirmation_path": result
                        .confirmation_path
                        .as_ref()
                        .map(|path| path.display().to_string()),
                    "frames_used": result.frames_used,
                }),
            )?;
            crate::infra::persistence::record_tool_artifact(
                &store,
                session_id,
                "submit_capture_curation",
                &serde_json::json!({
                    "path": result.gif_path.display().to_string(),
                    "kind": "summary_gif",
                    "mime": "image/gif",
                }),
            )?;
            if let Some(confirmation) = &result.confirmation_path {
                crate::infra::persistence::record_tool_artifact(
                    &store,
                    session_id,
                    "submit_capture_curation",
                    &serde_json::json!({
                        "path": confirmation.display().to_string(),
                        "kind": "confirmation_still",
                        "mime": "image/jpeg",
                    }),
                )?;
            }
            store.append_event(
                session_id,
                "tool.finished",
                serde_json::json!({ "name": "submit_capture_curation" }),
            )?;
            Ok(format!(
                "Saved summary GIF ({} frame(s)) to {}{}",
                result.frames_used,
                result.gif_path.display(),
                result
                    .confirmation_path
                    .as_ref()
                    .map(|path| format!("; confirmation still: {}", path.display()))
                    .unwrap_or_default()
            ))
        }
        Err(error) => {
            store.append_event(
                session_id,
                "tool.failed",
                serde_json::json!({
                    "name": "submit_capture_curation",
                    "error": format!("{error:#}"),
                }),
            )?;
            Ok(format!("capture curation failed: {error:#}"))
        }
    }
}
