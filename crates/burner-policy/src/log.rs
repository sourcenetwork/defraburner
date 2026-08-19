//! Append-only decision log: `data_root/decisions.jsonl`, rotated at 8 MiB
//! to `decisions.jsonl.1` (at most 2 files total), with a bounded
//! `tail(n)` reader for `/admin/status` and the dashboard.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::clamp::ClampedAction;

const LOG_FILE_NAME: &str = "decisions.jsonl";
/// Rotate the current log to `.1` once it reaches this size.
const ROTATE_AT_BYTES: u64 = 8 * 1024 * 1024;

/// One recorded policy call: what was asked, what was decided, what was
/// authorized, and what happened when it was (or wasn't) acted on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionLogEntry {
    pub ts_ms: u64,
    pub tick: u64,
    pub package: String,
    pub input_sha256: String,
    pub raw_decision: Value,
    pub clamped: Vec<ClampedAction>,
    pub clamps_applied: Vec<String>,
    pub executed: bool,
    pub error: Option<String>,
}

fn log_path(data_root: &Path) -> PathBuf {
    data_root.join(LOG_FILE_NAME)
}

fn rotated_path(data_root: &Path) -> PathBuf {
    data_root.join(format!("{LOG_FILE_NAME}.1"))
}

/// Appends `entry` to `data_root/decisions.jsonl`, rotating first if the
/// file is already at or over `ROTATE_AT_BYTES`.
pub async fn append(data_root: &Path, entry: &DecisionLogEntry) -> Result<()> {
    let path = log_path(data_root);
    rotate_if_needed(data_root, &path).await?;

    let mut line = serde_json::to_string(entry).context("serializing decision log entry")?;
    line.push('\n');

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    file.write_all(line.as_bytes())
        .await
        .with_context(|| format!("appending to {}", path.display()))?;
    Ok(())
}

async fn rotate_if_needed(data_root: &Path, path: &Path) -> Result<()> {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("stat-ing {}", path.display())),
    };
    if metadata.len() < ROTATE_AT_BYTES {
        return Ok(());
    }
    let rotated = rotated_path(data_root);
    // A pre-existing `.1` is simply replaced: at most 2 files (current +
    // one rotated-out) are ever kept.
    tokio::fs::rename(path, &rotated)
        .await
        .with_context(|| format!("rotating {} to {}", path.display(), rotated.display()))
}

/// Reads the last `n` entries, newest last (append order): the current
/// file alone when it already holds at least `n` lines (the common case),
/// falling back to `decisions.jsonl.1` only when it doesn't. Bounded by
/// construction: `ROTATE_AT_BYTES` caps how much either file can ever
/// hold, so this never grows with total process lifetime.
pub async fn tail(data_root: &Path, n: usize) -> Result<Vec<DecisionLogEntry>> {
    let current = log_path(data_root);
    let mut lines = if current.exists() {
        read_lines(&current).await?
    } else {
        Vec::new()
    };

    if lines.len() < n {
        let rotated = rotated_path(data_root);
        if rotated.exists() {
            let mut older = read_lines(&rotated).await?;
            older.extend(lines);
            lines = older;
        }
    }

    let start = lines.len().saturating_sub(n);
    Ok(lines.split_off(start))
}

/// Malformed lines (never expected in practice: `append` only ever writes
/// whole, valid lines) are skipped rather than failing the whole read, so
/// one bad line cannot take down `/admin/status` or the dashboard.
async fn read_lines(path: &Path) -> Result<Vec<DecisionLogEntry>> {
    let content = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(content
        .lines()
        .filter_map(
            |line| match serde_json::from_str::<DecisionLogEntry>(line) {
                Ok(entry) => Some(entry),
                Err(error) => {
                    tracing::warn!(path = %path.display(), error = %error, "skipping a malformed decision log line");
                    None
                }
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tick: u64) -> DecisionLogEntry {
        DecisionLogEntry {
            ts_ms: 1000 + tick,
            tick,
            package: "autoscale-default".to_string(),
            input_sha256: "abc".to_string(),
            raw_decision: serde_json::json!({"action": "hold"}),
            clamped: Vec::new(),
            clamps_applied: Vec::new(),
            executed: true,
            error: None,
        }
    }

    #[tokio::test]
    async fn append_then_tail_round_trips_in_order() {
        let dir = tempfile::tempdir().unwrap();
        for tick in 1..=5 {
            append(dir.path(), &entry(tick)).await.unwrap();
        }
        let tail = tail(dir.path(), 3).await.unwrap();
        assert_eq!(
            tail.iter().map(|e| e.tick).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
    }

    #[tokio::test]
    async fn tail_of_an_empty_log_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tail = tail(dir.path(), 20).await.unwrap();
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn tail_n_larger_than_the_log_returns_everything() {
        let dir = tempfile::tempdir().unwrap();
        for tick in 1..=3 {
            append(dir.path(), &entry(tick)).await.unwrap();
        }
        let tail = tail(dir.path(), 100).await.unwrap();
        assert_eq!(tail.len(), 3);
    }

    #[tokio::test]
    async fn rotation_keeps_at_most_two_files_and_tail_spans_both() {
        let dir = tempfile::tempdir().unwrap();
        // Force a rotation by writing directly past ROTATE_AT_BYTES, then
        // append one more real entry through the normal path.
        let path = log_path(dir.path());
        let big_line = "x".repeat(ROTATE_AT_BYTES as usize + 10);
        tokio::fs::write(&path, format!("{big_line}\n"))
            .await
            .unwrap();

        append(dir.path(), &entry(1)).await.unwrap();

        assert!(
            rotated_path(dir.path()).exists(),
            "oversized log should have rotated to .1"
        );
        let current_len = tokio::fs::metadata(&path).await.unwrap().len();
        assert!(
            current_len < ROTATE_AT_BYTES,
            "current log should have been reset by rotation, was {current_len} bytes"
        );

        let tail = tail(dir.path(), 20).await.unwrap();
        assert_eq!(
            tail.len(),
            1,
            "the oversized rotated line is malformed JSON and is skipped"
        );
        assert_eq!(tail[0].tick, 1);
    }
}
