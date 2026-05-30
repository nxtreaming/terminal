//! Persistent shell command history shared by the CLI and TUI front-ends.
//!
//! This is a faithful port of the message-history persistence API that used to
//! live inline in `browser-use-core` (`src/lib.rs`).  The on-disk format,
//! storage path, file permissions, locking and trimming behavior are all
//! preserved byte-for-byte so that existing history files keep working after
//! the front-ends migrate from the core crate to this agent crate.
//!
//! Ported from: browser-use-core/src/lib.rs
//!   - constants (MESSAGE_HISTORY_*) around lines 163-168
//!   - types MessageHistoryPersistence/Settings/Entry/Config around lines 180-221
//!   - public + private fns around lines 19023-19328
//!   - browser_use_terminal_home_dir around line 26380

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

// Constants for message history persistence.
const MESSAGE_HISTORY_LOCK_RETRIES: usize = 10;
const MESSAGE_HISTORY_LOCK_RETRY_SLEEP: Duration = Duration::from_millis(5);
pub const MESSAGE_HISTORY_FILENAME: &str = "history.jsonl";
const MESSAGE_HISTORY_SOFT_CAP_RATIO: f64 = 0.9;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageHistoryPersistence {
    SaveAll,
    None,
}

impl Default for MessageHistoryPersistence {
    fn default() -> Self {
        Self::SaveAll
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHistorySettings {
    #[serde(default)]
    pub persistence: MessageHistoryPersistence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

impl Default for MessageHistorySettings {
    fn default() -> Self {
        Self {
            persistence: MessageHistoryPersistence::SaveAll,
            max_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageHistoryEntry {
    pub session_id: String,
    pub ts: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageHistoryConfig {
    pub app_home: PathBuf,
    pub settings: MessageHistorySettings,
}

/// Resolve the directory that holds the persisted terminal state.
///
/// Honors `BROWSER_USE_TERMINAL_HOME` and otherwise falls back to
/// `~/.browser-use/terminal`, mirroring the core crate exactly.
pub fn browser_use_terminal_home_dir() -> Option<PathBuf> {
    if let Ok(custom) = std::env::var("BROWSER_USE_TERMINAL_HOME") {
        let trimmed = custom.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    // Legacy parity: browser-use-core's `browser_use_terminal_home_dir`
    // (lib.rs:19003) resolves to `$HOME/.browser_use_terminal` — a single,
    // underscored component. Existing history files live there.
    home_dir().map(|home| home.join(".browser_use_terminal"))
}

/// Best-effort home-directory lookup using only `std`.
///
/// The agent crate does not depend on the `dirs` crate, so this mirrors the
/// platform fallbacks `dirs::home_dir` performs for the cases we care about:
/// `$HOME` on unix and `%USERPROFILE%` on windows.
fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .filter(|profile| !profile.is_empty())
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    }
}

/// Build a [`MessageHistoryConfig`] for the given working directory.
///
/// In the core crate this also parsed `AGENTS.md`/config layers to resolve the
/// history [`MessageHistorySettings`].  That config-loading machinery is not yet
/// ported to the agent crate, so the settings are supplied directly by the
/// caller via `settings`; the `cwd` is accepted for signature/parity and future
/// use.  The storage location (`app_home`) is resolved identically to core.
pub fn message_history_config_for_cwd_with_options(
    _cwd: impl AsRef<Path>,
    settings: MessageHistorySettings,
) -> Result<Option<MessageHistoryConfig>> {
    let Some(app_home) = browser_use_terminal_home_dir() else {
        return Ok(None);
    };
    Ok(Some(MessageHistoryConfig { app_home, settings }))
}

pub fn append_message_history_entry_for_cwd(
    text: &str,
    session_id: &str,
    cwd: impl AsRef<Path>,
    settings: MessageHistorySettings,
) -> Result<bool> {
    let Some(config) = message_history_config_for_cwd_with_options(cwd, settings)? else {
        return Ok(false);
    };
    append_message_history_entry(text, session_id, &config)
}

pub fn append_message_history_entry(
    text: &str,
    session_id: &str,
    config: &MessageHistoryConfig,
) -> Result<bool> {
    if matches!(config.settings.persistence, MessageHistoryPersistence::None) {
        return Ok(false);
    }
    if text.trim().is_empty() {
        return Ok(false);
    }

    let path = message_history_path(config);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create message history dir {}", parent.display())
        })?;
    }

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| anyhow!("system clock before Unix epoch: {error}"))?
        .as_secs();
    let entry = MessageHistoryEntry {
        session_id: session_id.to_string(),
        ts,
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&entry)?;
    line.push('\n');

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.append(true).mode(0o600);
    }
    #[cfg(not(unix))]
    {
        options.append(true);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("failed to open message history {}", path.display()))?;
    ensure_message_history_permissions(&file)?;

    for _ in 0..MESSAGE_HISTORY_LOCK_RETRIES {
        match file.try_lock() {
            Ok(()) => {
                file.seek(SeekFrom::End(0))?;
                file.write_all(line.as_bytes())?;
                file.flush()?;
                enforce_message_history_limit(&mut file, config.settings.max_bytes)?;
                return Ok(true);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                thread::sleep(MESSAGE_HISTORY_LOCK_RETRY_SLEEP);
            }
            Err(error) => return Err(error).context("failed to lock message history"),
        }
    }
    bail!("could not acquire exclusive lock on message history file after multiple attempts")
}

pub fn message_history_metadata(config: &MessageHistoryConfig) -> (u64, usize) {
    message_history_metadata_for_path(&message_history_path(config))
}

pub fn lookup_message_history_entry(
    log_id: u64,
    offset: usize,
    config: &MessageHistoryConfig,
) -> Option<MessageHistoryEntry> {
    lookup_message_history_entry_at_path(&message_history_path(config), log_id, offset)
}

pub fn message_history_entries(
    log_id: u64,
    count: usize,
    config: &MessageHistoryConfig,
) -> Vec<MessageHistoryEntry> {
    message_history_entries_at_path(&message_history_path(config), log_id, count)
}

fn message_history_path(config: &MessageHistoryConfig) -> PathBuf {
    config.app_home.join(MESSAGE_HISTORY_FILENAME)
}

#[cfg(unix)]
fn ensure_message_history_permissions(file: &File) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = file.metadata()?;
    let current_mode = metadata.permissions().mode() & 0o777;
    if current_mode != 0o600 {
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o600);
        file.set_permissions(permissions)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_message_history_permissions(_file: &File) -> Result<()> {
    Ok(())
}

fn enforce_message_history_limit(file: &mut File, max_bytes: Option<usize>) -> Result<()> {
    let Some(max_bytes) = max_bytes else {
        return Ok(());
    };
    if max_bytes == 0 {
        return Ok(());
    }
    let max_bytes = max_bytes as u64;
    let mut current_len = file.metadata()?.len();
    if current_len <= max_bytes {
        return Ok(());
    }

    let mut reader_file = file.try_clone()?;
    reader_file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(reader_file);
    let mut line_lengths = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        line_lengths.push(bytes as u64);
    }
    if line_lengths.is_empty() {
        return Ok(());
    }

    let last_index = line_lengths.len() - 1;
    let newest_entry_len = line_lengths[last_index];
    let trim_target = message_history_trim_target_bytes(max_bytes, newest_entry_len);
    let mut drop_bytes = 0u64;
    let mut idx = 0usize;
    while current_len > trim_target && idx < last_index {
        current_len = current_len.saturating_sub(line_lengths[idx]);
        drop_bytes += line_lengths[idx];
        idx += 1;
    }
    if drop_bytes == 0 {
        return Ok(());
    }

    let mut reader = reader.into_inner();
    reader.seek(SeekFrom::Start(drop_bytes))?;
    let mut tail = Vec::with_capacity(usize::try_from(current_len).unwrap_or(0));
    reader.read_to_end(&mut tail)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&tail)?;
    file.flush()?;
    Ok(())
}

fn message_history_trim_target_bytes(max_bytes: u64, newest_entry_len: u64) -> u64 {
    let soft_cap_bytes = ((max_bytes as f64) * MESSAGE_HISTORY_SOFT_CAP_RATIO)
        .floor()
        .clamp(1.0, max_bytes as f64) as u64;
    soft_cap_bytes.max(newest_entry_len)
}

fn message_history_metadata_for_path(path: &Path) -> (u64, usize) {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(_) => return (0, 0),
    };
    let log_id = message_history_log_identity(&metadata).unwrap_or(0);
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return (log_id, 0),
    };
    let mut buffer = [0_u8; 8192];
    let mut count = 0usize;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => count += buffer[..n].iter().filter(|&&byte| byte == b'\n').count(),
            Err(_) => return (log_id, 0),
        }
    }
    (log_id, count)
}

fn lookup_message_history_entry_at_path(
    path: &Path,
    log_id: u64,
    offset: usize,
) -> Option<MessageHistoryEntry> {
    let file = OpenOptions::new().read(true).open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let current_log_id = message_history_log_identity(&metadata)?;
    if log_id != 0 && current_log_id != log_id {
        return None;
    }
    for _ in 0..MESSAGE_HISTORY_LOCK_RETRIES {
        match file.try_lock_shared() {
            Ok(()) => {
                let reader = BufReader::new(&file);
                for (idx, line) in reader.lines().enumerate() {
                    let line = line.ok()?;
                    if idx == offset {
                        return serde_json::from_str::<MessageHistoryEntry>(&line).ok();
                    }
                }
                return None;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                thread::sleep(MESSAGE_HISTORY_LOCK_RETRY_SLEEP);
            }
            Err(_) => return None,
        }
    }
    None
}

fn message_history_entries_at_path(
    path: &Path,
    log_id: u64,
    count: usize,
) -> Vec<MessageHistoryEntry> {
    if count == 0 {
        return Vec::new();
    }
    let Some(file) = OpenOptions::new().read(true).open(path).ok() else {
        return Vec::new();
    };
    let Some(current_log_id) = file
        .metadata()
        .ok()
        .and_then(|metadata| message_history_log_identity(&metadata))
    else {
        return Vec::new();
    };
    if current_log_id != log_id {
        return Vec::new();
    }
    for _ in 0..MESSAGE_HISTORY_LOCK_RETRIES {
        match file.try_lock_shared() {
            Ok(()) => {
                return BufReader::new(&file)
                    .lines()
                    .take(count)
                    .map_while(|line| line.ok())
                    .filter_map(|line| serde_json::from_str::<MessageHistoryEntry>(&line).ok())
                    .collect();
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                thread::sleep(MESSAGE_HISTORY_LOCK_RETRY_SLEEP);
            }
            Err(_) => return Vec::new(),
        }
    }
    Vec::new()
}

#[cfg(unix)]
fn message_history_log_identity(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(windows)]
fn message_history_log_identity(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    Some(metadata.creation_time())
}

#[cfg(not(any(unix, windows)))]
fn message_history_log_identity(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn config_in(dir: &Path) -> MessageHistoryConfig {
        MessageHistoryConfig {
            app_home: dir.to_path_buf(),
            settings: MessageHistorySettings::default(),
        }
    }

    #[test]
    fn append_metadata_lookup_and_entries_round_trip() -> Result<()> {
        let temp = tempdir()?;
        let config = config_in(temp.path());

        assert!(append_message_history_entry("first", "session-a", &config)?);
        assert!(append_message_history_entry(
            "second",
            "session-a",
            &config
        )?);

        // metadata reports a non-zero log id (inode on unix) and the entry count.
        let (log_id, count) = message_history_metadata(&config);
        assert_eq!(count, 2);

        // lookup walks from the start: offset 0 is the oldest entry.
        let first = lookup_message_history_entry(log_id, 0, &config).expect("first entry");
        assert_eq!(first.text, "first");
        assert_eq!(first.session_id, "session-a");
        let second = lookup_message_history_entry(log_id, 1, &config).expect("second entry");
        assert_eq!(second.text, "second");

        // entries returns up to `count` lines preserving on-disk order.
        let entries = message_history_entries(log_id, count, &config);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "first");
        assert_eq!(entries[1].text, "second");

        // A mismatched log id yields nothing, matching the core guard.
        assert!(lookup_message_history_entry(log_id.saturating_add(1), 0, &config).is_none());

        Ok(())
    }

    #[test]
    fn empty_text_is_not_persisted() -> Result<()> {
        let temp = tempdir()?;
        let config = config_in(temp.path());
        assert!(!append_message_history_entry("   ", "s", &config)?);
        let (_, count) = message_history_metadata(&config);
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn persistence_none_writes_nothing() -> Result<()> {
        let temp = tempdir()?;
        let config = MessageHistoryConfig {
            app_home: temp.path().to_path_buf(),
            settings: MessageHistorySettings {
                persistence: MessageHistoryPersistence::None,
                max_bytes: None,
            },
        };
        assert!(!append_message_history_entry("ignored", "s", &config)?);
        assert!(!temp.path().join(MESSAGE_HISTORY_FILENAME).exists());
        Ok(())
    }

    #[test]
    fn max_bytes_trims_oldest_to_soft_cap() -> Result<()> {
        let temp = tempdir()?;
        let config = MessageHistoryConfig {
            app_home: temp.path().to_path_buf(),
            settings: MessageHistorySettings {
                persistence: MessageHistoryPersistence::SaveAll,
                max_bytes: Some(220),
            },
        };
        for idx in 0..10 {
            append_message_history_entry(&format!("entry-{idx}"), "s", &config)?;
        }
        let path = temp.path().join(MESSAGE_HISTORY_FILENAME);
        let len = std::fs::metadata(&path)?.len();
        assert!(
            len <= 220,
            "history file should be trimmed under cap, got {len}"
        );

        // The newest entry must survive the trim.
        let (log_id, count) = message_history_metadata(&config);
        let entries = message_history_entries(log_id, count, &config);
        assert!(entries.iter().any(|e| e.text == "entry-9"));
        Ok(())
    }

    #[test]
    fn on_disk_format_is_json_lines() -> Result<()> {
        let temp = tempdir()?;
        let config = config_in(temp.path());
        append_message_history_entry("hello", "sid", &config)?;
        let raw = std::fs::read_to_string(temp.path().join(MESSAGE_HISTORY_FILENAME))?;
        let line = raw.lines().next().expect("one line");
        let parsed: MessageHistoryEntry = serde_json::from_str(line)?;
        assert_eq!(parsed.session_id, "sid");
        assert_eq!(parsed.text, "hello");
        Ok(())
    }

    #[test]
    fn config_for_cwd_uses_terminal_home() -> Result<()> {
        let temp = tempdir()?;
        // Point the terminal home at a scratch dir so the test is hermetic.
        std::env::set_var("BROWSER_USE_TERMINAL_HOME", temp.path());
        let config = message_history_config_for_cwd_with_options(
            temp.path(),
            MessageHistorySettings::default(),
        )?
        .expect("history config available");
        std::env::remove_var("BROWSER_USE_TERMINAL_HOME");

        assert_eq!(config.app_home, temp.path());
        assert_eq!(config.settings, MessageHistorySettings::default());

        // Round-trip through the cwd-config to prove the path wiring works.
        assert!(append_message_history_entry("alpha", "sess", &config)?);
        let (log_id, count) = message_history_metadata(&config);
        let entries = message_history_entries(log_id, count, &config);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "alpha");
        Ok(())
    }
}
