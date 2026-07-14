use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub timestamp: String,
    pub request_name: String,
    pub method: String,
    pub url: String,
    pub status: String,
    pub response: String,
}

#[derive(Clone)]
pub struct HistoryManager {
    path: PathBuf,
}

impl HistoryManager {
    pub fn new(config_dir: &Path) -> Self {
        Self { path: config_dir.join("history.jsonl") }
    }

    pub fn record(&self, request_name: &str, method: &str, url: &str, status: &str, response: &str) -> Result<HistoryEntry> {
        let entry = HistoryEntry {
            timestamp: current_timestamp(),
            request_name: request_name.to_string(),
            method: method.to_string(),
            url: url.to_string(),
            status: status.to_string(),
            response: response.to_string(),
        };
        let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&entry)?)?;
        Ok(entry)
    }

    pub fn load_all(&self) -> Result<Vec<HistoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<HistoryEntry>(&line) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn current_timestamp() -> String {
    format_unix_timestamp(now_unix())
}

/// Civil date/time from a Unix timestamp (UTC) via Howard Hinnant's
/// days-from-civil algorithm, to avoid pulling in a date/time crate.
fn format_unix_timestamp(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (hour, min, sec) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", year, m, d, hour, min, sec)
}
