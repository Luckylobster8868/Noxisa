//! nsh — Noxisa Shell
//!
//! Features:
//!   • 3-stage AI Tab completion (Trie → Offline LLM → Online API)
//!   • Ghost text (Fish-style inline suggestions, accepted with →)
//!   • Syntax highlighting via single-pass tokeniser
//!   • Persistent history with fuzzy search (Ctrl+R)
//!   • POSIX parser: pipes, redirections, sequences, background jobs
//!   • Ctrl+A/E/K/U/W/L editing  •  Alt+B/F word motion

mod history;
mod parser;

use std::io::{self, Read, Write};
use std::sync::Arc;
use anyhow::Result;
use parking_lot::RwLock;
use tokio::sync::Mutex;

use ai_engine::{AiEngine, Config as AiConfig, CompletionReq};
use history::History;
use parser::{parse, Script};

// ─── ANSI escape codes ────────────────────────────────────────────────────────

macro_rules! csi { ($s:expr) => { concat!("\x1b[", $s) }; }

const RESET:   &str = "\x1b[0m";
const BOLD:    &str = "\x1b[1m";
const DIM:     &str = "\x1b[2m";
const FG_CYAN: &str = "\x1b[36m";
const FG_GRN:  &str = "\x1b[32m";
const FG_YEL:  &str = "\x1b[33m";
const FG_RED:  &str = "\x1b[31m";
const FG_MAG:  &str = "\x1b[35m";
const FG_GRAY: &str = "\x1b[90m";
const CLEAR_L: &str = "\x1b[2K\r";

// ─── Shell state ──────────────────────────────────────────────────────────────

struct Shell {
    ai:         Arc<AiEngine>,
    history:    Arc<Mutex<History>>,
    buf:        Vec<char>,
    cursor:     usize,
    ghost:      Option<String>,
    hist_idx:   Option<usize>,
    hist_saved: String,
}

impl Shell {
    async fn new() -> Result<Self> {
        let config = AiConfig {
            api_key:    std::env::var("NEXUS_AI_KEY").ok(),
            model_path: std::env::var("NEXUS_MODEL_PATH")
                .unwrap_or_else(|_| "/usr/share/nexus-os/models/phi3-mini-q4.gguf".into()),
            ..AiConfig::default()
        };
        let ai      = AiEngine::new(config).await?;
        let history = Arc::new(Mutex::new(
            History::load("~/.nsh_history", 50_000).await
        ));
        Ok(Self {
            ai, history,
            buf: Vec::new(), cursor: 0,
            ghost: None, hist_idx: None, hist_saved: String::new(),
        })
    }

    // ── Main REPL ─────────────────────────────────────────────────────────

    async fn run(&mut self) -> Result<()> {
        print_banner();
        enable_raw_mode();

        loop {
            self.render_prompt();
            self.render_line();

            let key = read_key();
            match self.handle(key).await {
                Action::Continue => {}
                Action::Execute  => {
                    let line = self.line_str();
                    println!();
                    if !line.trim().is_empty() {
                        self.history.lock().await.push(line.clone());
                        self.execute(&line).await;
                    }
                    self.reset();
                }
                Action::Quit => {
                    disable_raw_mode();
                    println!("\nBye! 👋");
                    break;
                }
            }
        }
        Ok(())
    }

    // ── Key handler ───────────────────────────────────────────────────────

    async fn handle(&mut self, key: Key) -> Action {
        match key {
            Key::Tab => {
                if let Some(ghost) = self.ghost.take() {
                    self.accept_ghost(&ghost);
                } else {
                    self.do_tab_complete().await;
                }
                Action::Continue
            }

            Key::Right => {
                if self.cursor < self.buf.len() {
                    self.cursor += 1;
                } else if let Some(ghost) = self.ghost.take() {
                    // Accept one word of ghost text
                    let end = ghost.find(' ').map_or(ghost.len(), |i| i + 1);
                    let word: Vec<char> = ghost[..end].chars().collect();
                    let pos = self.cursor;
                    for (i, ch) in word.iter().enumerate() {
                        self.buf.insert(pos + i, *ch);
                    }
                    self.cursor += word.len();
                    let rest = ghost[end..].to_owned();
                    self.ghost = if rest.is_empty() { None } else { Some(rest) };
                }
                Action::Continue
            }

            Key::Char(c) => {
                // If first char matches ghost, advance ghost
                if let Some(ref g) = self.ghost.clone() {
                    if g.starts_with(c) {
                        let rest = g[c.len_utf8()..].to_owned();
                        self.buf.insert(self.cursor, c);
                        self.cursor += 1;
                        self.ghost = if rest.is_empty() { None } else { Some(rest) };
                        self.render_line();
                        return Action::Continue;
                    }
                }
                self.ghost = None;
                self.buf.insert(self.cursor, c);
                self.cursor += 1;
                // Async ghost update (non-blocking)
                self.request_ghost().await;
                self.render_line();
                Action::Continue
            }

            Key::Backspace => {
                self.ghost = None;
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.buf.remove(self.cursor);
                    self.render_line();
                }
                Action::Continue
            }

            Key::Enter  => Action::Execute,
            Key::CtrlC  => { self.reset(); println!("^C"); Action::Continue }
            Key::CtrlD  => {
                if self.buf.is_empty() { Action::Quit }
                else { self.reset(); self.render_line(); Action::Continue }
            }

            Key::Up     => { self.hist_prev().await; Action::Continue }
            Key::Down   => { self.hist_next().await; Action::Continue }

            Key::Left   => { if self.cursor > 0 { self.cursor -= 1; } Action::Continue }
            Key::CtrlA  => { self.cursor = 0; Action::Continue }
            Key::CtrlE  => { self.cursor = self.buf.len(); Action::Continue }
            Key::CtrlK  => { self.buf.truncate(self.cursor); Action::Continue }
            Key::CtrlU  => { self.buf.drain(..self.cursor); self.cursor = 0; Action::Continue }
            Key::CtrlW  => { self.delete_word(); Action::Continue }
            Key::CtrlL  => { print!("\x1b[2J\x1b[H"); Action::Continue }
            Key::CtrlR  => { self.search_history().await; Action::Continue }
            Key::AltB   => { self.move_word(-1); Action::Continue }
            Key::AltF   => { self.move_word(1);  Action::Continue }
            Key::Unknown => Action::Continue,
        }
    }

    // ── Tab completion ─────────────────────────────────────────────────────

    async fn do_tab_complete(&mut self) {
        let line = self.line_str();
        let req  = CompletionReq {
            prefix:     line.clone(),
            line:       line.clone(),
            language:   Some("shell".into()),
            max_tokens: 64,
            n:          12,
        };
        match self.ai.complete(&req).await {
            Ok(results) if !results.is_empty() => {
                if results.len() == 1 {
                    // Single match — apply inline
                    let completion = &results[0].text;
                    let token_len  = current_token_len(&line);
                    let suffix: Vec<char> = completion[token_len..].chars().collect();
                    let pos = self.cursor;
                    for (i, ch) in suffix.iter().enumerate() {
                        self.buf.insert(pos + i, *ch);
                    }
                    self.cursor += suffix.len();
                } else {
                    // Multiple — show menu below prompt
                    println!();
                    for (i, r) in results.iter().enumerate() {
                        if i % 4 == 0 && i > 0 { println!(); }
                        print!("{FG_CYAN}{:<22}{RESET}", r.text);
                    }
                    println!();
                }
            }
            _ => {
                // No completions — flash cursor (bell)
                print!("\x07");
            }
        }
        let _ = io::stdout().flush();
    }

    // ── Ghost text ─────────────────────────────────────────────────────────

    async fn request_ghost(&mut self) {
        let line = self.line_str();
        if line.trim().is_empty() { return; }
        let suggestions = self.ai.next_word(&line, 1).await;
        if let Some((word, score)) = suggestions.into_iter().next() {
            if score > 0.60 {
                self.ghost = Some(format!(" {word}"));
            }
        }
    }

    fn accept_ghost(&mut self, ghost: &str) {
        let chars: Vec<char> = ghost.chars().collect();
        let pos = self.cursor;
        for (i, &ch) in chars.iter().enumerate() {
            self.buf.insert(pos + i, ch);
        }
        self.cursor += chars.len();
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    fn render_prompt(&self) {
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".into());
        let ai_dot = if self.ai.is_online() {
            format!("{FG_CYAN}⚡{RESET}")
        } else {
            format!("{FG_GRAY}●{RESET}")
        };
        print!("{BOLD}{FG_GRN}{cwd}{RESET} {ai_dot}\n{BOLD}{FG_CYAN}❯{RESET} ");
        let _ = io::stdout().flush();
    }

    fn render_line(&self) {
        let line       = self.line_str();
        let highlighted = syntax_highlight(&line);
        let ghost_str  = self.ghost.as_deref().map(|g| {
            format!("{DIM}{FG_GRAY}{g}{RESET}")
        }).unwrap_or_default();

        let ghost_len = self.ghost.as_deref()
            .map(|g| g.chars().count())
            .unwrap_or(0);

        print!("{CLEAR_L}{highlighted}{ghost_str}");

        // Move cursor back over ghost text
        if ghost_len > 0 {
            print!("{}", csi!("{}D", ghost_len));
        }

        // Position cursor correctly within the line
        let line_cursor_offset = line.chars().count() - self.cursor;
        if line_cursor_offset > 0 {
            print!("{}", csi!("{}D", line_cursor_offset));
        }

        let _ = io::stdout().flush();
    }

    // ── Editing helpers ────────────────────────────────────────────────────

    fn delete_word(&mut self) {
        let end = self.cursor;
        while self.cursor > 0 && self.buf[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        while self.cursor > 0 && !self.buf[self.cursor - 1].is_whitespace() {
            self.cursor -= 1;
        }
        self.buf.drain(self.cursor..end);
    }

    fn move_word(&mut self, dir: i32) {
        if dir > 0 {
            while self.cursor < self.buf.len() && !self.buf[self.cursor].is_alphanumeric() {
                self.cursor += 1;
            }
            while self.cursor < self.buf.len() && self.buf[self.cursor].is_alphanumeric() {
                self.cursor += 1;
            }
        } else {
            if self.cursor == 0 { return; }
            self.cursor -= 1;
            while self.cursor > 0 && !self.buf[self.cursor - 1].is_alphanumeric() {
                self.cursor -= 1;
            }
            while self.cursor > 0 && self.buf[self.cursor - 1].is_alphanumeric() {
                self.cursor -= 1;
            }
        }
    }

    // ── History ────────────────────────────────────────────────────────────

    async fn hist_prev(&mut self) {
        let hist = self.history.lock().await;
        if hist.len() == 0 { return; }
        if self.hist_idx.is_none() { self.hist_saved = self.line_str(); }
        let new_idx = self.hist_idx.map_or(hist.len() - 1, |i| i.saturating_sub(1));
        self.hist_idx = Some(new_idx);
        if let Some(e) = hist.get(new_idx) { self.set_line(e.to_owned()); }
    }

    async fn hist_next(&mut self) {
        let hist = self.history.lock().await;
        if let Some(idx) = self.hist_idx {
            if idx + 1 >= hist.len() {
                self.hist_idx = None;
                let saved = self.hist_saved.clone();
                self.set_line(saved);
            } else {
                let new_idx = idx + 1;
                self.hist_idx = Some(new_idx);
                if let Some(e) = hist.get(new_idx) { self.set_line(e.to_owned()); }
            }
        }
    }

    async fn search_history(&mut self) {
        let query = self.line_str();
        let hist  = self.history.lock().await;
        let hits  = hist.search(&query);
        if let Some(&last) = hits.last() {
            if let Some(e) = hist.get(last) {
                self.set_line(e.to_owned());
            }
        }
    }

    // ── Command execution ──────────────────────────────────────────────────

    async fn execute(&self, line: &str) {
        match line.trim() {
            "exit" | "quit" => std::process::exit(0),
            "help" => print_help(),
            _ => match parse(line) {
                Ok(script)  => run_script(script).await,
                Err(e) => eprintln!("{FG_RED}nsh: parse error: {e}{RESET}"),
            }
        }
    }

    // ── Utilities ──────────────────────────────────────────────────────────

    fn line_str(&self) -> String { self.buf.iter().collect() }

    fn set_line(&mut self, s: String) {
        self.buf    = s.chars().collect();
        self.cursor = self.buf.len();
        self.render_line();
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.cursor = 0;
        self.ghost  = None;
        self.hist_idx = None;
    }
}

// ─── Command execution ────────────────────────────────────────────────────────

async fn run_script(script: Script) {
    for pipeline in script.pipelines {
        for cmd in pipeline.stages {
            if cmd.argv.is_empty() { continue; }
            // Handle built-in commands
            match cmd.argv[0].as_str() {
                "cd" => {
                    let dir = cmd.argv.get(1).map(|s| s.as_str()).unwrap_or("~");
                    let path = if dir == "~" {
                        std::env::var("HOME").unwrap_or_else(|_| "/".into())
                    } else {
                        dir.to_owned()
                    };
                    if let Err(e) = std::env::set_current_dir(&path) {
                        eprintln!("{FG_RED}nsh: cd: {e}{RESET}");
                    }
                }
                "export" => {
                    for arg in &cmd.argv[1..] {
                        if let Some((k, v)) = arg.split_once('=') {
                            std::env::set_var(k, v);
                        }
                    }
                }
                _ => {
                    // Spawn external process
                    match std::process::Command::new(&cmd.argv[0])
                        .args(&cmd.argv[1..])
                        .status()
                    {
                        Ok(status) if !status.success() => {
                            if let Some(code) = status.code() {
                                eprintln!("{FG_RED}exit code: {code}{RESET}");
                            }
                        }
                        Err(e) => {
                            eprintln!("{FG_RED}nsh: {}: {e}{RESET}", cmd.argv[0]);
                        }
                        Ok(_) => {}
                    }
                }
            }
        }
    }
}

// ─── Syntax highlighting ──────────────────────────────────────────────────────

fn syntax_highlight(line: &str) -> String {
    let mut out   = String::with_capacity(line.len() * 2);
    let mut chars = line.chars().peekable();
    let mut tok   = String::new();
    let mut first = true;

    const KWS: &[&str] = &["if","then","else","fi","for","do","done","while",
                            "case","esac","function","in","select","until"];
    const BLT: &[&str] = &["echo","cd","export","source","alias","unalias",
                            "set","unset","read","exec","eval","exit",
                            "return","shift","type","which"];

    macro_rules! flush_tok {
        () => {
            if !tok.is_empty() {
                if KWS.contains(&tok.as_str()) {
                    out.push_str(&format!("{BOLD}{FG_YEL}{}{RESET}", tok));
                } else if first && BLT.contains(&tok.as_str()) {
                    out.push_str(&format!("{BOLD}{FG_GRN}{}{RESET}", tok));
                } else if first {
                    out.push_str(&format!("{BOLD}{}{RESET}", tok));
                } else if tok.starts_with('-') {
                    out.push_str(&format!("{FG_CYAN}{}{RESET}", tok));
                } else if tok.parse::<f64>().is_ok() {
                    out.push_str(&format!("{FG_MAG}{}{RESET}", tok));
                } else {
                    out.push_str(&tok);
                }
                tok.clear();
                first = false;
            }
        };
    }

    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                flush_tok!();
                out.push_str(FG_GRAY);
                out.push(ch);
                out.extend(chars.by_ref());
                out.push_str(RESET);
                return out;
            }
            '"' => {
                flush_tok!();
                let mut s = String::from(ch);
                for c in chars.by_ref() { s.push(c); if c == '"' { break; } }
                out.push_str(&format!("{FG_YEL}{s}{RESET}"));
            }
            '\'' => {
                flush_tok!();
                let mut s = String::from(ch);
                for c in chars.by_ref() { s.push(c); if c == '\'' { break; } }
                out.push_str(&format!("{FG_YEL}{s}{RESET}"));
            }
            '$' => {
                flush_tok!();
                let mut var = String::from('$');
                while chars.peek().map_or(false, |c| c.is_alphanumeric() || *c == '_') {
                    var.push(chars.next().unwrap());
                }
                out.push_str(&format!("{FG_MAG}{var}{RESET}"));
            }
            '|' | '>' | '<' | ';' | '&' => {
                flush_tok!();
                out.push_str(&format!("{FG_RED}{ch}{RESET}"));
                first = true;
            }
            ' ' | '\t' => { flush_tok!(); out.push(ch); }
            _   => tok.push(ch),
        }
    }
    flush_tok!();
    out
}

// ─── Terminal raw mode ────────────────────────────────────────────────────────

#[derive(Debug)]
enum Key {
    Char(char), Tab, Enter, Backspace,
    Up, Down, Left, Right,
    CtrlA, CtrlC, CtrlD, CtrlE, CtrlK, CtrlL, CtrlR, CtrlU, CtrlW,
    AltB, AltF,
    Unknown,
}

enum Action { Continue, Execute, Quit }

fn read_key() -> Key {
    let mut buf = [0u8; 8];
    let n = io::stdin().read(&mut buf).unwrap_or(0);
    if n == 0 { return Key::Unknown; }
    match &buf[..n] {
        [b'\r'] | [b'\n']          => Key::Enter,
        [b'\t']                    => Key::Tab,
        [0x7F] | [0x08]            => Key::Backspace,
        [0x01]                     => Key::CtrlA,
        [0x03]                     => Key::CtrlC,
        [0x04]                     => Key::CtrlD,
        [0x05]                     => Key::CtrlE,
        [0x0B]                     => Key::CtrlK,
        [0x0C]                     => Key::CtrlL,
        [0x12]                     => Key::CtrlR,
        [0x15]                     => Key::CtrlU,
        [0x17]                     => Key::CtrlW,
        [0x1B, b'b']               => Key::AltB,
        [0x1B, b'f']               => Key::AltF,
        [0x1B, b'[', b'A']        => Key::Up,
        [0x1B, b'[', b'B']        => Key::Down,
        [0x1B, b'[', b'C']        => Key::Right,
        [0x1B, b'[', b'D']        => Key::Left,
        [c] if (*c as char).is_control() => Key::Unknown,
        _ => {
            if let Ok(s) = std::str::from_utf8(&buf[..n]) {
                if let Some(ch) = s.chars().next() {
                    return Key::Char(ch);
                }
            }
            Key::Unknown
        }
    }
}

#[cfg(unix)]
fn enable_raw_mode() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(0, &mut t);
        t.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        t.c_oflag &= !libc::OPOST;
        t.c_cflag |=  libc::CS8;
        t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        t.c_cc[libc::VMIN]  = 1;
        t.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSAFLUSH, &t);
    }
}

#[cfg(unix)]
fn disable_raw_mode() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        libc::tcgetattr(0, &mut t);
        t.c_iflag |= libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP;
        t.c_oflag |= libc::OPOST;
        t.c_lflag |= libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG;
        libc::tcsetattr(0, libc::TCSAFLUSH, &t);
    }
}

#[cfg(not(unix))]
fn enable_raw_mode() {}
#[cfg(not(unix))]
fn disable_raw_mode() {}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn current_token_len(line: &str) -> usize {
    line.rfind(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '.')
        .map_or(0, |i| line.len() - i - 1)
}

fn print_banner() {
    println!("{BOLD}{FG_CYAN}");
    println!("  ███╗   ██╗███████╗██╗  ██╗██╗   ██╗███████╗");
    println!("  ████╗  ██║██╔════╝╚██╗██╔╝██║   ██║██╔════╝");
    println!("  ██╔██╗ ██║███████╗ ╚███╔╝ ██║   ██║███████╗");
    println!("  ██║╚██╗██║╚════██║ ██╔██╗ ██║   ██║╚════██║");
    println!("  ██║ ╚████║███████║██╔╝ ██╗╚██████╔╝███████║");
    println!("  ╚═╝  ╚═══╝╚══════╝╚═╝  ╚═╝ ╚═════╝ ╚══════╝{RESET}");
    println!("{FG_GRAY}  Noxisa Shell v0.1  •  Tab: AI complete  •  →: accept ghost text{RESET}\n");
}

fn print_help() {
    println!("{BOLD}Built-in commands:{RESET}");
    println!("  cd <dir>      Change directory");
    println!("  export K=V    Set environment variable");
    println!("  exit / quit   Exit the shell");
    println!("  help          Show this message");
    println!();
    println!("{BOLD}Key bindings:{RESET}");
    println!("  Tab           Complete / accept ghost text");
    println!("  →             Accept one word of ghost text");
    println!("  ↑/↓           Navigate history");
    println!("  Ctrl+R        Search history");
    println!("  Ctrl+A/E      Start / end of line");
    println!("  Ctrl+K        Delete to end of line");
    println!("  Ctrl+U        Delete to start of line");
    println!("  Ctrl+W        Delete previous word");
    println!("  Alt+B/F       Word-by-word navigation");
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let mut shell = Shell::new().await?;
    shell.run().await
}
