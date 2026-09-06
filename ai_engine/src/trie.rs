//! Completion Trie — O(k) prefix lookup, k = query token length
//!
//! Stores language keywords, stdlib symbols, snippets, and user identifiers.
//! Children stored in a HashMap per node (sparse, memory-efficient).
//! Completions are scored; top-N returned sorted by score descending.

use std::collections::HashMap;

// ─── Category ─────────────────────────────────────────────────────────────────

/// Classification of a completion item — used for filtering and scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Keyword,
    Builtin,
    Snippet,
    Stdlib,
    Identifier, // learned from user's current file
    Path,
}

// ─── Trie node ────────────────────────────────────────────────────────────────

struct Node {
    children: HashMap<char, usize>,
    /// If this node ends a completion, store (text, score, category).
    terminal: Option<(String, f32, Category)>,
}

impl Node {
    fn new() -> Self {
        Self { children: HashMap::new(), terminal: None }
    }
}

// ─── CompletionTrie ───────────────────────────────────────────────────────────

pub struct CompletionTrie {
    nodes: Vec<Node>,
}

impl CompletionTrie {
    pub fn new() -> Self {
        // Node 0 is the root
        Self { nodes: vec![Node::new()] }
    }

    // ── Insertion ─────────────────────────────────────────────────────────

    /// Insert a completion string with its score and category.
    /// Time: O(|text|)
    pub fn insert(&mut self, text: &str, score: f32, cat: Category) {
        let mut idx = 0usize;
        for ch in text.chars() {
            idx = if let Some(&next) = self.nodes[idx].children.get(&ch) {
                next
            } else {
                let next = self.nodes.len();
                self.nodes.push(Node::new());
                self.nodes[idx].children.insert(ch, next);
                next
            };
        }
        // Only overwrite if new score is higher
        let replace = self.nodes[idx].terminal
            .as_ref()
            .map_or(true, |(_, s, _)| score > *s);
        if replace {
            self.nodes[idx].terminal = Some((text.to_owned(), score, cat));
        }
    }

    // ── Query ─────────────────────────────────────────────────────────────

    /// Return up to `max` completions for the current token in `line`.
    /// Result is sorted by score descending.
    /// Time: O(|token| + matches)
    pub fn complete(&self, line: &str, max: usize) -> Vec<(String, f32)> {
        let token = current_token(line);
        if token.is_empty() { return Vec::new(); }

        // Walk to the prefix node
        let mut node_idx = 0usize;
        for ch in token.chars() {
            match self.nodes[node_idx].children.get(&ch) {
                Some(&next) => node_idx = next,
                None        => return Vec::new(),
            }
        }

        // BFS to collect all terminals in the subtree
        let mut results: Vec<(String, f32)> = Vec::new();
        let mut queue = vec![node_idx];
        let limit = max * 8; // over-collect then sort + truncate

        while !queue.is_empty() && results.len() < limit {
            let mut next_queue = Vec::new();
            for idx in queue.drain(..) {
                let node = &self.nodes[idx];
                if let Some((text, score, _)) = &node.terminal {
                    results.push((text.clone(), *score));
                }
                next_queue.extend(node.children.values().copied());
            }
            queue = next_queue;
        }

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then(a.0.cmp(&b.0)));
        results.truncate(max);
        results
    }

    /// Learn a user-typed identifier for future completions.
    /// Only accepts identifiers ≥ 3 chars; silently ignores others.
    pub fn learn(&mut self, ident: &str) {
        let valid = ident.len() >= 3
            && ident.chars().all(|c| c.is_alphanumeric() || c == '_');
        if valid {
            self.insert(ident, 0.65, Category::Identifier);
        }
    }

    /// Number of nodes (diagnostic).
    pub fn node_count(&self) -> usize { self.nodes.len() }

    // ── Bulk load ─────────────────────────────────────────────────────────

    /// Populate with built-in completions for all supported languages.
    /// NOTE: Takes `&mut self` — no unsafe self-cast needed (was a bug in v1).
    pub fn load_builtins(&mut self) {
        self.load_rust();
        self.load_python();
        self.load_shell();
        self.load_paths();
    }

    fn load_rust(&mut self) {
        // Keywords
        for kw in &["fn","let","mut","pub","use","mod","struct","enum","trait",
                    "impl","for","in","if","else","match","loop","while","return",
                    "async","await","move","ref","dyn","where","type","const",
                    "static","unsafe","extern","self","Self","super","crate",
                    "Box","Arc","Rc","Vec","HashMap","HashSet","Option","Result",
                    "Some","None","Ok","Err","true","false",
        ] {
            self.insert(kw, 0.90, Category::Keyword);
        }

        // Common stdlib
        for sym in &[
            "Vec::new()", "Vec::with_capacity(", "HashMap::new()",
            "Arc::new(", "Mutex::new(", "RwLock::new(",
            "String::new()", "String::from(", "format!(",
            "println!(", "eprintln!(", "todo!()", "unimplemented!()",
            "std::fs::read_to_string(", "std::fs::write(",
            "std::env::var(", "std::path::Path::new(",
            "tokio::spawn(", "anyhow::bail!(", "anyhow::anyhow!(",
        ] {
            self.insert(sym, 0.82, Category::Stdlib);
        }

        // Snippets
        for (s, score) in &[
            ("fn main() {\n    \n}", 0.95f32),
            ("#[derive(Debug, Clone, PartialEq, Eq)]\n", 0.89),
            ("#[tokio::main]\nasync fn main() -> anyhow::Result<()> {\n    Ok(())\n}", 0.88),
            ("impl Default for  {\n    fn default() -> Self {\n        Self {}\n    }\n}", 0.84),
            ("for item in iter {\n    \n}", 0.85),
            ("match expr {\n    Some(v) => v,\n    None    => return,\n}", 0.84),
            ("if let Some(x) =  {\n    \n}", 0.83),
        ] {
            self.insert(s, *score, Category::Snippet);
        }
    }

    fn load_python(&mut self) {
        for kw in &["def","class","import","from","return","if","elif","else",
                    "for","while","with","as","try","except","finally","raise",
                    "yield","async","await","lambda","pass","None","True","False",
                    "print(","len(","range(","enumerate(","zip(","isinstance(",
                    "type(","list(","dict(","set(","tuple(","int(","str(","float(",
        ] {
            self.insert(kw, 0.88, Category::Keyword);
        }
        for (s, score) in &[
            ("def func(args):\n    pass", 0.87f32),
            ("class MyClass:\n    def __init__(self):\n        pass", 0.85),
            ("if __name__ == '__main__':\n    main()", 0.91),
            ("import numpy as np", 0.83),
            ("import pandas as pd", 0.83),
        ] {
            self.insert(s, *score, Category::Snippet);
        }
    }

    fn load_shell(&mut self) {
        for cmd in &[
            "ls","ls -la","ls -lh","cd","pwd","echo","cat","grep","find",
            "sed","awk","curl","wget","git","make","cargo","cargo build",
            "cargo build --release","cargo test","cargo run","cargo clippy",
            "rustc","python3","node","npm","docker","kubectl","ssh","scp",
            "git status","git add .","git commit -m ","git push","git pull",
            "git log --oneline","git diff","git stash","git checkout -b ",
        ] {
            self.insert(cmd, 0.85, Category::Builtin);
        }
    }

    fn load_paths(&mut self) {
        for p in &["/usr/","/usr/bin/","/usr/lib/","/etc/","/home/",
                   "/var/","/tmp/","/proc/","/sys/",
                   "/usr/share/nexus-os/","/usr/share/nexus-os/models/",
        ] {
            self.insert(p, 0.60, Category::Path);
        }
    }
}

/// Extract the token currently being typed (last word on the line).
fn current_token(line: &str) -> &str {
    let end = line.len();
    let start = line.rfind(|c: char| {
        !c.is_alphanumeric()
            && c != '_'
            && c != ':'
            && c != '.'
            && c != '/'
            && c != '!'
            && c != '('
    })
    .map_or(0, |i| i + 1);
    &line[start..end]
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_insert_complete() {
        let mut t = CompletionTrie::new();
        t.insert("fn",     0.90, Category::Keyword);
        t.insert("for",    0.85, Category::Keyword);
        t.insert("format", 0.80, Category::Stdlib);

        let r = t.complete("fo", 10);
        assert!(r.iter().any(|(s, _)| s == "for"));
        assert!(r.iter().any(|(s, _)| s == "format"));
    }

    #[test]
    fn score_ordering() {
        let mut t = CompletionTrie::new();
        t.insert("HashMap", 0.90, Category::Stdlib);
        t.insert("HashSet",  0.70, Category::Stdlib);
        let r = t.complete("Hash", 5);
        assert_eq!(r[0].0, "HashMap");
    }

    #[test]
    fn current_token_extraction() {
        assert_eq!(current_token("let x = Vec::wi"), "Vec::wi");
        assert_eq!(current_token("for item in it"),  "it");
        assert_eq!(current_token("println!("),       "println!(");
    }

    #[test]
    fn learn_identifier() {
        let mut t = CompletionTrie::new();
        t.learn("my_variable");
        let r = t.complete("my_", 5);
        assert!(r.iter().any(|(s, _)| s == "my_variable"));
    }

    #[test]
    fn short_ident_not_learned() {
        let mut t = CompletionTrie::new();
        t.learn("ab"); // too short
        let r = t.complete("ab", 5);
        assert!(r.is_empty());
    }
}
