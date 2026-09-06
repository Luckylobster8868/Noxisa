//! MAC — Mandatory Access Control
//!
//! Type-enforcement rules: every subject (process) and object (file, socket, IPC)
//! has a LABEL. Policy rules say which source-label → target-label operations
//! are permitted.
//!
//! Example rules:
//!   allow browser_t  → net_socket_t  : connect, send, recv
//!   allow editor_t   → user_file_t   : read, write
//!   deny  any        → kernel_data_t : *
//!
//! Default policy: deny everything not explicitly allowed (whitelist).
//! This is the same model used by SELinux and AppArmor.

extern crate alloc;
use alloc::{string::String, vec::Vec};
use spin::RwLock;

// ─── Labels ───────────────────────────────────────────────────────────────────

/// A security label — a short string like "browser_t" or "kernel_t".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Label(pub String);

impl Label {
    pub fn new(s: &str) -> Self { Self(String::from(s)) }

    // Predefined system labels
    pub fn kernel()     -> Self { Self::new("kernel_t") }
    pub fn init()       -> Self { Self::new("init_t") }
    pub fn daemon()     -> Self { Self::new("daemon_t") }
    pub fn user()       -> Self { Self::new("user_t") }
    pub fn browser()    -> Self { Self::new("browser_t") }
    pub fn untrusted()  -> Self { Self::new("untrusted_t") }
    pub fn sys_file()   -> Self { Self::new("sys_file_t") }
    pub fn user_file()  -> Self { Self::new("user_file_t") }
    pub fn net_socket() -> Self { Self::new("net_socket_t") }
    pub fn ipc_obj()    -> Self { Self::new("ipc_obj_t") }
}

// ─── Access vectors ───────────────────────────────────────────────────────────

bitflags::bitflags! {
    /// Which operations an access vector covers.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Access: u32 {
        const READ      = 1 << 0;
        const WRITE     = 1 << 1;
        const EXECUTE   = 1 << 2;
        const CREATE    = 1 << 3;
        const DELETE    = 1 << 4;
        const CONNECT   = 1 << 5;
        const SEND      = 1 << 6;
        const RECV      = 1 << 7;
        const SIGNAL    = 1 << 8;
        const SETATTR   = 1 << 9;
        const GETATTR   = 1 << 10;
        const APPEND    = 1 << 11;
        const IOCTL     = 1 << 12;
    }
}

// ─── Policy rules ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Rule {
    /// Subject (process) label.
    pub subject:  Label,
    /// Object (resource) label.
    pub object:   Label,
    /// Permitted operations.
    pub allow:    Access,
}

impl Rule {
    pub fn allow(subject: Label, object: Label, access: Access) -> Self {
        Self { subject, object, allow: access }
    }
}

// ─── Policy database ──────────────────────────────────────────────────────────

struct MacPolicy {
    rules: Vec<Rule>,
    /// Per-process label assignments
    proc_labels: Vec<(u32 /*pid*/, Label)>,
    /// Per-resource label assignments (path → label)
    resource_labels: Vec<(String, Label)>,
}

impl MacPolicy {
    fn new() -> Self {
        Self {
            rules:            Vec::new(),
            proc_labels:      Vec::new(),
            resource_labels:  Vec::new(),
        }
    }

    fn load_default_policy(&mut self) {
        use Access as A;

        // ── Kernel and init: unrestricted ──
        self.add(Rule::allow(Label::kernel(), Label::sys_file(),  A::all()));
        self.add(Rule::allow(Label::kernel(), Label::user_file(), A::all()));
        self.add(Rule::allow(Label::init(),   Label::sys_file(),  A::all()));
        self.add(Rule::allow(Label::init(),   Label::user_file(), A::all()));
        self.add(Rule::allow(Label::init(),   Label::ipc_obj(),   A::all()));

        // ── Daemons: read system, write their own files ──
        self.add(Rule::allow(Label::daemon(), Label::sys_file(),
            A::READ | A::GETATTR));
        self.add(Rule::allow(Label::daemon(), Label::ipc_obj(),
            A::SEND | A::RECV | A::CREATE));
        self.add(Rule::allow(Label::daemon(), Label::net_socket(),
            A::CONNECT | A::SEND | A::RECV));

        // ── Users: read/write user files, no system files ──
        self.add(Rule::allow(Label::user(), Label::user_file(),
            A::READ | A::WRITE | A::CREATE | A::DELETE | A::GETATTR | A::SETATTR | A::APPEND));
        self.add(Rule::allow(Label::user(), Label::sys_file(),
            A::READ | A::GETATTR));
        self.add(Rule::allow(Label::user(), Label::ipc_obj(),
            A::SEND | A::RECV));
        self.add(Rule::allow(Label::user(), Label::net_socket(),
            A::CONNECT | A::SEND | A::RECV));

        // ── Browser: network + user files, NOT system files ──
        self.add(Rule::allow(Label::browser(), Label::net_socket(),
            A::CONNECT | A::SEND | A::RECV));
        self.add(Rule::allow(Label::browser(), Label::user_file(),
            A::READ | A::WRITE | A::CREATE | A::GETATTR));

        // ── Untrusted: almost nothing ──
        self.add(Rule::allow(Label::untrusted(), Label::user_file(),
            A::READ | A::GETATTR));
        // No network, no system files, no IPC
    }

    fn add(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Check if subject may perform access on object. O(n) rule scan.
    fn check(&self, subject: &Label, object: &Label, access: Access) -> bool {
        for rule in &self.rules {
            if &rule.subject == subject && &rule.object == object {
                if rule.allow.contains(access) {
                    return true;
                }
            }
        }
        false // default deny
    }
}

static POLICY: RwLock<Option<MacPolicy>> = RwLock::new(None);

/// Initialise MAC with the default policy. Call once during boot.
pub fn init() {
    let mut policy = MacPolicy::new();
    policy.load_default_policy();
    policy.proc_labels.push((0, Label::kernel()));
    *POLICY.write() = Some(policy);
    crate::kprintln!("[mac] Mandatory access control initialised");
}

/// Assign a security label to a process.
pub fn label_process(pid: u32, label: Label) {
    if let Some(p) = POLICY.write().as_mut() {
        if let Some(entry) = p.proc_labels.iter_mut().find(|(id, _)| *id == pid) {
            entry.1 = label;
        } else {
            p.proc_labels.push((pid, label));
        }
    }
}

/// Assign a security label to a resource path.
pub fn label_resource(path: &str, label: Label) {
    if let Some(p) = POLICY.write().as_mut() {
        let path = String::from(path);
        if let Some(entry) = p.resource_labels.iter_mut().find(|(k, _)| *k == path) {
            entry.1 = label;
        } else {
            p.resource_labels.push((path, label));
        }
    }
}

/// Check whether `pid` may perform `access` on `resource_path`.
/// Returns true if allowed, false if denied.
pub fn check(pid: u32, resource_path: &str, access: Access) -> bool {
    let guard = POLICY.read();
    let Some(policy) = guard.as_ref() else { return true }; // if not initialised, allow

    // Look up process label
    let subj = match policy.proc_labels.iter().find(|(p, _)| *p == pid) {
        Some((_, l)) => l,
        None         => return false, // unknown process — deny
    };

    // Look up resource label (longest prefix match)
    let obj = policy.resource_labels.iter()
        .filter(|(k, _)| resource_path.starts_with(k.as_str()))
        .max_by_key(|(k, _)| k.len())
        .map(|(_, l)| l);

    let obj = match obj {
        Some(l) => l,
        None    => return false, // unlabelled resource — deny by default
    };

    policy.check(subj, obj, access)
}

/// Remove a process's label on exit.
pub fn deregister(pid: u32) {
    if let Some(p) = POLICY.write().as_mut() {
        p.proc_labels.retain(|(id, _)| *id != pid);
    }
}
