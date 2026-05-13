use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use browser_use_protocol::{SessionMeta, ToolCall};
use browser_use_store::Store;
use ignore::WalkBuilder;
use serde_json::{json, Value};

const DEFAULT_MAX_READ_LINES: usize = 400;
const DEFAULT_MAX_READ_BYTES: usize = 80_000;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 100;
const DEFAULT_MAX_LIST_RESULTS: usize = 200;

#[derive(Debug)]
pub(crate) struct FileToolResult {
    pub(crate) content: Value,
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
) -> Result<FileToolResult> {
    run_file_tool(store, session, call, "view_image", || {
        let path = required_path(session, &call.arguments)?;
        let detail = call
            .arguments
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mime = image_mime(&path);
        let image = json!({
            "path": path.display().to_string(),
            "mime_type": mime,
            "detail": detail,
            "bytes": bytes.len(),
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
            content: Value::Array(vec![
                json!({
                    "type": "output_text",
                    "text": format!("viewed image: {} ({mime}, {} bytes)", path.display(), bytes.len()),
                }),
                json!({
                    "type": "input_image",
                    "image_url": format!("data:{mime};base64,{}", general_purpose::STANDARD.encode(bytes)),
                    "detail": detail,
                }),
            ]),
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
        let ops = parse_patch(patch)?;
        let cwd = PathBuf::from(&session.cwd);
        let mut changes = Vec::new();
        for op in ops {
            changes.push(apply_operation(&cwd, op)?);
        }
        for change in &changes {
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
        }
        store.append_event(
            &session.id,
            "patch.finished",
            json!({
                "tool_call_id": call.id,
                "changed_files": changes.len(),
            }),
        )?;
        let mut lines = vec!["Applied patch.".to_string()];
        for change in &changes {
            let mut line = format!("- {} {}", change.kind, change.path.display());
            if let Some(move_path) = &change.move_path {
                line.push_str(&format!(" -> {}", move_path.display()));
            }
            lines.push(line);
        }
        Ok(FileToolResult {
            content: Value::String(lines.join("\n")),
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
    let mut command = Command::new("rg");
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

#[derive(Debug)]
struct AppliedChange {
    path: PathBuf,
    kind: &'static str,
    move_path: Option<PathBuf>,
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
    old: Vec<String>,
    new: Vec<String>,
}

fn parse_patch(patch: &str) -> Result<Vec<PatchOperation>> {
    let lines = patch.lines().collect::<Vec<_>>();
    if lines.first().copied() != Some("*** Begin Patch") {
        bail!("patch must start with *** Begin Patch");
    }
    if lines.last().copied() != Some("*** End Patch") {
        bail!("patch must end with *** End Patch");
    }
    let mut index = 1;
    let mut ops = Vec::new();
    while index < lines.len().saturating_sub(1) {
        let line = lines[index];
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = Vec::new();
            while index < lines.len() {
                let line = lines[index];
                if line.starts_with("*** ") {
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
                if let Some(target) = lines[index].strip_prefix("*** Move to: ") {
                    move_path = Some(target.to_string());
                    index += 1;
                }
            }
            let mut hunks = Vec::new();
            while index < lines.len() {
                let line = lines[index];
                if line.starts_with("*** ") {
                    break;
                }
                if line == "*** End of File" {
                    index += 1;
                    continue;
                }
                if !line.starts_with("@@") {
                    bail!("update hunk must start with @@");
                }
                index += 1;
                let mut old = Vec::new();
                let mut new = Vec::new();
                while index < lines.len() {
                    let line = lines[index];
                    if line.starts_with("@@") || line.starts_with("*** ") {
                        break;
                    }
                    if line == "*** End of File" {
                        index += 1;
                        break;
                    }
                    let Some(marker) = line.chars().next() else {
                        bail!("empty update hunk line");
                    };
                    let text = line[marker.len_utf8()..].to_string();
                    match marker {
                        ' ' => {
                            old.push(text.clone());
                            new.push(text);
                        }
                        '-' => old.push(text),
                        '+' => new.push(text),
                        _ => bail!("update hunk lines must start with space, -, or +"),
                    }
                    index += 1;
                }
                hunks.push(PatchHunk { old, new });
            }
            if hunks.is_empty() && move_path.is_none() {
                bail!("update file requires at least one hunk or move target");
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
    Ok(ops)
}

fn apply_operation(cwd: &Path, op: PatchOperation) -> Result<AppliedChange> {
    match op {
        PatchOperation::Add { path, content } => {
            let path = resolve_path(cwd, &path);
            if path.exists() {
                bail!("file already exists: {}", path.display());
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
            Ok(AppliedChange {
                path,
                kind: "added",
                move_path: None,
            })
        }
        PatchOperation::Delete { path } => {
            let path = resolve_path(cwd, &path);
            fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
            Ok(AppliedChange {
                path,
                kind: "deleted",
                move_path: None,
            })
        }
        PatchOperation::Update {
            path,
            move_path,
            hunks,
        } => {
            let path = resolve_path(cwd, &path);
            let original =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let (mut lines, final_newline) = split_text(&original);
            let mut cursor = 0;
            for hunk in hunks {
                let Some(pos) = find_sequence(&lines, &hunk.old, cursor) else {
                    bail!("patch hunk did not match {}", path.display());
                };
                lines.splice(pos..pos + hunk.old.len(), hunk.new);
                cursor = pos;
            }
            let new_content = lines_to_text(&lines, final_newline);
            let move_path = move_path.map(|target| resolve_path(cwd, &target));
            let write_path = move_path.as_ref().unwrap_or(&path);
            if let Some(parent) = write_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            fs::write(write_path, new_content)
                .with_context(|| format!("write {}", write_path.display()))?;
            if move_path.as_ref().is_some_and(|target| target != &path) {
                fs::remove_file(&path).with_context(|| format!("delete {}", path.display()))?;
            }
            Ok(AppliedChange {
                path,
                kind: if move_path.is_some() {
                    "moved"
                } else {
                    "modified"
                },
                move_path,
            })
        }
    }
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
    if let Some(path) = resolve_virtual_home_path(cwd, raw) {
        return path;
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_virtual_home_path(cwd: &Path, raw: &str) -> Option<PathBuf> {
    let suffix = raw.strip_prefix("/home/user")?;
    let home = virtual_home_for_cwd(cwd)?;
    let suffix = suffix.trim_start_matches('/');
    if suffix.is_empty() {
        Some(home)
    } else {
        Some(home.join(suffix))
    }
}

fn virtual_home_for_cwd(cwd: &Path) -> Option<PathBuf> {
    if cwd.file_name().and_then(|name| name.to_str()) != Some("cwd") {
        return None;
    }
    let root = cwd.parent()?.to_path_buf();
    if root.join("outputs").exists() {
        Some(root)
    } else {
        None
    }
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

fn image_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "image/png",
    }
}

fn split_text(text: &str) -> (Vec<String>, bool) {
    (
        text.lines().map(ToOwned::to_owned).collect(),
        text.ends_with('\n'),
    )
}

fn lines_to_text(lines: &[String], final_newline: bool) -> String {
    let mut text = lines.join("\n");
    if final_newline && !lines.is_empty() {
        text.push('\n');
    }
    text
}

fn find_sequence(lines: &[String], needle: &[String], start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(lines.len()));
    }
    if needle.len() > lines.len() {
        return None;
    }
    (start.min(lines.len())..=lines.len().saturating_sub(needle.len()))
        .find(|index| &lines[*index..*index + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_session(tmp: &TempDir) -> (Store, SessionMeta) {
        let store = Store::open(tmp.path().join("state")).expect("store");
        let cwd = tmp.path().join("work");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = store.create_session(None, cwd).expect("session");
        (store, session)
    }

    fn virtual_home_session(tmp: &TempDir) -> (Store, SessionMeta, PathBuf) {
        let store = Store::open(tmp.path().join("state")).expect("store");
        let root = tmp.path().join("task-root");
        let cwd = root.join("cwd");
        fs::create_dir_all(root.join("outputs")).expect("outputs");
        fs::create_dir_all(&cwd).expect("cwd");
        let session = store.create_session(None, cwd).expect("session");
        (store, session, root)
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
        let patch = r#"*** Begin Patch
*** Add File: a.txt
+hello
+world
*** Update File: a.txt
@@
 hello
-world
+rust
*** Update File: a.txt
*** Move to: b.txt
@@
-hello
+hi
 rust
*** Delete File: b.txt
*** End Patch"#;
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_1".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");
        assert!(!Path::new(&session.cwd).join("a.txt").exists());
        assert!(!Path::new(&session.cwd).join("b.txt").exists());
        let events = store.events_for_session(&session.id).expect("events");
        assert!(events
            .iter()
            .any(|event| event.event_type == "patch.file_changed"));
    }

    #[test]
    fn apply_patch_maps_virtual_home_paths() {
        let tmp = TempDir::new().expect("tmp");
        let (store, session, root) = virtual_home_session(&tmp);
        let patch = r#"*** Begin Patch
*** Add File: /home/user/outputs/result.txt
+artifact
*** End Patch"#;
        apply_patch_tool(
            &store,
            &session,
            &ToolCall {
                id: "patch_virtual_home".to_string(),
                name: "apply_patch".to_string(),
                arguments: json!({"patch": patch}),
            },
        )
        .expect("patch");
        assert_eq!(
            fs::read_to_string(root.join("outputs/result.txt")).expect("result"),
            "artifact\n"
        );
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
        fs::write(
            &path,
            [
                137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0,
                1, 8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 248,
                15, 4, 0, 9, 251, 3, 253, 167, 209, 143, 38, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
                96, 130,
            ],
        )
        .expect("write png");
        let result = view_image(
            &store,
            &session,
            &ToolCall {
                id: "image_1".to_string(),
                name: "view_image".to_string(),
                arguments: json!({"path": "pixel.png", "detail": "high"}),
            },
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
}
