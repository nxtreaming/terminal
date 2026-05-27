use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use browser_use_protocol::{SessionMeta, ToolCall};
use browser_use_store::Store;
use ignore::WalkBuilder;
use serde_json::{json, Value};
use sha1::Digest;

use crate::prompt_image::{load_for_prompt_bytes, PromptImageMode};
use crate::tools::agent_env::{apply_agent_tool_path_to_command, ripgrep_command_path};

const DEFAULT_MAX_READ_LINES: usize = 400;
const DEFAULT_MAX_READ_BYTES: usize = 80_000;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 100;
const DEFAULT_MAX_LIST_RESULTS: usize = 200;
const MAX_INLINE_LOCAL_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_TURN_DIFF_CHARS: usize = 60_000;
const PATCH_REJECTED_OUTSIDE_PROJECT_REASON: &str =
    "patch rejected: writing outside of the project; rejected by user approval settings";
const PROTECTED_PATCH_METADATA_NAMES: &[&str] = &[".git", ".agents", ".browser-use", ".codex"];
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
const DEV_NULL: &str = "/dev/null";
const REGULAR_FILE_MODE: &str = "100644";

static TURN_DIFF_TRACKERS: OnceLock<Mutex<HashMap<String, TurnDiffTracker>>> = OnceLock::new();

#[derive(Debug)]
pub(crate) struct FileToolResult {
    pub(crate) content: Value,
}

pub(crate) fn reset_turn_diff_tracker_for_session(session_id: &str, cwd: &Path) {
    let mut trackers = turn_diff_trackers()
        .lock()
        .expect("turn diff tracker mutex poisoned");
    trackers.insert(
        session_id.to_string(),
        TurnDiffTracker::with_display_root(cwd.to_path_buf()),
    );
}

pub(crate) fn read_file(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "read_file", || {
        let path = required_path(session, &call.arguments)?;
        let max_bytes = usize_arg(&call.arguments, "max_bytes").unwrap_or(DEFAULT_MAX_READ_BYTES);
        let max_lines = usize_arg(&call.arguments, "max_lines").unwrap_or(DEFAULT_MAX_READ_LINES);
        let start_line = usize_arg(&call.arguments, "start_line").unwrap_or(1).max(1);
        let end_line = usize_arg(&call.arguments, "end_line");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let binary = bytes.iter().take(8192).any(|byte| *byte == 0);
        if binary {
            let text = format!("binary file: {} ({} bytes)", path.display(), bytes.len());
            store.append_event(
                &session.id,
                "file.read",
                json!({
                    "tool_call_id": call.id,
                    "path": path.display().to_string(),
                    "binary": true,
                    "bytes": bytes.len(),
                }),
            )?;
            return Ok(FileToolResult {
                content: Value::String(text),
            });
        }

        let text = String::from_utf8_lossy(&bytes);
        let lines = text.lines().collect::<Vec<_>>();
        let total_lines = lines.len();
        let end =
            end_line.unwrap_or_else(|| start_line.saturating_add(max_lines).saturating_sub(1));
        let mut selected = Vec::new();
        for line_no in start_line..=end {
            let Some(line) = lines.get(line_no.saturating_sub(1)) else {
                break;
            };
            selected.push(format!("{line_no:>6}\t{line}"));
            if selected.len() >= max_lines {
                break;
            }
        }
        let mut rendered = selected.join("\n");
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        let range_truncated = end < total_lines || selected.len() >= max_lines;
        let (rendered, byte_truncated) = truncate_chars(&rendered, max_bytes);
        let truncated = range_truncated || byte_truncated;
        let content = format!(
            "{}:{}-{} ({} lines{})\n{}",
            path.display(),
            start_line,
            start_line + selected.len().saturating_sub(1),
            total_lines,
            if truncated { ", truncated" } else { "" },
            rendered,
        );
        store.append_event(
            &session.id,
            "file.read",
            json!({
                "tool_call_id": call.id,
                "path": path.display().to_string(),
                "start_line": start_line,
                "end_line": start_line + selected.len().saturating_sub(1),
                "total_lines": total_lines,
                "truncated": truncated,
                "bytes": bytes.len(),
            }),
        )?;
        Ok(FileToolResult {
            content: Value::String(content),
        })
    })
}

pub(crate) fn search_files(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "search_files", || {
        let query = call
            .arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            bail!("search_files requires query");
        }
        let root = optional_path(session, &call.arguments, "path")?
            .unwrap_or_else(|| PathBuf::from(&session.cwd));
        let max_results =
            usize_arg(&call.arguments, "max_results").unwrap_or(DEFAULT_MAX_SEARCH_RESULTS);
        let context_lines = usize_arg(&call.arguments, "context_lines").unwrap_or(0);
        let globs = string_list_arg(&call.arguments, "glob");

        let search = match rg_search(&root, query, &globs, context_lines, max_results) {
            Ok(search) => search,
            Err(error) if is_not_found(&error) => {
                fallback_search(&root, query, &globs, max_results)?
            }
            Err(error) => return Err(error),
        };
        let content = if search.matches.is_empty() {
            format!("no matches for {query:?} under {}", root.display())
        } else {
            let mut lines = search
                .matches
                .iter()
                .map(|item| {
                    format!(
                        "{}:{}:{}: {}",
                        item.path.display(),
                        item.line,
                        item.column.unwrap_or(1),
                        item.text.trim_end()
                    )
                })
                .collect::<Vec<_>>();
            if search.truncated {
                lines.push(format!(
                    "[truncated after {} matches]",
                    search.matches.len()
                ));
            }
            lines.join("\n")
        };
        store.append_event(
            &session.id,
            "file.search",
            json!({
                "tool_call_id": call.id,
                "query": query,
                "path": root.display().to_string(),
                "matches": search.matches.len(),
                "truncated": search.truncated,
            }),
        )?;
        Ok(FileToolResult {
            content: Value::String(content),
        })
    })
}

pub(crate) fn list_files(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "list_files", || {
        let root = optional_path(session, &call.arguments, "path")?
            .unwrap_or_else(|| PathBuf::from(&session.cwd));
        let query = call
            .arguments
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let max_results =
            usize_arg(&call.arguments, "max_results").unwrap_or(DEFAULT_MAX_LIST_RESULTS);
        let include_hidden = call
            .arguments
            .get("include_hidden")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let include_dirs = call
            .arguments
            .get("include_dirs")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut files = Vec::new();
        let walker = WalkBuilder::new(&root)
            .hidden(!include_hidden)
            .git_ignore(true)
            .git_exclude(true)
            .ignore(true)
            .build();
        for entry in walker {
            let entry = entry?;
            let file_type = entry.file_type();
            let is_dir = file_type.map(|kind| kind.is_dir()).unwrap_or(false);
            if is_dir && !include_dirs {
                continue;
            }
            if file_type.map(|kind| kind.is_file()).unwrap_or(false) || (include_dirs && is_dir) {
                let path = entry.path();
                let display = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .display()
                    .to_string();
                if display.is_empty() || !matches_path_query(&display, &query) {
                    continue;
                }
                files.push(display);
                if files.len() >= max_results {
                    break;
                }
            }
        }
        files.sort();
        let truncated = files.len() >= max_results;
        let mut content = files.join("\n");
        if truncated {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&format!("[truncated after {} paths]", files.len()));
        }
        store.append_event(
            &session.id,
            "file.list",
            json!({
                "tool_call_id": call.id,
                "path": root.display().to_string(),
                "query": query,
                "count": files.len(),
                "truncated": truncated,
            }),
        )?;
        Ok(FileToolResult {
            content: Value::String(content),
        })
    })
}

pub(crate) fn view_image(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    can_request_original_detail: bool,
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "view_image", || {
        let path = required_path(session, &call.arguments)?;
        let requested_detail = call
            .arguments
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("high");
        if !matches!(requested_detail, "high" | "original") {
            bail!(
                "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{requested_detail}`"
            );
        }
        let detail = if requested_detail == "original" && can_request_original_detail {
            "original"
        } else {
            "high"
        };
        let mode = if detail == "original" {
            PromptImageMode::Original
        } else {
            PromptImageMode::ResizeToFit
        };
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let prompt_image = load_for_prompt_bytes(&path, bytes, mode).with_context(|| {
            format!(
                "view_image cannot inline {}: unsupported or invalid image bytes",
                path.display()
            )
        })?;
        if prompt_image.bytes.len() > MAX_INLINE_LOCAL_IMAGE_BYTES {
            bail!(
                "view_image cannot inline {}: image is {} bytes after normalization, above the {} byte inline limit",
                path.display(),
                prompt_image.bytes.len(),
                MAX_INLINE_LOCAL_IMAGE_BYTES
            );
        }
        let mime = prompt_image.mime;
        let image = json!({
            "path": path.display().to_string(),
            "mime_type": mime,
            "detail": detail,
            "bytes": prompt_image.bytes.len(),
            "width": prompt_image.width,
            "height": prompt_image.height,
        });
        let event = store.append_event(
            &session.id,
            "tool.image",
            json!({
                "name": "view_image",
                "tool_call_id": call.id,
                "image": image,
            }),
        )?;
        store.record_artifact(
            &session.id,
            Some(event.seq),
            "image",
            &path,
            Some(mime),
            image.clone(),
        )?;
        Ok(FileToolResult {
            content: Value::Array(vec![json!({
                "type": "input_image",
                "image_url": prompt_image.into_data_url(),
                "detail": detail,
            })]),
        })
    })
}

pub(crate) fn apply_patch_tool(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "apply_patch", || {
        let patch = patch_arg(&call.arguments)?;
        store.append_event(
            &session.id,
            "patch.started",
            json!({
                "tool_call_id": call.id,
                "lines": patch.lines().count(),
            }),
        )?;
        let cwd = PathBuf::from(&session.cwd);
        let ops = match parse_patch(patch) {
            Ok(ops) => ops,
            Err(error) => {
                emit_patch_finished(
                    store,
                    session,
                    call,
                    PatchFinish {
                        status: "failed",
                        success: false,
                        stdout: "",
                        stderr: &error.to_string(),
                        planned_changes: Vec::new(),
                        committed_changes: &[],
                        committed_delta_exact: true,
                    },
                )?;
                return Err(error);
            }
        };
        if let Err(error) = verify_patch_operations(&cwd, &ops) {
            emit_patch_finished(
                store,
                session,
                call,
                PatchFinish {
                    status: "failed",
                    success: false,
                    stdout: "",
                    stderr: &error.to_string(),
                    planned_changes: planned_patch_changes_payload(&ops),
                    committed_changes: &[],
                    committed_delta_exact: true,
                },
            )?;
            return Err(error);
        }
        let planned_changes = planned_patch_changes_payload(&ops);
        let changes = match apply_patch_operations(&cwd, ops, |change| {
            store.append_event(
                &session.id,
                "patch.file_changed",
                json!({
                    "tool_call_id": call.id,
                    "path": change.path.display().to_string(),
                    "kind": change.kind,
                    "move_path": change.move_path.as_ref().map(|path| path.display().to_string()),
                }),
            )?;
            Ok(())
        }) {
            Ok(changes) => changes,
            Err(error) => {
                emit_patch_finished(
                    store,
                    session,
                    call,
                    PatchFinish {
                        status: "failed",
                        success: false,
                        stdout: "",
                        stderr: &error.source.to_string(),
                        planned_changes,
                        committed_changes: error.committed_changes(),
                        committed_delta_exact: true,
                    },
                )?;
                return Err(error.into());
            }
        };
        let mut lines = vec!["Success. Updated the following files:".to_string()];
        for change in &changes {
            let marker = match change.kind {
                "added" => "A",
                "deleted" => "D",
                _ => "M",
            };
            let line = format!("{marker} {}", change.display_path.display());
            lines.push(line);
        }
        let content = format!("{}\n", lines.join("\n"));
        emit_patch_finished(
            store,
            session,
            call,
            PatchFinish {
                status: "completed",
                success: true,
                stdout: &content,
                stderr: "",
                planned_changes,
                committed_changes: &changes,
                committed_delta_exact: true,
            },
        )?;
        Ok(FileToolResult {
            content: Value::String(content),
        })
    })
}

fn run_file_tool(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    name: &str,
    run: impl FnOnce() -> Result<FileToolResult>,
) -> Result<FileToolResult> {
    store.append_event(
        &session.id,
        "tool.started",
        json!({
            "name": name,
            "tool_call_id": call.id,
            "arguments": call.arguments,
        }),
    )?;
    match run() {
        Ok(result) => {
            store.append_event(
                &session.id,
                "tool.finished",
                json!({
                    "name": name,
                    "tool_call_id": call.id,
                }),
            )?;
            Ok(result)
        }
        Err(error) => {
            store.append_event(
                &session.id,
                "tool.failed",
                json!({
                    "name": name,
                    "tool_call_id": call.id,
                    "error": error.to_string(),
                }),
            )?;
            Err(error)
        }
    }
}

#[derive(Debug)]
struct SearchResult {
    matches: Vec<SearchMatch>,
    truncated: bool,
}

#[derive(Debug)]
struct SearchMatch {
    path: PathBuf,
    line: u64,
    column: Option<u64>,
    text: String,
}

fn rg_search(
    root: &Path,
    query: &str,
    globs: &[String],
    context_lines: usize,
    max_results: usize,
) -> Result<SearchResult> {
    let mut command = Command::new(ripgrep_command_path());
    command
        .arg("--json")
        .arg("--line-number")
        .arg("--column")
        .arg("--color")
        .arg("never");
    if context_lines > 0 {
        command.arg("-C").arg(context_lines.to_string());
    }
    for glob in globs {
        command.arg("--glob").arg(glob);
    }
    command.arg(query).arg(root);
    apply_agent_tool_path_to_command(&mut command);
    let output = command.output().context("run rg")?;
    if !output.status.success() && output.status.code() != Some(1) {
        bail!(
            "rg failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut matches = Vec::new();
    let mut truncated = false;
    for line in stdout.lines() {
        let value: Value = serde_json::from_str(line).with_context(|| "parse rg json")?;
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "match" && kind != "context" {
            continue;
        }
        let Some(data) = value.get("data") else {
            continue;
        };
        let Some(path) = data
            .get("path")
            .and_then(|path| path.get("text"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let text = data
            .get("lines")
            .and_then(|lines| lines.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
        let column = data
            .get("submatches")
            .and_then(Value::as_array)
            .and_then(|matches| matches.first())
            .and_then(|item| item.get("start"))
            .and_then(Value::as_u64)
            .map(|start| start + 1);
        if matches.len() >= max_results {
            truncated = true;
            break;
        }
        matches.push(SearchMatch {
            path: PathBuf::from(path),
            line: line_number,
            column,
            text,
        });
    }
    Ok(SearchResult { matches, truncated })
}

fn fallback_search(
    root: &Path,
    query: &str,
    globs: &[String],
    max_results: usize,
) -> Result<SearchResult> {
    let mut matches = Vec::new();
    let mut truncated = false;
    let query_lower = query.to_lowercase();
    for entry in WalkBuilder::new(root).hidden(true).build() {
        let entry = entry?;
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let path = entry.path();
        if !globs.is_empty() && !globs.iter().any(|glob| simple_glob_match(path, glob)) {
            continue;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            let Some(column) = line.to_lowercase().find(&query_lower) else {
                continue;
            };
            if matches.len() >= max_results {
                truncated = true;
                return Ok(SearchResult { matches, truncated });
            }
            matches.push(SearchMatch {
                path: path.to_path_buf(),
                line: (index + 1) as u64,
                column: Some((column + 1) as u64),
                text: line.to_string(),
            });
        }
    }
    Ok(SearchResult { matches, truncated })
}

#[derive(Clone, Debug)]
struct AppliedChange {
    path: PathBuf,
    old_display_path: PathBuf,
    display_path: PathBuf,
    kind: &'static str,
    move_path: Option<PathBuf>,
    overwritten_move_content: Option<String>,
    old_content: Option<String>,
    new_content: Option<String>,
}

#[derive(Debug)]
struct TurnDiffSnapshot {
    unified_diff: String,
    changed_files: usize,
    exact: bool,
    should_emit: bool,
}

#[derive(Debug)]
struct TurnDiffTracker {
    valid: bool,
    display_root: PathBuf,
    baseline_by_path: HashMap<PathBuf, String>,
    current_by_path: HashMap<PathBuf, String>,
    origin_by_current_path: HashMap<PathBuf, PathBuf>,
}

impl TurnDiffTracker {
    fn with_display_root(display_root: PathBuf) -> Self {
        Self {
            valid: true,
            display_root,
            baseline_by_path: HashMap::new(),
            current_by_path: HashMap::new(),
            origin_by_current_path: HashMap::new(),
        }
    }

    fn track_delta(&mut self, changes: &[AppliedChange], exact: bool) -> TurnDiffSnapshot {
        let previous_diff = self.get_unified_diff();
        if exact {
            for change in changes {
                self.apply_change(change);
            }
        } else {
            self.invalidate();
        }
        let current_diff = self.get_unified_diff();
        let changed_files = current_diff.as_ref().map_or(0, |(_, count)| *count);
        TurnDiffSnapshot {
            unified_diff: current_diff.map(|(diff, _)| diff).unwrap_or_default(),
            changed_files,
            exact: self.valid,
            should_emit: !changes.is_empty() && (previous_diff.is_some() || changed_files > 0),
        }
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }

    fn apply_change(&mut self, change: &AppliedChange) {
        match change.kind {
            "added" => self.apply_add(
                &change.path,
                change.new_content.as_deref().unwrap_or_default(),
                change.old_content.as_deref(),
            ),
            "deleted" => self.apply_delete(
                &change.path,
                change.old_content.as_deref().unwrap_or_default(),
            ),
            "moved" => self.apply_update(
                &change.path,
                change.move_path.as_deref(),
                change.old_content.as_deref().unwrap_or_default(),
                change.overwritten_move_content.as_deref(),
                change.new_content.as_deref().unwrap_or_default(),
            ),
            _ => self.apply_update(
                &change.path,
                None,
                change.old_content.as_deref().unwrap_or_default(),
                None,
                change.new_content.as_deref().unwrap_or_default(),
            ),
        }
    }

    fn apply_add(&mut self, path: &Path, content: &str, overwritten_content: Option<&str>) {
        self.origin_by_current_path.remove(path);
        if !self.current_by_path.contains_key(path) && !self.baseline_by_path.contains_key(path) {
            if let Some(overwritten_content) = overwritten_content {
                self.baseline_by_path
                    .insert(path.to_path_buf(), overwritten_content.to_string());
            }
        }
        self.current_by_path
            .insert(path.to_path_buf(), content.to_string());
    }

    fn apply_delete(&mut self, path: &Path, content: &str) {
        if self.current_by_path.remove(path).is_none() && !self.baseline_by_path.contains_key(path)
        {
            self.baseline_by_path
                .insert(path.to_path_buf(), content.to_string());
        }
        self.origin_by_current_path.remove(path);
    }

    fn apply_update(
        &mut self,
        source_path: &Path,
        move_path: Option<&Path>,
        old_content: &str,
        overwritten_move_content: Option<&str>,
        new_content: &str,
    ) {
        if !self.current_by_path.contains_key(source_path)
            && !self.baseline_by_path.contains_key(source_path)
        {
            self.baseline_by_path
                .insert(source_path.to_path_buf(), old_content.to_string());
        }

        match move_path {
            Some(dest_path) => {
                if !self.current_by_path.contains_key(dest_path)
                    && !self.baseline_by_path.contains_key(dest_path)
                {
                    if let Some(overwritten_move_content) = overwritten_move_content {
                        self.baseline_by_path.insert(
                            dest_path.to_path_buf(),
                            overwritten_move_content.to_string(),
                        );
                    }
                }
                let origin = self
                    .origin_by_current_path
                    .remove(source_path)
                    .unwrap_or_else(|| source_path.to_path_buf());
                self.current_by_path.remove(source_path);
                self.current_by_path
                    .insert(dest_path.to_path_buf(), new_content.to_string());
                self.origin_by_current_path.remove(dest_path);
                if dest_path != origin.as_path() {
                    self.origin_by_current_path
                        .insert(dest_path.to_path_buf(), origin);
                }
            }
            None => {
                self.current_by_path
                    .insert(source_path.to_path_buf(), new_content.to_string());
            }
        }
    }

    fn get_unified_diff(&self) -> Option<(String, usize)> {
        if !self.valid {
            return None;
        }

        let rename_pairs = self.rename_pairs();
        let paired_destinations = rename_pairs.values().cloned().collect::<HashSet<_>>();
        let mut handled = HashSet::new();
        let mut paths = self
            .baseline_by_path
            .keys()
            .chain(self.current_by_path.keys())
            .cloned()
            .collect::<Vec<_>>();
        paths.sort_by_key(|path| self.display_path(path));
        paths.dedup();

        let mut aggregated = String::new();
        let mut changed_files = 0;
        for path in paths {
            if !handled.insert(path.clone()) {
                continue;
            }
            if paired_destinations.contains(&path) {
                continue;
            }

            let diff = if let Some(dest) = rename_pairs.get(&path) {
                handled.insert(dest.clone());
                self.render_rename_diff(&path, dest)
            } else {
                self.render_path_diff(&path)
            };
            if let Some(diff) = diff {
                changed_files += 1;
                aggregated.push_str(&diff);
                if !aggregated.ends_with('\n') {
                    aggregated.push('\n');
                }
            }
        }

        (!aggregated.is_empty()).then_some((aggregated, changed_files))
    }

    fn rename_pairs(&self) -> HashMap<PathBuf, PathBuf> {
        self.origin_by_current_path
            .iter()
            .filter_map(|(dest_path, origin_path)| {
                if dest_path == origin_path
                    || self.current_by_path.contains_key(origin_path)
                    || !self.current_by_path.contains_key(dest_path)
                    || !self.baseline_by_path.contains_key(origin_path)
                    || self.baseline_by_path.contains_key(dest_path)
                {
                    return None;
                }
                Some((origin_path.clone(), dest_path.clone()))
            })
            .collect()
    }

    fn render_path_diff(&self, path: &Path) -> Option<String> {
        self.render_diff(
            path,
            self.baseline_by_path.get(path).map(String::as_str),
            path,
            self.current_by_path.get(path).map(String::as_str),
        )
    }

    fn render_rename_diff(&self, source_path: &Path, dest_path: &Path) -> Option<String> {
        self.render_diff(
            source_path,
            self.baseline_by_path.get(source_path).map(String::as_str),
            dest_path,
            self.current_by_path.get(dest_path).map(String::as_str),
        )
    }

    fn render_diff(
        &self,
        left_path: &Path,
        left_content: Option<&str>,
        right_path: &Path,
        right_content: Option<&str>,
    ) -> Option<String> {
        render_unified_file_diff(
            &self.display_path(left_path),
            left_content,
            &self.display_path(right_path),
            right_content,
        )
    }

    fn display_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.display_root)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/")
    }
}

fn turn_diff_trackers() -> &'static Mutex<HashMap<String, TurnDiffTracker>> {
    TURN_DIFF_TRACKERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn track_session_turn_diff(
    session_id: &str,
    cwd: &Path,
    changes: &[AppliedChange],
    exact: bool,
) -> TurnDiffSnapshot {
    let mut trackers = turn_diff_trackers()
        .lock()
        .expect("turn diff tracker mutex poisoned");
    trackers
        .entry(session_id.to_string())
        .or_insert_with(|| TurnDiffTracker::with_display_root(cwd.to_path_buf()))
        .track_delta(changes, exact)
}

#[derive(Debug)]
struct PatchApplyError {
    source: anyhow::Error,
    committed_changes: Vec<AppliedChange>,
}

impl PatchApplyError {
    fn new(source: anyhow::Error, committed_changes: Vec<AppliedChange>) -> Self {
        Self {
            source,
            committed_changes,
        }
    }

    fn committed_changes(&self) -> &[AppliedChange] {
        &self.committed_changes
    }
}

impl fmt::Display for PatchApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#}", self.source)?;
        if !self.committed_changes.is_empty() {
            let files = self
                .committed_changes
                .iter()
                .map(|change| change.display_path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            write!(
                f,
                " ({} file(s) were already updated before the failure: {files})",
                self.committed_changes.len()
            )?;
        }
        Ok(())
    }
}

impl StdError for PatchApplyError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        Some(self.source.as_ref())
    }
}

struct PatchFinish<'a> {
    status: &'static str,
    success: bool,
    stdout: &'a str,
    stderr: &'a str,
    planned_changes: Vec<Value>,
    committed_changes: &'a [AppliedChange],
    committed_delta_exact: bool,
}

fn emit_patch_finished(
    store: &Store,
    session: &SessionMeta,
    call: &ToolCall,
    finish: PatchFinish<'_>,
) -> Result<()> {
    let unified_diff = render_applied_changes_unified_diff(finish.committed_changes);
    let (unified_diff, diff_truncated) = unified_diff
        .as_deref()
        .map(|diff| truncate_chars(diff, MAX_TURN_DIFF_CHARS))
        .unwrap_or_else(|| (String::new(), false));
    store.append_event(
        &session.id,
        "patch.finished",
        json!({
            "tool_call_id": call.id,
            "status": finish.status,
            "success": finish.success,
            "stdout": finish.stdout,
            "stderr": finish.stderr,
            "changed_files": finish.committed_changes.len(),
            "changes": finish.planned_changes,
            "committed_changes": applied_changes_payload(finish.committed_changes),
            "committed_files": applied_changes_payload(finish.committed_changes),
            "committed_delta_exact": finish.committed_delta_exact,
            "unified_diff": unified_diff.clone(),
            "diff_truncated": diff_truncated,
        }),
    )?;
    if !finish.committed_changes.is_empty() {
        let cumulative = track_session_turn_diff(
            &session.id,
            Path::new(&session.cwd),
            finish.committed_changes,
            finish.committed_delta_exact,
        );
        if !cumulative.should_emit {
            return Ok(());
        }
        let (cumulative_diff, cumulative_diff_truncated) =
            truncate_chars(&cumulative.unified_diff, MAX_TURN_DIFF_CHARS);
        store.append_event(
            &session.id,
            "turn.diff",
            json!({
                "source": "apply_patch",
                "tool_call_id": call.id,
                "changed_files": cumulative.changed_files,
                "patch_changed_files": finish.committed_changes.len(),
                "changes": applied_changes_payload(finish.committed_changes),
                "cumulative": true,
                "exact": cumulative.exact,
                "committed_delta_exact": finish.committed_delta_exact,
                "unified_diff": cumulative_diff,
                "diff_truncated": cumulative_diff_truncated,
            }),
        )?;
    }
    Ok(())
}

#[derive(Debug)]
enum PatchOperation {
    Add {
        path: String,
        content: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_path: Option<String>,
        hunks: Vec<PatchHunk>,
    },
}

#[derive(Debug)]
struct PatchHunk {
    context: Option<String>,
    old: Vec<String>,
    new: Vec<String>,
    end_of_file: bool,
}

fn parse_patch(patch: &str) -> Result<Vec<PatchOperation>> {
    let lines = patch_lines(patch);
    if lines.first().map(|line| line.trim()) != Some("*** Begin Patch") {
        bail!("patch must start with *** Begin Patch");
    }
    if lines.last().map(|line| line.trim()) != Some("*** End Patch") {
        bail!("patch must end with *** End Patch");
    }
    let mut index = 1;
    if let Some(line) = lines.get(index) {
        if let Some(environment_id) = line
            .trim_start()
            .strip_prefix("*** Environment ID: ")
            .map(str::trim)
        {
            if environment_id.is_empty() {
                bail!("apply_patch environment_id cannot be empty");
            }
            index += 1;
        }
    }
    let mut ops = Vec::new();
    while index < lines.len().saturating_sub(1) {
        let line = lines[index].trim();
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = Vec::new();
            while index < lines.len() {
                let line = lines[index];
                if line.trim_start().starts_with("*** ") {
                    break;
                }
                let Some(added) = line.strip_prefix('+') else {
                    bail!("add file lines must start with +");
                };
                content.push(added.to_string());
                index += 1;
            }
            ops.push(PatchOperation::Add {
                path: path.to_string(),
                content: lines_to_text(&content, true),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            ops.push(PatchOperation::Delete {
                path: path.to_string(),
            });
            index += 1;
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut move_path = None;
            if index < lines.len() {
                if let Some(target) = lines[index].trim().strip_prefix("*** Move to: ") {
                    move_path = Some(target.to_string());
                    index += 1;
                }
            }
            let mut hunks = Vec::new();
            while index < lines.len() {
                let line = lines[index];
                if line.trim().is_empty() {
                    index += 1;
                    continue;
                }
                if line.trim().starts_with("*** ") {
                    break;
                }
                let allow_missing_context = hunks.is_empty();
                let context = if line == "@@" {
                    index += 1;
                    None
                } else if let Some(context) = line.strip_prefix("@@ ") {
                    index += 1;
                    Some(context.to_string())
                } else if allow_missing_context {
                    None
                } else {
                    bail!("update hunk must start with @@");
                };
                if index >= lines.len() {
                    bail!("update hunk does not contain any lines");
                }
                let mut old = Vec::new();
                let mut new = Vec::new();
                let mut end_of_file = false;
                let mut parsed_lines = 0usize;
                while index < lines.len() {
                    let line = lines[index];
                    let trimmed = line.trim();
                    if line == "*** End of File" {
                        if parsed_lines == 0 {
                            bail!("update hunk does not contain any lines");
                        }
                        end_of_file = true;
                        index += 1;
                        break;
                    }
                    if trimmed.starts_with("@@") || trimmed.starts_with("*** ") {
                        break;
                    }
                    match line.chars().next() {
                        None => {
                            old.push(String::new());
                            new.push(String::new());
                        }
                        Some(marker @ (' ' | '-' | '+')) => {
                            let text = line[marker.len_utf8()..].to_string();
                            match marker {
                                ' ' => {
                                    old.push(text.clone());
                                    new.push(text);
                                }
                                '-' => old.push(text),
                                '+' => new.push(text),
                                _ => unreachable!(),
                            }
                        }
                        Some(_) if parsed_lines > 0 => break,
                        Some(_) => bail!("update hunk lines must start with space, -, or +"),
                    }
                    index += 1;
                    parsed_lines += 1;
                }
                if parsed_lines == 0 {
                    bail!("update hunk does not contain any lines");
                }
                hunks.push(PatchHunk {
                    context,
                    old,
                    new,
                    end_of_file,
                });
            }
            if hunks.is_empty() {
                bail!("update file requires at least one hunk");
            }
            ops.push(PatchOperation::Update {
                path: path.to_string(),
                move_path,
                hunks,
            });
        } else {
            bail!("unknown patch directive: {line}");
        }
    }
    if ops.is_empty() {
        bail!("No files were modified.");
    }
    Ok(ops)
}

fn patch_lines(patch: &str) -> Vec<&str> {
    let lines = patch.trim().lines().collect::<Vec<_>>();
    if lines.len() >= 3
        && heredoc_start(lines[0].trim())
        && lines.last().is_some_and(|line| line.trim() == "EOF")
    {
        return lines[1..lines.len() - 1].to_vec();
    }
    lines
}

fn heredoc_start(line: &str) -> bool {
    matches!(line, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
}

fn apply_patch_operations(
    cwd: &Path,
    ops: Vec<PatchOperation>,
    mut on_change: impl FnMut(&AppliedChange) -> Result<()>,
) -> std::result::Result<Vec<AppliedChange>, PatchApplyError> {
    if ops.is_empty() {
        return Err(PatchApplyError::new(
            anyhow::anyhow!("No files were modified."),
            Vec::new(),
        ));
    }
    let mut changes = Vec::new();
    for op in ops {
        let change = apply_patch_operation(cwd, op)
            .map_err(|error| PatchApplyError::new(error, changes.clone()))?;
        changes.push(change);
        on_change(changes.last().expect("just pushed change"))
            .map_err(|error| PatchApplyError::new(error, changes.clone()))?;
    }
    Ok(changes)
}

fn applied_changes_payload(changes: &[AppliedChange]) -> Vec<Value> {
    changes
        .iter()
        .map(|change| {
            json!({
                "path": change.path.display().to_string(),
                "display_path": change.display_path.display().to_string(),
                "kind": change.kind,
                "move_path": change.move_path.as_ref().map(|path| path.display().to_string()),
            })
        })
        .collect()
}

fn planned_patch_changes_payload(ops: &[PatchOperation]) -> Vec<Value> {
    ops.iter()
        .map(|op| match op {
            PatchOperation::Add { path, .. } => json!({
                "path": path,
                "kind": "added",
                "move_path": Value::Null,
            }),
            PatchOperation::Delete { path } => json!({
                "path": path,
                "kind": "deleted",
                "move_path": Value::Null,
            }),
            PatchOperation::Update {
                path, move_path, ..
            } => json!({
                "path": path,
                "kind": if move_path.is_some() { "moved" } else { "modified" },
                "move_path": move_path,
            }),
        })
        .collect()
}

fn verify_patch_operations(cwd: &Path, ops: &[PatchOperation]) -> Result<()> {
    if ops.is_empty() {
        bail!("No files were modified.");
    }
    for op in ops {
        match op {
            PatchOperation::Add { path, .. } => {
                let path = resolve_patch_path(cwd, path)?;
                if let Ok(metadata) = fs::metadata(&path) {
                    if !metadata.is_file() {
                        bail!(
                            "Failed to write file {}: path is a directory",
                            path.display()
                        );
                    }
                }
            }
            PatchOperation::Delete { path } => {
                let path = resolve_patch_path(cwd, path)?;
                fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read {}", path.display()))?;
            }
            PatchOperation::Update {
                path,
                move_path,
                hunks,
            } => {
                let path = resolve_patch_path(cwd, path)?;
                if let Some(move_path) = move_path {
                    resolve_patch_path(cwd, move_path)?;
                }
                let original = fs::read_to_string(&path)
                    .with_context(|| format!("Failed to read file to update {}", path.display()))?;
                let original_lines = split_patch_lines(&original);
                compute_replacements(&original_lines, &path, hunks)?;
            }
        }
    }
    Ok(())
}

fn apply_patch_operation(cwd: &Path, op: PatchOperation) -> Result<AppliedChange> {
    match op {
        PatchOperation::Add { path, content } => {
            let display_path = PathBuf::from(&path);
            let path = resolve_patch_path(cwd, &path)?;
            let old_content = fs::read_to_string(&path).ok();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::write(&path, &content)
                .with_context(|| format!("Failed to write file {}", path.display()))?;
            Ok(AppliedChange {
                path,
                old_display_path: display_path.clone(),
                display_path,
                kind: "added",
                move_path: None,
                overwritten_move_content: None,
                old_content,
                new_content: Some(content),
            })
        }
        PatchOperation::Delete { path } => {
            let display_path = PathBuf::from(&path);
            let path = resolve_patch_path(cwd, &path)?;
            let old_content = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete file {}", path.display()))?;
            Ok(AppliedChange {
                path,
                old_display_path: display_path.clone(),
                display_path,
                kind: "deleted",
                move_path: None,
                overwritten_move_content: None,
                old_content: Some(old_content),
                new_content: None,
            })
        }
        PatchOperation::Update {
            path,
            move_path,
            hunks,
        } => {
            let display_path = PathBuf::from(move_path.as_deref().unwrap_or(&path));
            let old_display_path = PathBuf::from(&path);
            let path = resolve_patch_path(cwd, &path)?;
            let move_path = move_path
                .map(|target| resolve_patch_path(cwd, &target))
                .transpose()?;
            let overwritten_move_content = move_path.as_ref().and_then(|target| {
                (target != &path)
                    .then(|| fs::read_to_string(target).ok())
                    .flatten()
            });
            let original = fs::read_to_string(&path)
                .with_context(|| format!("Failed to read file to update {}", path.display()))?;
            let original_lines = split_patch_lines(&original);
            let replacements = compute_replacements(&original_lines, &path, &hunks)?;
            let mut new_lines = apply_replacements(original_lines, &replacements);
            if !new_lines.last().is_some_and(String::is_empty) {
                new_lines.push(String::new());
            }
            let new_content = new_lines.join("\n");
            if let Some(target) = &move_path {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create {}", parent.display()))?;
                }
                fs::write(target, &new_content)
                    .with_context(|| format!("Failed to write file {}", target.display()))?;
                fs::remove_file(&path)
                    .with_context(|| format!("Failed to remove original {}", path.display()))?;
            } else {
                fs::write(&path, &new_content)
                    .with_context(|| format!("Failed to write file {}", path.display()))?;
            }
            Ok(AppliedChange {
                path,
                old_display_path,
                display_path,
                kind: if move_path.is_some() {
                    "moved"
                } else {
                    "modified"
                },
                move_path,
                overwritten_move_content,
                old_content: Some(original),
                new_content: Some(new_content),
            })
        }
    }
}

fn render_applied_changes_unified_diff(changes: &[AppliedChange]) -> Option<String> {
    let mut out = String::new();
    for change in changes {
        let left_path = change.display_old_path();
        let right_path = change.display_new_path();
        if let Some(diff) = render_unified_file_diff(
            &left_path,
            change.old_content.as_deref(),
            &right_path,
            change.new_content.as_deref(),
        ) {
            out.push_str(&diff);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

impl AppliedChange {
    fn display_old_path(&self) -> String {
        match self.kind {
            "moved" => self.old_display_path.display().to_string(),
            _ => self.display_path.display().to_string(),
        }
        .replace('\\', "/")
    }

    fn display_new_path(&self) -> String {
        self.display_path.display().to_string().replace('\\', "/")
    }
}

fn render_unified_file_diff(
    left_path: &str,
    left_content: Option<&str>,
    right_path: &str,
    right_content: Option<&str>,
) -> Option<String> {
    if left_content == right_content {
        return None;
    }
    let left_oid = left_content.map_or_else(
        || ZERO_OID.to_string(),
        |content| git_blob_oid(content.as_bytes()),
    );
    let right_oid = right_content.map_or_else(
        || ZERO_OID.to_string(),
        |content| git_blob_oid(content.as_bytes()),
    );
    let mut diff = format!("diff --git a/{left_path} b/{right_path}\n");
    match (left_content, right_content) {
        (None, Some(_)) => diff.push_str(&format!("new file mode {REGULAR_FILE_MODE}\n")),
        (Some(_), None) => diff.push_str(&format!("deleted file mode {REGULAR_FILE_MODE}\n")),
        (Some(_), Some(_)) => {}
        (None, None) => return None,
    }
    diff.push_str(&format!("index {left_oid}..{right_oid}\n"));
    let old_header = if left_content.is_some() {
        format!("a/{left_path}")
    } else {
        DEV_NULL.to_string()
    };
    let new_header = if right_content.is_some() {
        format!("b/{right_path}")
    } else {
        DEV_NULL.to_string()
    };
    let unified =
        similar::TextDiff::from_lines(left_content.unwrap_or(""), right_content.unwrap_or(""))
            .unified_diff()
            .context_radius(3)
            .header(&old_header, &new_header)
            .to_string();
    diff.push_str(&unified);
    Some(diff)
}

fn git_blob_oid(data: &[u8]) -> String {
    let header = format!("blob {}\0", data.len());
    let mut hasher = sha1::Sha1::new();
    hasher.update(header.as_bytes());
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

fn patch_arg(arguments: &Value) -> Result<&str> {
    if let Some(patch) = arguments.as_str() {
        return Ok(patch);
    }
    arguments
        .get("patch")
        .and_then(Value::as_str)
        .context("apply_patch requires patch")
}

fn required_path(session: &SessionMeta, arguments: &Value) -> Result<PathBuf> {
    let raw = arguments
        .get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("path is required")?;
    Ok(resolve_path(Path::new(&session.cwd), raw))
}

fn optional_path(session: &SessionMeta, arguments: &Value, key: &str) -> Result<Option<PathBuf>> {
    Ok(arguments
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(|raw| resolve_path(Path::new(&session.cwd), raw)))
}

fn resolve_path(cwd: &Path, raw: &str) -> PathBuf {
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_patch_path(cwd: &Path, raw: &str) -> Result<PathBuf> {
    let root = normalize_path(cwd);
    let path = normalize_path(&resolve_path(cwd, raw));
    if !path.starts_with(&root) {
        bail!(PATCH_REJECTED_OUTSIDE_PROJECT_REASON);
    }
    ensure_patch_path_does_not_touch_protected_metadata(&root, &path)?;
    ensure_real_path_stays_under_root(&root, &path)?;
    Ok(path)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn ensure_patch_path_does_not_touch_protected_metadata(root: &Path, path: &Path) -> Result<()> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let Some(Component::Normal(first_component)) = relative.components().next() else {
        return Ok(());
    };
    if PROTECTED_PATCH_METADATA_NAMES
        .iter()
        .any(|name| first_component == OsStr::new(name))
    {
        bail!(PATCH_REJECTED_OUTSIDE_PROJECT_REASON);
    }
    Ok(())
}

fn ensure_real_path_stays_under_root(root: &Path, path: &Path) -> Result<()> {
    let real_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if let Ok(real_path) = path.canonicalize() {
        if !real_path.starts_with(&real_root) {
            bail!(PATCH_REJECTED_OUTSIDE_PROJECT_REASON);
        }
        return Ok(());
    }

    let mut ancestor = path.parent();
    while let Some(parent) = ancestor {
        if let Ok(real_parent) = parent.canonicalize() {
            if !real_parent.starts_with(&real_root) {
                bail!(PATCH_REJECTED_OUTSIDE_PROJECT_REASON);
            }
            return Ok(());
        }
        ancestor = parent.parent();
    }
    Ok(())
}

fn usize_arg(arguments: &Value, key: &str) -> Option<usize> {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
}

fn string_list_arg(arguments: &Value, key: &str) -> Vec<String> {
    match arguments.get(key) {
        Some(Value::String(value)) if !value.trim().is_empty() => vec![value.to_string()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    if text.chars().count() <= max_chars {
        return (text.to_string(), false);
    }
    let mut out = text.chars().take(max_chars).collect::<String>();
    out.push_str("\n[truncated]");
    (out, true)
}

fn matches_path_query(path: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let path = path.to_lowercase();
    let query = query.to_lowercase();
    if path.contains(&query) {
        return true;
    }
    let mut chars = path.chars();
    query.chars().all(|needle| chars.any(|item| item == needle))
}

fn simple_glob_match(path: &Path, glob: &str) -> bool {
    let path = path.display().to_string();
    if let Some(suffix) = glob.strip_prefix("*.") {
        return path.ends_with(&format!(".{suffix}"));
    }
    path.contains(glob.trim_matches('*'))
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|source| source.downcast_ref::<io::Error>())
        .any(|error| error.kind() == io::ErrorKind::NotFound)
}

fn lines_to_text(lines: &[String], final_newline: bool) -> String {
    let mut text = lines.join("\n");
    if final_newline && !lines.is_empty() {
        text.push('\n');
    }
    text
}

fn split_patch_lines(text: &str) -> Vec<String> {
    let mut lines = text.split('\n').map(ToOwned::to_owned).collect::<Vec<_>>();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

fn compute_replacements(
    original_lines: &[String],
    path: &Path,
    hunks: &[PatchHunk],
) -> Result<Vec<(usize, usize, Vec<String>)>> {
    let mut replacements = Vec::new();
    let mut line_index = 0usize;

    for hunk in hunks {
        if let Some(context) = &hunk.context {
            if let Some(index) = seek_sequence(
                original_lines,
                std::slice::from_ref(context),
                line_index,
                false,
            ) {
                line_index = index + 1;
            } else {
                bail!("Failed to find context '{}' in {}", context, path.display());
            }
        }

        if hunk.old.is_empty() {
            let insertion_index = if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_index, 0, hunk.new.clone()));
            continue;
        }

        let mut pattern = hunk.old.as_slice();
        let mut new_slice = hunk.new.as_slice();
        let mut found = seek_sequence(original_lines, pattern, line_index, hunk.end_of_file);

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, hunk.end_of_file);
        }

        if let Some(start_index) = found {
            replacements.push((start_index, pattern.len(), new_slice.to_vec()));
            line_index = start_index + pattern.len();
        } else {
            bail!(
                "Failed to find expected lines in {}:\n{}",
                path.display(),
                hunk.old.join("\n")
            );
        }
    }

    replacements.sort_by(|(left, _, _), (right, _, _)| left.cmp(right));
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start_index, old_len, new_segment) in replacements.iter().rev() {
        for _ in 0..*old_len {
            if *start_index < lines.len() {
                lines.remove(*start_index);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_index + offset, new_line.clone());
        }
    }
    lines
}

fn seek_sequence(
    lines: &[String],
    needle: &[String],
    start: usize,
    end_of_file: bool,
) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(lines.len()));
    }
    if needle.len() > lines.len() {
        return None;
    }
    let search_start = if end_of_file && lines.len() >= needle.len() {
        lines.len() - needle.len()
    } else {
        start.min(lines.len())
    };
    for index in search_start..=lines.len().saturating_sub(needle.len()) {
        if lines[index..index + needle.len()] == *needle {
            return Some(index);
        }
    }
    for index in search_start..=lines.len().saturating_sub(needle.len()) {
        if needle
            .iter()
            .enumerate()
            .all(|(offset, line)| lines[index + offset].trim_end() == line.trim_end())
        {
            return Some(index);
        }
    }
    for index in search_start..=lines.len().saturating_sub(needle.len()) {
        if needle
            .iter()
            .enumerate()
            .all(|(offset, line)| lines[index + offset].trim() == line.trim())
        {
            return Some(index);
        }
    }
    for index in search_start..=lines.len().saturating_sub(needle.len()) {
        if needle.iter().enumerate().all(|(offset, line)| {
            normalize_common_punctuation(&lines[index + offset])
                == normalize_common_punctuation(line)
        }) {
            return Some(index);
        }
    }
    None
}

fn normalize_common_punctuation(text: &str) -> String {
    text.trim()
        .chars()
        .map(|ch| match ch {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use image::{DynamicImage, GenericImageView, ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;
    use tempfile::TempDir;

    fn test_session(tmp: &TempDir) -> (Store, SessionMeta) {
        let store = Store::open(tmp.path().join("state")).expect("store");
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = store.create_session(None, cwd).expect("session");
        (store, session)
    }

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_pixel(width, height, Rgba([255, 0, 0, 255]));
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode png");
        encoded.into_inner()
    }

    fn noisy_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = ImageBuffer::from_fn(width, height, |x, y| {
            let seed = x
                .wrapping_mul(1_103_515_245)
                .wrapping_add(y.wrapping_mul(12_345));
            Rgba([
                (seed & 0xff) as u8,
                ((seed >> 8) & 0xff) as u8,
                ((seed >> 16) & 0xff) as u8,
                255,
            ])
        });
        let mut encoded = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut encoded, ImageFormat::Png)
            .expect("encode noisy png");
        encoded.into_inner()
    }

    #[test]
    fn read_file_returns_numbered_range() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        fs::write(
            Path::new(&session.cwd).join("file.txt"),
            "one\ntwo\nthree\n",
        )
        .expect("write");
        let result = read_file(
            &store,
            &session,
            &ToolCall {
                id: "read_1".to_string(),
                name: "read_file".to_string(),
                namespace: None,
                arguments: json!({"path": "file.txt", "start_line": 2, "max_lines": 1}),
            },
        )
        .expect("read");
        assert!(result.content.as_str().expect("text").contains("two"));
    }

    #[test]
    fn apply_patch_add_update_delete_and_move() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("update.txt"), "hello\nworld\n").expect("write update");
        fs::write(cwd.join("move.txt"), "hello\nrust\n").expect("write move");
        fs::write(cwd.join("delete.txt"), "obsolete\n").expect("write delete");
        let patch = r#"*** Begin Patch
*** Add File: a.txt
+added
*** Update File: update.txt
@@
 hello
-world
+rust
*** Update File: move.txt
*** Move to: b.txt
@@
-hello
+hi
 rust
*** Delete File: delete.txt
*** End Patch"#;
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_1".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");
        assert_eq!(
            fs::read_to_string(cwd.join("a.txt")).expect("read add"),
            "added\n"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("update.txt")).expect("read update"),
            "hello\nrust\n"
        );
        assert!(!cwd.join("move.txt").exists());
        assert_eq!(
            fs::read_to_string(cwd.join("b.txt")).expect("read move"),
            "hi\nrust\n"
        );
        assert!(!cwd.join("delete.txt").exists());
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "patch.file_changed"));
        let finished = events
            .iter()
            .find(|event| event.event_type == "patch.finished")
            .expect("patch finished");
        assert_eq!(finished.payload["status"], "completed");
        assert_eq!(finished.payload["success"], true);
        assert_eq!(finished.payload["changed_files"], 4);
        assert_eq!(
            finished.payload["committed_changes"]
                .as_array()
                .expect("committed changes")
                .len(),
            4
        );
        assert_eq!(finished.payload["committed_delta_exact"], true);
        let patch_diff = finished.payload["unified_diff"]
            .as_str()
            .expect("patch unified diff");
        assert!(patch_diff.contains("diff --git a/a.txt b/a.txt"));
        assert!(patch_diff.contains("new file mode 100644"));
        assert!(patch_diff.contains("diff --git a/update.txt b/update.txt"));
        assert!(patch_diff.contains("-world"));
        assert!(patch_diff.contains("+rust"));
        assert!(patch_diff.contains("diff --git a/move.txt b/"));
        assert!(patch_diff.contains("b.txt"));
        assert!(patch_diff.contains("deleted file mode 100644"));
        let turn_diff = events
            .iter()
            .find(|event| event.event_type == "turn.diff")
            .expect("turn diff");
        assert_eq!(turn_diff.payload["source"], "apply_patch");
        assert_eq!(turn_diff.payload["changed_files"], 4);
        assert_eq!(turn_diff.payload["cumulative"], true);
        assert_eq!(turn_diff.payload["exact"], true);
        let cumulative_diff = turn_diff.payload["unified_diff"]
            .as_str()
            .expect("turn diff");
        assert_eq!(cumulative_diff.matches("diff --git").count(), 4);
        assert!(cumulative_diff.contains("diff --git a/a.txt b/a.txt"));
        assert!(cumulative_diff.contains("diff --git a/update.txt b/update.txt"));
        assert!(cumulative_diff.contains("diff --git a/move.txt b/b.txt"));
        assert!(cumulative_diff.contains("diff --git a/delete.txt b/delete.txt"));
    }

    #[test]
    fn apply_patch_turn_diff_accumulates_net_patch_changes_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_add".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Add File: a.txt\n+foo\n*** End Patch"}),
            },
        )
        .expect("add");
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_update".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n foo\n+bar\n*** End Patch"}),
            },
        )
        .expect("update");

        assert_eq!(
            fs::read_to_string(cwd.join("a.txt")).expect("read"),
            "foo\nbar\n"
        );
        let events = store.events_for_session(&session.id).expect("events");
        let turn_diffs = events
            .iter()
            .filter(|event| event.event_type == "turn.diff")
            .collect::<Vec<_>>();
        assert_eq!(turn_diffs.len(), 2);
        let cumulative = turn_diffs.last().expect("last diff");
        assert_eq!(cumulative.payload["source"], "apply_patch");
        assert_eq!(cumulative.payload["cumulative"], true);
        assert_eq!(cumulative.payload["exact"], true);
        assert_eq!(cumulative.payload["changed_files"], 1);
        let diff = cumulative.payload["unified_diff"].as_str().expect("diff");
        assert_eq!(diff.matches("diff --git").count(), 1);
        assert!(diff.contains("new file mode 100644"));
        assert!(diff.contains("+foo"));
        assert!(diff.contains("+bar"));
    }

    #[test]
    fn apply_patch_turn_diff_clears_when_net_diff_returns_to_baseline() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_add".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Add File: temp.txt\n+gone\n*** End Patch"}),
            },
        )
        .expect("add");
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_delete".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Delete File: temp.txt\n*** End Patch"}),
            },
        )
        .expect("delete");

        let events = store.events_for_session(&session.id).expect("events");
        let cumulative = events
            .iter()
            .filter(|event| event.event_type == "turn.diff")
            .next_back()
            .expect("last diff");
        assert_eq!(cumulative.payload["source"], "apply_patch");
        assert_eq!(cumulative.payload["cumulative"], true);
        assert_eq!(cumulative.payload["changed_files"], 0);
        assert_eq!(cumulative.payload["unified_diff"], "");
    }

    #[test]
    fn apply_patch_turn_diff_reset_starts_a_new_codex_style_turn() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);

        reset_turn_diff_tracker_for_session(&session.id, cwd);
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_add".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Add File: turn.txt\n+first\n*** End Patch"}),
            },
        )
        .expect("add");

        reset_turn_diff_tracker_for_session(&session.id, cwd);
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_delete".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Delete File: turn.txt\n*** End Patch"}),
            },
        )
        .expect("delete");

        let events = store.events_for_session(&session.id).expect("events");
        let cumulative = events
            .iter()
            .filter(|event| event.event_type == "turn.diff")
            .next_back()
            .expect("last diff");
        assert_eq!(cumulative.payload["source"], "apply_patch");
        assert_eq!(cumulative.payload["changed_files"], 1);
        let diff = cumulative.payload["unified_diff"].as_str().expect("diff");
        assert!(diff.contains("diff --git a/turn.txt b/turn.txt"));
        assert!(diff.contains("deleted file mode 100644"));
        assert!(diff.contains("-first"));
    }

    #[test]
    fn apply_patch_turn_diff_uses_configured_display_root() {
        let tmp = TempDir::new().expect("tmp");
        let store = Store::open(tmp.path().join("state")).expect("store");
        let repo = tmp.path().join("repo");
        let cwd = repo.join("sub");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = store.create_session(None, &cwd).expect("session");

        reset_turn_diff_tracker_for_session(&session.id, &repo);
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_add".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Add File: file.txt\n+rooted\n*** End Patch"}),
            },
        )
        .expect("add");

        let events = store.events_for_session(&session.id).expect("events");
        let cumulative = events
            .iter()
            .find(|event| event.event_type == "turn.diff")
            .expect("turn diff");
        let diff = cumulative.payload["unified_diff"].as_str().expect("diff");
        assert!(diff.contains("diff --git a/sub/file.txt b/sub/file.txt"));
    }

    #[test]
    fn apply_patch_turn_diff_handles_delete_readd_and_move_overwrite_edges() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("cycle.txt"), "before\n").expect("cycle");
        fs::write(cwd.join("from.txt"), "source\n").expect("from");
        fs::write(cwd.join("to.txt"), "dest\n").expect("to");

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_delete".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Delete File: cycle.txt\n*** End Patch"}),
            },
        )
        .expect("delete");
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_readd".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Add File: cycle.txt\n+after\n*** End Patch"}),
            },
        )
        .expect("readd");
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_move".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** Update File: from.txt\n*** Move to: to.txt\n@@\n-source\n+moved\n*** End Patch"}),
            },
        )
        .expect("move");

        let events = store.events_for_session(&session.id).expect("events");
        let cumulative = events
            .iter()
            .filter(|event| event.event_type == "turn.diff")
            .next_back()
            .expect("last diff");
        assert_eq!(cumulative.payload["changed_files"], 3);
        let diff = cumulative.payload["unified_diff"].as_str().expect("diff");
        assert!(diff.contains("diff --git a/cycle.txt b/cycle.txt"));
        assert!(diff.contains("-before"));
        assert!(diff.contains("+after"));
        assert!(diff.contains("diff --git a/from.txt b/from.txt"));
        assert!(diff.contains("deleted file mode 100644"));
        assert!(diff.contains("diff --git a/to.txt b/to.txt"));
        assert!(diff.contains("-dest"));
        assert!(diff.contains("+moved"));
    }

    #[test]
    fn apply_patch_verifies_entire_patch_before_writing() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("a.txt"), "one\n").expect("write a");
        fs::write(cwd.join("b.txt"), "two\n").expect("write b");
        let patch = r#"*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-missing
+TWO
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_fail".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(cwd.join("a.txt")).expect("read a"),
            "one\n"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("b.txt")).expect("read b"),
            "two\n"
        );
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events.iter().any(|event| event.event_type == "tool.failed"));
        assert!(!events
            .iter()
            .any(|event| event.event_type == "patch.file_changed"));
        let finished = events
            .iter()
            .find(|event| event.event_type == "patch.finished")
            .expect("patch finished");
        assert_eq!(finished.payload["status"], "failed");
        assert_eq!(finished.payload["success"], false);
        assert_eq!(finished.payload["changed_files"], 0);
        assert_eq!(
            finished.payload["changes"]
                .as_array()
                .expect("planned changes")
                .len(),
            2
        );
    }

    #[test]
    fn apply_patch_reports_committed_prefix_after_runtime_failure_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("not_a_dir"), "blocking parent\n").expect("write parent file");
        let patch = r#"*** Begin Patch
*** Add File: created.txt
+created
*** Add File: not_a_dir/child.txt
+child
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_partial_failure".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        let error = result.expect_err("patch should fail after first committed change");
        assert!(
            error.to_string().contains("already updated before the failure"),
            "partial committed files should be model-visible through the recovered error: {error:#}"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("created.txt")).expect("created file"),
            "created\n"
        );
        let events = store.events_for_session(&session.id).expect("events");
        let file_changes = events
            .iter()
            .filter(|event| event.event_type == "patch.file_changed")
            .collect::<Vec<_>>();
        assert_eq!(file_changes.len(), 1);
        assert_eq!(
            file_changes[0].payload["path"],
            json!(cwd.join("created.txt").display().to_string())
        );
        let finished = events
            .iter()
            .find(|event| event.event_type == "patch.finished")
            .expect("patch finished");
        assert_eq!(finished.payload["status"], "failed");
        assert_eq!(finished.payload["success"], false);
        assert_eq!(finished.payload["changed_files"], 1);
        assert_eq!(
            finished.payload["committed_changes"][0]["display_path"],
            "created.txt"
        );
        assert_eq!(finished.payload["committed_delta_exact"], true);
    }

    #[test]
    fn apply_patch_matches_codex_add_and_update_edge_semantics() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("duplicate.txt"), "old content\n").expect("write duplicate");
        fs::write(cwd.join("no_newline.txt"), "no newline at end").expect("write no newline");
        let patch = r#"*** Begin Patch
*** Add File: duplicate.txt
+new content
*** Update File: no_newline.txt
@@
-no newline at end
+first line
+second line
*** End Patch"#;

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_edges".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");

        assert_eq!(
            fs::read_to_string(cwd.join("duplicate.txt")).expect("read duplicate"),
            "new content\n"
        );
        assert_eq!(
            fs::read_to_string(cwd.join("no_newline.txt")).expect("read no newline"),
            "first line\nsecond line\n"
        );
    }

    #[test]
    fn apply_patch_accepts_codex_raw_patch_wrappers_and_missing_context() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("file.txt"), "import foo\n").expect("write file");
        let patch = r#"
<<'EOF'
  *** Begin Patch
*** Environment ID: local
*** Update File: file.txt
 import foo
+bar
*** End Patch
EOF
"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_raw_edges".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!(patch),
            },
        )
        .expect("patch");

        assert_eq!(
            fs::read_to_string(cwd.join("file.txt")).expect("read file"),
            "import foo\nbar\n"
        );
        let content = result.content.as_str().expect("content");
        assert_eq!(
            content,
            "Success. Updated the following files:\nM file.txt\n"
        );
    }

    #[test]
    fn apply_patch_honors_context_eof_and_pure_addition_semantics() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(
            cwd.join("target.txt"),
            "alpha\nneedle\nold one\nmiddle\nold two\ntail\n",
        )
        .expect("write target");
        let patch = r#"*** Begin Patch
*** Update File: target.txt
@@ needle
-old one
+new one
@@
+appended
@@
 old two
-tail
+tail updated
*** End of File
*** End Patch"#;

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_context_eof".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");

        assert_eq!(
            fs::read_to_string(cwd.join("target.txt")).expect("read target"),
            "alpha\nneedle\nnew one\nmiddle\nold two\ntail updated\nappended\n"
        );
    }

    #[test]
    fn apply_patch_preverifies_before_runtime_writes_like_codex_tool() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        let patch = r#"*** Begin Patch
*** Add File: created.txt
+hello
*** Update File: created.txt
@@
-hello
+changed
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_add_then_update".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result.is_err());
        assert!(!cwd.join("created.txt").exists());
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "patch.file_changed"));
    }

    #[test]
    fn apply_patch_accepts_crlf_and_outer_newlines_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        let patch =
            "\r\n*** Begin Patch\r\n*** Add File: crlf.txt\r\n+hello\r\n*** End Patch\r\n\r\n";

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_crlf".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");

        assert_eq!(
            fs::read_to_string(cwd.join("crlf.txt")).expect("read crlf"),
            "hello\n"
        );
    }

    #[test]
    fn apply_patch_rejects_empty_update_hunk_even_with_move() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::write(cwd.join("source.txt"), "content\n").expect("write source");
        let patch = r#"*** Begin Patch
*** Update File: source.txt
*** Move to: dest.txt
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_empty_update".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result.is_err());
        assert!(cwd.join("source.txt").exists());
        assert!(!cwd.join("dest.txt").exists());
    }

    #[test]
    fn apply_patch_rejects_empty_patch() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_empty".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": "*** Begin Patch\n*** End Patch"}),
            },
        );

        assert!(result
            .expect_err("empty patch should fail")
            .to_string()
            .contains("No files were modified."));
    }

    #[test]
    fn apply_patch_rejects_parent_path_outside_cwd() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let patch = r#"*** Begin Patch
*** Add File: ../outside/escape.txt
+outside
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_outside_parent".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result
            .expect_err("outside path should fail")
            .to_string()
            .contains(PATCH_REJECTED_OUTSIDE_PROJECT_REASON));
        assert!(!outside.join("escape.txt").exists());
        let events = store.events_for_session(&session.id).expect("events");
        assert!(!events
            .iter()
            .any(|event| event.event_type == "patch.file_changed"));
    }

    #[test]
    fn apply_patch_rejects_absolute_path_outside_cwd() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let outside = tmp.path().join("outside.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+outside\n*** End Patch",
            outside.display()
        );

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_outside_abs".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result
            .expect_err("outside absolute path should fail")
            .to_string()
            .contains(PATCH_REJECTED_OUTSIDE_PROJECT_REASON));
        assert!(!outside.exists());
    }

    #[test]
    fn apply_patch_allows_absolute_path_inside_cwd() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let inside = Path::new(&session.cwd).join("absolute-inside.txt");
        let patch = format!(
            "*** Begin Patch\n*** Add File: {}\n+inside\n*** End Patch",
            inside.display()
        );

        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_inside_abs".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        )
        .expect("inside absolute path should be allowed");

        assert_eq!(fs::read_to_string(inside).expect("read inside"), "inside\n");
    }

    #[test]
    fn apply_patch_rejects_protected_workspace_metadata_paths() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        fs::create_dir_all(cwd.join(".git")).expect(".git");
        fs::create_dir_all(cwd.join(".agents").join("skills")).expect(".agents");
        let cases = [
            ("patch_git_metadata", ".git/config"),
            ("patch_agents_metadata", ".agents/skills/example.md"),
            ("patch_browser_use_metadata", ".browser-use/config.toml"),
            ("patch_codex_metadata", ".codex/config.toml"),
        ];

        for (id, path) in cases {
            let patch = format!("*** Begin Patch\n*** Add File: {path}\n+blocked\n*** End Patch");
            let result = apply_patch_tool(
                &store,
                &session,
                &ToolCall {
                    id: id.to_string(),
                    name: "apply_patch".to_string(),
                    namespace: None,
                    arguments: json!({"patch": patch}),
                },
            );

            assert!(
                result
                    .expect_err("metadata path should fail")
                    .to_string()
                    .contains(PATCH_REJECTED_OUTSIDE_PROJECT_REASON),
                "{path} should be rejected"
            );
            assert!(!cwd.join(path).exists(), "{path} should not be written");
        }
    }

    #[cfg(unix)]
    #[test]
    fn apply_patch_rejects_symlink_escape() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let cwd = Path::new(&session.cwd);
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, cwd.join("link")).expect("symlink");
        let patch = r#"*** Begin Patch
*** Add File: link/escape.txt
+outside
*** End Patch"#;

        let result = apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_symlink_escape".to_string(),
                name: "apply_patch".to_string(),
                namespace: None,
                arguments: json!({"patch": patch}),
            },
        );

        assert!(result
            .expect_err("symlink escape should fail")
            .to_string()
            .contains(PATCH_REJECTED_OUTSIDE_PROJECT_REASON));
        assert!(!outside.join("escape.txt").exists());
    }

    #[test]
    fn resolve_path_does_not_rewrite_absolute_paths() {
        let tmp = TempDir::new().expect("tmp");
        let cwd = tmp.path().join("task-root").join("cwd");
        fs::create_dir_all(cwd.parent().unwrap().join("outputs")).expect("outputs");
        fs::create_dir_all(&cwd).expect("cwd");

        let result = resolve_path(&cwd, "/opt/runtime/result.txt");

        assert_eq!(result, PathBuf::from("/opt/runtime/result.txt"));
    }

    #[test]
    fn search_and_list_files_work() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        fs::write(Path::new(&session.cwd).join("alpha.rs"), "fn target() {}\n").expect("write");
        let search = search_files(
            &store,
            &session,
            &ToolCall {
                id: "search_1".to_string(),
                name: "search_files".to_string(),
                namespace: None,
                arguments: json!({"query": "target", "glob": "*.rs"}),
            },
        )
        .expect("search");
        assert!(search.content.as_str().expect("text").contains("target"));
        let listed = list_files(
            &store,
            &session,
            &ToolCall {
                id: "list_1".to_string(),
                name: "list_files".to_string(),
                namespace: None,
                arguments: json!({"query": "alpha"}),
            },
        )
        .expect("list");
        assert!(listed.content.as_str().expect("text").contains("alpha.rs"));
    }

    #[test]
    fn view_image_records_image_artifact() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let path = Path::new(&session.cwd).join("pixel.png");
        fs::write(&path, png_bytes(1, 1)).expect("write png");
        let result = view_image(
            &store,
            &session,
            &ToolCall {
                id: "image_1".to_string(),
                name: "view_image".to_string(),
                namespace: None,
                arguments: json!({"path": "pixel.png", "detail": "high"}),
            },
            false,
        )
        .expect("view image");
        assert!(result
            .content
            .as_array()
            .expect("content")
            .iter()
            .any(|part| { part.get("type").and_then(Value::as_str) == Some("input_image") }));
        let artifacts = store.artifacts_for_session(&session.id).expect("artifacts");
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].kind, "image");
    }

    #[test]
    fn view_image_high_detail_resizes_large_images() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let path = Path::new(&session.cwd).join("wide.png");
        fs::write(&path, png_bytes(2304, 864)).expect("write png");

        let result = view_image(
            &store,
            &session,
            &ToolCall {
                id: "image_1".to_string(),
                name: "view_image".to_string(),
                namespace: None,
                arguments: json!({"path": "wide.png", "detail": "high"}),
            },
            true,
        )
        .expect("view image");

        let image_url = result.content[0]["image_url"].as_str().expect("image url");
        let (prefix, encoded) = image_url.split_once(',').expect("data url");
        assert_eq!(prefix, "data:image/png;base64");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode image");
        let resized = image::load_from_memory(&decoded).expect("load resized");
        assert_eq!(resized.dimensions(), (2048, 768));
        let events = store.events_for_session(&session.id).expect("events");
        let image_event = events
            .iter()
            .find(|event| event.event_type == "tool.image")
            .expect("tool image event");
        assert_eq!(image_event.payload["image"]["width"], 2048);
        assert_eq!(image_event.payload["image"]["height"], 768);
    }

    #[test]
    fn view_image_resizes_before_enforcing_inline_limit_like_codex() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let path = Path::new(&session.cwd).join("large-raw.png");
        let bytes = noisy_png_bytes(2600, 2600);
        assert!(
            bytes.len() > MAX_INLINE_LOCAL_IMAGE_BYTES,
            "test fixture should exceed raw inline limit; got {}",
            bytes.len()
        );
        fs::write(&path, bytes).expect("write png");

        let result = view_image(
            &store,
            &session,
            &ToolCall {
                id: "image_1".to_string(),
                name: "view_image".to_string(),
                namespace: None,
                arguments: json!({"path": "large-raw.png", "detail": "high"}),
            },
            true,
        )
        .expect("large raw image should resize before limit check");

        let image_url = result.content[0]["image_url"].as_str().expect("image url");
        let (_, encoded) = image_url.split_once(',').expect("data url");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("decode image");
        assert!(decoded.len() <= MAX_INLINE_LOCAL_IMAGE_BYTES);
        let resized = image::load_from_memory(&decoded).expect("load resized");
        assert_eq!(resized.dimensions(), (2048, 2048));
    }

    #[test]
    fn view_image_rejects_invalid_image_bytes() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session) = test_session(&tmp);
        let path = Path::new(&session.cwd).join("not-image.png");
        fs::write(&path, b"not actually an image").expect("write invalid image");

        let error = view_image(
            &store,
            &session,
            &ToolCall {
                id: "image_1".to_string(),
                name: "view_image".to_string(),
                namespace: None,
                arguments: json!({"path": "not-image.png"}),
            },
            false,
        )
        .expect_err("invalid image should reject");

        assert!(format!("{error:#}").contains("unsupported or invalid image bytes"));
    }
}
