//! Kernel AI Shell — runs inside the kernel before userspace is ready
//!
//! This is the emergency / early-boot shell that appears if the compositor
//! or init fails to start. It is ALSO the backing implementation of the
//! userspace shell's kernel IPC bridge.
//!
//! AI features:
//!   • Tab → sends prefix to "ai.req" IPC channel → aidaemon → LLM → completes
//!   • Ghost text: after each character, requests a 1-token prediction
//!   • `?? <query>` prefix → asks aidaemon for an explanation
//!   • Error detection: if a command fails, auto-asks AI for the fix
//!   • Word completion trie is embedded in kernel (no aidaemon needed for it)
//!
//! When aidaemon is not running (pure early-boot), Tab still works via
//! the embedded trie (keywords, paths, commands) — AI just won't be available.

extern crate alloc;
use alloc::borrow::ToOwned;
use alloc::{string::String, vec::Vec, format};
use crate::ipc;

// ─── Embedded keyword trie for early-boot completion ─────────────────────────
// Stored as a flat sorted list — binary search gives O(log n) prefix match.
// No heap allocation — stored in .rodata.

static KERNEL_COMPLETIONS: &[&str] = &[
    "cat", "cd", "chmod", "chown", "clear", "cp",
    "dmesg", "echo", "exit",
    "find",
    "grep",
    "help", "history",
    "kill",
    "less", "ln", "ls", "ls -la", "ls -lh",
    "man", "mkdir", "mount", "mv",
    "npkg", "npkg install", "npkg list", "npkg remove", "npkg update",
    "ping", "ps",
    "reboot", "rm", "rmdir",
    "shutdown", "sleep", "stat",
    "tail", "top", "touch",
    "umount", "uname",
    "which",
];

/// Find completions for `prefix` using binary search. O(log n + k).
fn trie_complete<'a>(prefix: &str) -> Vec<&'static str> {
    let mut results = Vec::new();
    // Binary search to first match
    let pos = KERNEL_COMPLETIONS.partition_point(|&s| s < prefix);
    for &s in &KERNEL_COMPLETIONS[pos..] {
        if s.starts_with(prefix) {
            results.push(s);
            if results.len() >= 8 { break; }
        } else {
            break;
        }
    }
    results
}

// ─── Shell state ──────────────────────────────────────────────────────────────

pub struct KernelShell {
    buf:        String,
    cursor:     usize,
    history:    Vec<String>,
    hist_idx:   Option<usize>,
    ghost:      Option<String>,
    ai_ready:   bool,        // true once aidaemon responds to ping
}

impl KernelShell {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_idx: None,
            ghost: None,
            ai_ready: false,
        }
    }

    /// Check if aidaemon is alive by sending a ping request.
    pub fn probe_ai(&mut self) {
        // Send a minimal 1-token request; if we get a response, AI is up.
        let result = ipc::get_ai_completion("echo ", "shell", 1);
        self.ai_ready = !result.is_empty();
        if self.ai_ready {
            crate::kprintln!("[kshell] AI daemon connected");
        }
    }

    // ── Tab completion ─────────────────────────────────────────────────────

    /// Handle Tab key. Returns list of completions.
    pub fn tab_complete(&mut self) -> Vec<String> {
        let line = self.buf.clone();

        // Stage 0: embedded trie — always instant
        let trie_hits = trie_complete(&line);
        if !trie_hits.is_empty() {
            if trie_hits.len() == 1 {
                // Single match — apply it directly
                self.buf = String::from(trie_hits[0]);
                self.cursor = self.buf.len();
                return Vec::new(); // no menu needed
            }
            // Multiple trie matches — return them for display
            return trie_hits.iter().map(|s| String::from(*s)).collect();
        }

        // Stage 1 / 2: ask aidaemon (works offline → local model, online → API)
        if self.ai_ready {
            let completion = ipc::get_ai_completion(&line, "shell", 32);
            if !completion.is_empty() {
                // Append the completion suffix to the current line
                let token_start = line.rfind(|c: char| c == ' ' || c == '/')
                    .map_or(0, |i| i + 1);
                let completion_suffix = if completion.starts_with(&line[token_start..]) {
                    completion[line.len() - token_start..].to_owned()
                } else {
                    completion.clone()
                };
                self.buf.push_str(&completion_suffix);
                self.cursor = self.buf.len();
                return Vec::new();
            }
        }

        Vec::new()
    }

    // ── Ghost text (next-word prediction) ─────────────────────────────────

    /// Request a 1-token prediction after the current line. Non-blocking read.
    /// In the kernel shell this is called after every character typed.
    pub fn update_ghost(&mut self) {
        if !self.ai_ready { return; }
        // The IPC call is synchronous here — in a real impl it would be async.
        // For early-boot context we use a very short max_tokens=1 so it's fast.
        let pred = ipc::get_ai_completion(&self.buf, "shell", 1);
        self.ghost = if pred.is_empty() { None } else { Some(pred) };
    }

    // ── `?? <query>` — inline AI explanation ──────────────────────────────

    /// If the line starts with `??`, send the rest as an explanation query.
    pub fn maybe_ai_query(&self) -> Option<String> {
        let line = self.buf.trim();
        if !line.starts_with("??") { return None; }
        let query = line[2..].trim();
        if query.is_empty() { return None; }

        // Build a natural-language prompt
        let prompt = format!(
            "Explain this shell command briefly (1-2 lines): {}",
            query
        );
        let answer = ipc::get_ai_completion(&prompt, "shell", 80);
        if answer.is_empty() { None } else { Some(answer) }
    }

    // ── Auto-fix on error ─────────────────────────────────────────────────

    /// After a command fails, ask AI what went wrong and suggest a fix.
    pub fn ai_suggest_fix(&self, failed_cmd: &str, exit_code: i32) -> Option<String> {
        if !self.ai_ready { return None; }
        let prompt = format!(
            "The shell command `{}` failed with exit code {}. \
             Give a one-sentence explanation and the corrected command.",
            failed_cmd, exit_code
        );
        let suggestion = ipc::get_ai_completion(&prompt, "shell", 60);
        if suggestion.is_empty() { None } else { Some(suggestion) }
    }

    // ── Editing ────────────────────────────────────────────────────────────

    pub fn insert(&mut self, ch: char) {
        self.ghost = None;
        self.buf.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    pub fn backspace(&mut self) {
        self.ghost = None;
        if self.cursor > 0 {
            let prev = self.buf[..self.cursor]
                .char_indices().next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.buf.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    pub fn accept_ghost(&mut self) {
        if let Some(g) = self.ghost.take() {
            self.buf.push_str(&g);
            self.cursor = self.buf.len();
        }
    }

    pub fn line(&self) -> &str { &self.buf }
    pub fn ghost(&self) -> Option<&str> { self.ghost.as_deref() }

    pub fn commit(&mut self) -> String {
        let line = core::mem::take(&mut self.buf);
        self.cursor = 0;
        self.ghost  = None;
        if !line.trim().is_empty() {
            self.history.push(line.clone());
        }
        self.hist_idx = None;
        line
    }

    pub fn hist_prev(&mut self) {
        if self.history.is_empty() { return; }
        let idx = self.hist_idx
            .map(|i| i.saturating_sub(1))
            .unwrap_or(self.history.len() - 1);
        self.hist_idx = Some(idx);
        self.buf      = self.history[idx].clone();
        self.cursor   = self.buf.len();
    }

    pub fn hist_next(&mut self) {
        if let Some(idx) = self.hist_idx {
            if idx + 1 < self.history.len() {
                let new = idx + 1;
                self.hist_idx = Some(new);
                self.buf      = self.history[new].clone();
                self.cursor   = self.buf.len();
            } else {
                self.hist_idx = None;
                self.buf.clear();
                self.cursor = 0;
            }
        }
    }
}

// ─── Built-in commands ────────────────────────────────────────────────────────

/// Execute a line in the kernel shell context.
/// Returns (output, exit_code).
pub fn exec_builtin(line: &str) -> (String, i32) {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("help") => (
            String::from(
                "Noxisa Kernel Shell — built-ins:\n\
                 help, clear, reboot, dmesg, meminfo, ps, version\n\
                 Tab: AI completion    ??: AI explanation"
            ), 0
        ),
        Some("version") => (
            format!("Noxisa v0.1 — kernel shell (AI {})",
                if ipc::AI_REQ_CHANNEL.get().is_some() { "online" } else { "offline" }),
            0
        ),
        Some("meminfo") => (
            format!("Free: {} MiB",
                crate::memory::pmm::free_frames() * 4096 / (1024 * 1024)),
            0
        ),
        Some("clear") => (String::from("\x1b[2J\x1b[H"), 0),
        Some("reboot") => {
            crate::kprintln!("[kshell] Rebooting...");
            unsafe { core::arch::asm!("out 0x64, al", in("al") 0xFEu8, options(nomem, nostack)); }
            (String::new(), 0)
        }
        Some(cmd) => (
            format!("kshell: {}: command not found\n(hint: full shell launches after init)", cmd),
            127
        ),
        None => (String::new(), 0),
    }
}
