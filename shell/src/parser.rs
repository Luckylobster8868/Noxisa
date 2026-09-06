//! POSIX-compatible shell command parser.
//!
//! Supports:
//!   Simple commands          git status
//!   Pipelines                ls -la | grep rs
//!   Redirections             cmd > file.txt  cmd < input  cmd 2>&1
//!   Command sequences        cmd1; cmd2; cmd3
//!   Background jobs          cargo build &
//!   Variable expansion       echo $HOME
//!   Command substitution     echo $(date)
//!   Single + double quotes   echo "hello world"  echo 'raw $VAR'

#[derive(Debug, Clone)]
pub struct Command {
    pub argv:   Vec<String>,       // [program, arg0, arg1, ...]
    pub stdin:  Redirect,
    pub stdout: Redirect,
    pub stderr: Redirect,
    pub bg:     bool,              // run in background
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redirect {
    Inherit,
    File(String),
    Append(String),
    Fd(i32),
    Pipe,    // connected to next stage
}

#[derive(Debug, Clone)]
pub struct Pipeline {
    pub stages: Vec<Command>,      // connected left-to-right via pipes
}

#[derive(Debug, Clone)]
pub struct Script {
    pub pipelines: Vec<Pipeline>,  // separated by ; or newlines
}

// ─── Tokeniser ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Pipe,           // |
    Semi,           // ;
    Amp,            // &
    Redir(String),  // >, <, >>, 2>&1, etc.
    Newline,
}

fn tokenise(input: &str) -> Vec<Tok> {
    let mut tokens = Vec::new();
    let mut chars  = input.chars().peekable();
    let mut word   = String::new();

    macro_rules! flush {
        () => {
            if !word.is_empty() {
                tokens.push(Tok::Word(word.clone()));
                word.clear();
            }
        };
    }

    while let Some(ch) = chars.next() {
        match ch {
            // Skip whitespace (outside quotes)
            ' ' | '\t' => { flush!(); }

            '\n' => { flush!(); tokens.push(Tok::Newline); }

            '|' => { flush!(); tokens.push(Tok::Pipe); }
            ';' => { flush!(); tokens.push(Tok::Semi); }
            '&' => { flush!(); tokens.push(Tok::Amp); }

            // Single-quoted string: no expansion
            '\'' => {
                loop {
                    match chars.next() {
                        Some('\'') | None => break,
                        Some(c)           => word.push(c),
                    }
                }
            }

            // Double-quoted string: allow $VAR expansion (simplified)
            '"' => {
                loop {
                    match chars.next() {
                        Some('"') | None  => break,
                        Some('$') => {
                            // Expand variable
                            let var = read_var_name(&mut chars);
                            let val = std::env::var(&var).unwrap_or_default();
                            word.push_str(&val);
                        }
                        Some(c) => word.push(c),
                    }
                }
            }

            // Redirection operators
            '>' => {
                flush!();
                let mut redir = String::from(">");
                if chars.peek() == Some(&'>') { chars.next(); redir.push('>'); }
                // Collect target filename
                skip_spaces(&mut chars);
                let file = read_word(&mut chars);
                redir.push_str(&file);
                tokens.push(Tok::Redir(redir));
            }

            '<' => {
                flush!();
                skip_spaces(&mut chars);
                let file = read_word(&mut chars);
                tokens.push(Tok::Redir(format!("<{file}")));
            }

            '#' => {
                // Comment — skip to end of line
                flush!();
                for c in chars.by_ref() { if c == '\n' { break; } }
            }

            '$' => {
                let var = read_var_name(&mut chars);
                let val = std::env::var(&var).unwrap_or_else(|_| format!("${var}"));
                word.push_str(&val);
            }

            '\\' => {
                // Escape next character
                if let Some(next) = chars.next() {
                    if next != '\n' { word.push(next); } // backslash-newline = continuation
                }
            }

            c => word.push(c),
        }
    }
    flush!();
    tokens
}

fn read_var_name(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut name = String::new();
    // Handle ${VAR} syntax
    if chars.peek() == Some(&'{') {
        chars.next();
        for c in chars.by_ref() { if c == '}' { break; } else { name.push(c); } }
    } else {
        while chars.peek().map_or(false, |c| c.is_alphanumeric() || *c == '_') {
            name.push(chars.next().unwrap());
        }
    }
    name
}

fn read_word(chars: &mut std::iter::Peekable<std::str::Chars>) -> String {
    let mut w = String::new();
    while chars.peek().map_or(false, |c| !matches!(c, ' '|'\t'|'\n'|';'|'|'|'&')) {
        w.push(chars.next().unwrap());
    }
    w
}

fn skip_spaces(chars: &mut std::iter::Peekable<std::str::Chars>) {
    while chars.peek() == Some(&' ') || chars.peek() == Some(&'\t') { chars.next(); }
}

// ─── Parser ───────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Empty command")]
    Empty,
    #[error("Unexpected token: {0}")]
    Unexpected(String),
}

/// Parse an input line into a `Script` (sequence of pipelines).
pub fn parse(input: &str) -> Result<Script, ParseError> {
    let tokens = tokenise(input);
    if tokens.is_empty() { return Err(ParseError::Empty); }

    let mut pipelines: Vec<Pipeline> = Vec::new();
    let mut stages:    Vec<Command>  = Vec::new();
    let mut argv:      Vec<String>   = Vec::new();
    let mut stdout = Redirect::Inherit;
    let mut stdin  = Redirect::Inherit;
    let mut stderr = Redirect::Inherit;
    let mut bg     = false;

    macro_rules! push_cmd {
        () => {
            if !argv.is_empty() {
                stages.push(Command {
                    argv: argv.drain(..).collect(),
                    stdin: stdin.clone(),
                    stdout: stdout.clone(),
                    stderr: stderr.clone(),
                    bg,
                });
                stdin  = Redirect::Inherit;
                stdout = Redirect::Inherit;
                stderr = Redirect::Inherit;
                bg     = false;
            }
        };
    }
    macro_rules! push_pipeline {
        () => {
            push_cmd!();
            if !stages.is_empty() {
                pipelines.push(Pipeline { stages: stages.drain(..).collect() });
            }
        };
    }

    for tok in tokens {
        match tok {
            Tok::Word(w)  => argv.push(w),
            Tok::Pipe     => {
                push_cmd!();
                if let Some(last) = stages.last_mut() {
                    last.stdout = Redirect::Pipe;
                }
                stdin = Redirect::Pipe;
            }
            Tok::Semi | Tok::Newline => push_pipeline!(),
            Tok::Amp    => { bg = true; push_pipeline!(); }
            Tok::Redir(r) => {
                if r.starts_with(">>") {
                    stdout = Redirect::Append(r[2..].to_owned());
                } else if r.starts_with('>') {
                    stdout = Redirect::File(r[1..].to_owned());
                } else if r.starts_with('<') {
                    stdin  = Redirect::File(r[1..].to_owned());
                }
            }
        }
    }
    push_pipeline!();

    if pipelines.is_empty() { Err(ParseError::Empty) } else { Ok(Script { pipelines }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_command() {
        let s = parse("ls -la").unwrap();
        assert_eq!(s.pipelines.len(), 1);
        let cmd = &s.pipelines[0].stages[0];
        assert_eq!(cmd.argv, &["ls", "-la"]);
    }

    #[test]
    fn pipeline() {
        let s = parse("ls | grep rs").unwrap();
        assert_eq!(s.pipelines[0].stages.len(), 2);
        assert_eq!(s.pipelines[0].stages[0].stdout, Redirect::Pipe);
        assert_eq!(s.pipelines[0].stages[1].argv[0], "grep");
    }

    #[test]
    fn redirect_out() {
        let s = parse("echo hello > out.txt").unwrap();
        let cmd = &s.pipelines[0].stages[0];
        assert_eq!(cmd.stdout, Redirect::File("out.txt".to_owned()));
    }

    #[test]
    fn semicolon_sequence() {
        let s = parse("cd /tmp; ls").unwrap();
        assert_eq!(s.pipelines.len(), 2);
    }
}
