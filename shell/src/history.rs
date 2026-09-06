//! Shell history — persistent, searchable command history.
//!
//! Stores up to `max` entries; oldest entries are evicted when full.
//! Saved to `~/.nsh_history` on every new entry.

use std::path::PathBuf;
use anyhow::Result;

pub struct History {
    entries: Vec<String>,
    max:     usize,
    path:    PathBuf,
}

impl History {
    /// Load history from disk, or create empty if file doesn't exist.
    pub async fn load(path: &str, max: usize) -> Self {
        let path = PathBuf::from(path.replace('~', &home_dir()));
        let entries = tokio::fs::read_to_string(&path)
            .await
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| l.to_owned())
            .collect::<Vec<_>>();

        // Keep only last `max` entries
        let entries = if entries.len() > max {
            entries[entries.len() - max..].to_vec()
        } else {
            entries
        };

        Self { entries, max, path }
    }

    /// Add a new entry. Deduplicates consecutive identical commands.
    pub fn push(&mut self, line: String) {
        let trimmed = line.trim().to_owned();
        if trimmed.is_empty() { return; }
        if self.entries.last().map_or(false, |l| l == &trimmed) { return; }

        self.entries.push(trimmed);
        if self.entries.len() > self.max {
            self.entries.remove(0);
        }
    }

    /// Save history to disk asynchronously.
    pub async fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&self.path, self.entries.join("\n") + "\n").await?;
        Ok(())
    }

    /// Number of stored entries.
    pub fn len(&self) -> usize { self.entries.len() }

    /// Get entry at index `i` (0 = oldest).
    pub fn get(&self, i: usize) -> Option<&str> {
        self.entries.get(i).map(|s| s.as_str())
    }

    /// Fuzzy search: return indices of entries matching `query`.
    /// Simple substring match — O(n * |query|).
    pub fn search(&self, query: &str) -> Vec<usize> {
        let q = query.to_lowercase();
        self.entries.iter().enumerate()
            .filter(|(_, e)| e.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect()
    }
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/root".to_owned())
}
