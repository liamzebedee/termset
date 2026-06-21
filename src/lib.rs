//! manyterm — a workspace-organized mini terminal emulator.
//! (crate `termset_cli`; binary `terms`.)
//!
//! Backend : `alacritty_terminal` (PTY + VT/ANSI state machine + parser thread)
//! Frontend: `winit` (window + keyboard/mouse) + `softbuffer` (CPU framebuffer)
//!           + `fontdue` (glyph rasterization)
//!
//! Architecture is functional-core / imperative-shell:
//!
//!   * The workspace is a *rose tree* parsed from the `workspace` file. The
//!     tree and every decision over it (which rows to draw, which leaves a
//!     group contains, what to start/stop, whether something is already
//!     running) are **pure functions** — see the `core` section.
//!   * Only PTY spawning, byte I/O, `/proc` observation and drawing are
//!     effectful; those live in the `shell` section (`State` / `App`).
//!
//! Run with: `cargo run --release`

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop as PtyEventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Flags, Hyperlink};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::tty::{self, Options as PtyOptions};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

pub mod testkit;

mod config;
mod ui;
use config::*;
use ui::*;

const FONT_PX: f32 = 16.0;

/// Empty-terminal hint, with the platform's own "new shell" shortcut.
#[cfg(target_os = "macos")]
const NO_SESSION_HINT: &str =
    "No session here. Right-click a node \u{2192} Start, or \u{2318}T for a shell.";
#[cfg(not(target_os = "macos"))]
const NO_SESSION_HINT: &str =
    "No session here. Right-click a node \u{2192} Start, or Ctrl+Shift+T for a shell.";

// ===========================================================================
// core — the workspace as a pure rose tree
// ===========================================================================

/// Stable identifier for a position in the tree (arena index). Stable for the
/// lifetime of the process, including across spawn/exit of a leaf's session.
type NodeId = usize;

/// What a node *is*. `Leaf` carries the spec needed to run it.
#[derive(Clone)]
enum Kind {
    /// The invisible forest root (`workspaces`).
    Root,
    /// A folder. May contain groups and leaves. Has no command of its own.
    Group,
    /// A runnable session: a working directory and a default command. An
    /// empty `command` means "just a shell here" (Scratch/Transient/new tab).
    Leaf { workdir: PathBuf, command: String },
}

/// One node of the workspace tree. `expanded`/`dynamic` are the only mutable
/// bits and they are explicit, never hidden behind a traversal.
struct Node {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    name: String,
    kind: Kind,
    /// Groups only: whether children are shown. Ignored for leaves.
    expanded: bool,
    /// Created at runtime (a new scratch tab) rather than from the spec file.
    /// Dynamic leaves are removed from the tree when their session exits.
    dynamic: bool,
    /// Never serialized to the workspace file — a purely runtime helper tab
    /// (the background "edit layout" nano session). Recreated each launch.
    volatile: bool,
}

/// The workspace. An arena of `Node`s plus the root index. The arena layout is
/// the idiomatic Rust spelling of an immutable-shaped tree: structure is set
/// up once, traversals below are pure reads, mutations are localized.
struct Tree {
    nodes: Vec<Node>,
    root: NodeId,
}

impl Tree {
    fn push(&mut self, parent: Option<NodeId>, name: String, kind: Kind, dynamic: bool) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            parent,
            children: Vec::new(),
            name,
            kind,
            expanded: true,
            dynamic,
            volatile: false,
        });
        if let Some(p) = parent {
            self.nodes[p].children.push(id);
        }
        id
    }

    fn is_group(&self, id: NodeId) -> bool {
        matches!(self.nodes[id].kind, Kind::Group | Kind::Root)
    }
    fn is_leaf(&self, id: NodeId) -> bool {
        matches!(self.nodes[id].kind, Kind::Leaf { .. })
    }

    /// Leaf spec, if `id` is a leaf.
    fn leaf_spec(&self, id: NodeId) -> Option<(&Path, &str)> {
        match &self.nodes[id].kind {
            Kind::Leaf { workdir, command } => Some((workdir.as_path(), command.as_str())),
            _ => None,
        }
    }

    /// Mutable handle to a leaf's default command (the inspector edits this).
    fn command_mut(&mut self, id: NodeId) -> Option<&mut String> {
        match &mut self.nodes[id].kind {
            Kind::Leaf { command, .. } => Some(command),
            _ => None,
        }
    }

    /// Set a leaf's working directory (the inspector / use-cwd button). No-op
    /// on a group.
    fn set_workdir(&mut self, id: NodeId, dir: PathBuf) {
        if let Kind::Leaf { workdir, .. } = &mut self.nodes[id].kind {
            *workdir = dir;
        }
    }

    /// Current text of an inspector field for `id`. `Command`/`Directory` are
    /// `None` on a group (it has neither — those fields render disabled).
    fn field_text(&self, id: NodeId, f: Field) -> Option<String> {
        match f {
            Field::Title => Some(self.nodes[id].name.clone()),
            Field::Command => self.leaf_spec(id).map(|(_, c)| c.to_string()),
            Field::Dir => self
                .leaf_spec(id)
                .map(|(w, _)| w.display().to_string()),
        }
    }

    /// DFS over visible nodes (groups gate their subtree via `expanded`). This
    /// is the catamorphism the sidebar render and hit-testing both fold over,
    /// so the picture on screen and the click map can never disagree.
    ///
    /// `reveal` is the currently-selected node: a *volatile* node (the pre-warmed
    /// "Edit Config" session) is hidden from the sidebar unless it equals
    /// `reveal` — i.e. it only shows while it's the active tab. Pass an invalid
    /// id (e.g. the root) to hide all volatile nodes.
    fn rows(&self, reveal: NodeId) -> Vec<Row> {
        fn go(t: &Tree, id: NodeId, depth: usize, reveal: NodeId, out: &mut Vec<Row>) {
            for &c in &t.nodes[id].children {
                let n = &t.nodes[c];
                if n.volatile && c != reveal {
                    continue; // hidden background tab (e.g. Edit Config)
                }
                let is_group = matches!(n.kind, Kind::Group);
                out.push(Row {
                    id: c,
                    name: n.name.clone(),
                    is_group,
                    depth,
                });
                if is_group && n.expanded {
                    go(t, c, depth + 1, reveal, out);
                }
            }
        }
        let mut out = Vec::new();
        go(self, self.root, 0, reveal, &mut out);
        out
    }

    /// Every leaf in `id`'s subtree (a leaf yields itself). The fold that turns
    /// "Start on a group" into "start each of these leaves".
    fn leaves(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        fn go(t: &Tree, id: NodeId, out: &mut Vec<NodeId>) {
            if t.is_leaf(id) {
                out.push(id);
                return;
            }
            for &c in &t.nodes[id].children {
                go(t, c, out);
            }
        }
        go(self, id, &mut out);
        out
    }

    /// The group a new session should be attached to given the current
    /// selection: a selected group is its own context; a selected leaf hands
    /// off to its enclosing group.
    fn group_for_new(&self, sel: NodeId) -> NodeId {
        if self.is_group(sel) {
            sel
        } else {
            self.nodes[sel].parent.unwrap_or(self.root)
        }
    }

    /// First leaf in the subtree, in DFS order — used to pick what terminal to
    /// show when a group (rather than a leaf) is selected.
    fn first_leaf(&self, id: NodeId) -> Option<NodeId> {
        self.leaves(id).into_iter().next()
    }

    /// The sibling immediately before `id` among its parent's children, if any
    /// (i.e. `None` when `id` is the first child). Used to pick what to select
    /// after closing a session.
    fn prev_sibling(&self, id: NodeId) -> Option<NodeId> {
        let p = self.nodes[id].parent?;
        let kids = &self.nodes[p].children;
        let i = kids.iter().position(|&c| c == id)?;
        i.checked_sub(1).map(|j| kids[j])
    }

    /// `Group / Sub / Leaf` path string for the header bar.
    fn path(&self, id: NodeId) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == self.root {
                break;
            }
            parts.push(self.nodes[c].name.clone());
            cur = self.nodes[c].parent;
        }
        parts.reverse();
        parts.join("  /  ")
    }
}

/// A flattened, render-ready view of one visible tree node.
struct Row {
    id: NodeId,
    name: String,
    is_group: bool,
    /// Tree depth: `0` for a top-level node (a section, or a standalone
    /// top-level session like "Edit Config"), `1+` for nested sessions.
    depth: usize,
}


/// What the shell observed about a session's PTY, gathered from `/proc`.
/// Effectful to *produce* (`observe`), but a plain value the planner reasons
/// about purely.
#[derive(Clone, Default)]
struct Obs {
    /// The shell has a foreground child — i.e. a command is running, not just
    /// an idle prompt.
    foreground: bool,
    cmd: Option<String>,
}

/// Pure predicate: should `plan_start` *skip* running `command` here because
/// it's already running? True when the shell has a foreground job whose
/// command line mentions the configured program. Conservative: if we can't
/// tell, we don't skip (running again is recoverable with Ctrl+C; silently
/// not starting is not).
fn already_running(obs: &Obs, command: &str) -> bool {
    if !obs.foreground || command.trim().is_empty() {
        return false;
    }
    let prog = command
        .trim()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches("./");
    let prog = Path::new(prog)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(prog);
    !prog.is_empty() && obs.cmd.as_deref().is_some_and(|c| c.contains(prog))
}

/// An editable text field in the right-hand inspector.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Command,
    Dir,
}

impl Field {
    /// Inspector field order (also its row index in `field_rects`).
    const ALL: [Field; 3] = [Field::Title, Field::Command, Field::Dir];
    fn index(self) -> usize {
        Field::ALL.iter().position(|&x| x == self).unwrap()
    }
}

/// A keystroke against a single-line text field, normalized away from winit.
enum Edit {
    Ins(char),
    Back,
    Del,
    Left,
    Right,
    Home,
    End,
}

/// Pure: apply one `Edit` to `(text, caret)` and return the new pair. `caret`
/// is a character index in `0..=len`. Single-line, UTF-8 safe (operates on
/// `char`s, not bytes). The inspector's whole editing model is this function;
/// everything else is just plumbing keys into it and the result back out.
fn apply_edit(text: &str, caret: usize, e: Edit) -> (String, usize) {
    let mut v: Vec<char> = text.chars().collect();
    let mut c = caret.min(v.len());
    match e {
        Edit::Ins(ch) => {
            v.insert(c, ch);
            c += 1;
        }
        Edit::Back if c > 0 => {
            v.remove(c - 1);
            c -= 1;
        }
        Edit::Del if c < v.len() => {
            v.remove(c);
        }
        Edit::Left => c = c.saturating_sub(1),
        Edit::Right if c < v.len() => c += 1,
        Edit::Home => c = 0,
        Edit::End => c = v.len(),
        _ => {}
    }
    (v.into_iter().collect(), c)
}

/// One lifecycle effect the pure planner emits for the shell to execute.
#[derive(Clone)]
enum Action {
    /// Spawn an idle PTY for this leaf (sets cwd, runs no command).
    Spawn(NodeId),
    /// Type the leaf's default command + Enter into its session.
    Run(NodeId),
    /// Send Ctrl+C to the leaf's session.
    Sigint(NodeId),
}

/// Pure: starting `target` means, for every leaf under it, ensuring a session
/// exists and then running its command unless it's already running. Spawn
/// precedes Run for the same leaf so a re-started (previously exited) spec
/// leaf comes back. Groups fan out to all descendant leaves — each runs in its
/// own PTY thread, so this is parallel for free.
fn plan_start(
    tree: &Tree,
    has_session: &dyn Fn(NodeId) -> bool,
    obs: &HashMap<NodeId, Obs>,
    target: NodeId,
) -> Vec<Action> {
    let mut acts = Vec::new();
    for leaf in tree.leaves(target) {
        let Some((_, command)) = tree.leaf_spec(leaf) else {
            continue;
        };
        if !has_session(leaf) {
            acts.push(Action::Spawn(leaf));
        } else if obs.get(&leaf).is_some_and(|o| already_running(o, command)) {
            continue;
        }
        if !command.trim().is_empty() {
            acts.push(Action::Run(leaf));
        }
    }
    acts
}

/// Pure: stopping `target` sends Ctrl+C to every leaf under it that has a live
/// session.
fn plan_stop(tree: &Tree, has_session: &dyn Fn(NodeId) -> bool, target: NodeId) -> Vec<Action> {
    tree.leaves(target)
        .into_iter()
        .filter(|&l| has_session(l))
        .map(Action::Sigint)
        .collect()
}

/// A window-level command triggered by a keyboard shortcut, independent of the
/// physical keys that produce it. The keymap is OS-specific (see
/// [`match_shortcut`]); everything downstream of it is not — this is the
/// abstraction that lets the same actions wear ⌘ on macOS and Ctrl+Shift
/// elsewhere.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shortcut {
    Copy,
    Paste,
    NewTab,
    CloseTab,
    SelectPrev,
    SelectNext,
    Collapse,
    Expand,
    Start,
    Stop,
    ToggleSidebar,
    EditLayout,
    Quit,
}

/// Map a modified keystroke to a [`Shortcut`], or `None` to pass it through to
/// the terminal. Bindings are the intuitive ones for each platform:
///
/// * **macOS** uses ⌘ (Command), like every native mac app: ⌘C copy, ⌘V paste,
///   ⌘T new tab, ⌘W close, ⌘↑/⌘↓ select previous/next tree node, ⌘←/⌘→
///   collapse/expand the selected group, ⌘R run/start, ⌘. stop, ⌘B toggle the
///   sidebar, ⌘, edit layout (the layout file in an editor tab), ⌘Q quit. ⌘
///   never collides with the shell's own Ctrl- control codes, so all
///   of these are safe to claim.
/// * **Elsewhere** there is no ⌘, so the terminal convention Ctrl+Shift is used
///   (plain Ctrl+C etc. must stay free for the shell): Ctrl+Shift+C/V/T/W for
///   copy/paste/new/close, Ctrl+Shift+↑/↓ select previous/next node,
///   Ctrl+Shift+←/→ collapse/expand the selected group, Ctrl+Shift+B toggle the
///   sidebar, Ctrl+Shift+R start, Ctrl+Shift+X stop, Ctrl+Shift+, edit layout,
///   Ctrl+Shift+Q quit. (Shift+`,` reports as `<` on many layouts, so both are
///   accepted.)
fn match_shortcut(mods: ModifiersState, key: &Key) -> Option<Shortcut> {
    let ch = |name: &str| matches!(key, Key::Character(c) if c.eq_ignore_ascii_case(name));
    #[cfg(target_os = "macos")]
    {
        if !mods.super_key() {
            return None;
        }
        if ch(".") {
            return Some(Shortcut::Stop); // ⌘. — the mac "cancel" gesture
        }
        Some(match key {
            _ if ch("c") => Shortcut::Copy,
            _ if ch("v") => Shortcut::Paste,
            _ if ch("t") => Shortcut::NewTab,
            _ if ch("w") => Shortcut::CloseTab,
            Key::Named(NamedKey::ArrowUp) => Shortcut::SelectPrev,
            Key::Named(NamedKey::ArrowDown) => Shortcut::SelectNext,
            Key::Named(NamedKey::ArrowLeft) => Shortcut::Collapse,
            Key::Named(NamedKey::ArrowRight) => Shortcut::Expand,
            _ if ch("b") => Shortcut::ToggleSidebar,
            _ if ch("r") => Shortcut::Start,
            _ if ch(",") => Shortcut::EditLayout,
            _ if ch("q") => Shortcut::Quit,
            _ => return None,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        if !(mods.control_key() && mods.shift_key()) {
            return None;
        }
        Some(match key {
            _ if ch("c") => Shortcut::Copy,
            _ if ch("v") => Shortcut::Paste,
            _ if ch("t") => Shortcut::NewTab,
            _ if ch("w") => Shortcut::CloseTab,
            Key::Named(NamedKey::ArrowUp) => Shortcut::SelectPrev,
            Key::Named(NamedKey::ArrowDown) => Shortcut::SelectNext,
            Key::Named(NamedKey::ArrowLeft) => Shortcut::Collapse,
            Key::Named(NamedKey::ArrowRight) => Shortcut::Expand,
            _ if ch("b") => Shortcut::ToggleSidebar,
            _ if ch("r") => Shortcut::Start,
            _ if ch("x") => Shortcut::Stop,
            // Shift+`,` is `<` on most layouts; accept either.
            _ if ch(",") || ch("<") => Shortcut::EditLayout,
            _ if ch("q") => Shortcut::Quit,
            _ => return None,
        })
    }
}

/// Observe what a PTY's shell is doing (is a command in the foreground, and
/// what). Best-effort and OS-abstracted (see [`sys`]); any failure degrades to
/// "nothing observed" (`Obs::default`), which the planner treats as "not
/// running". `pid == 0` is the headless test harness's sentinel — never a real
/// shell — so it short-circuits to idle.
fn observe(shell_pid: u32) -> Obs {
    if shell_pid == 0 {
        return Obs::default();
    }
    sys::observe(shell_pid)
}

/// The shell's *current* working directory (where `cd` at the prompt left it).
/// OS-abstracted (see [`sys`]), best-effort.
fn proc_cwd(shell_pid: u32) -> Option<PathBuf> {
    if shell_pid == 0 {
        return None;
    }
    sys::proc_cwd(shell_pid)
}

/// Per-OS process introspection. Each variant exposes the same two pure-ish
/// functions; the rest of the program never sees the platform. `observe` is
/// called every frame (per visible session), so it must be cheap — Linux reads
/// `/proc`, macOS uses the `libproc` syscalls directly (no subprocess).
mod sys {
    use super::Obs;
    use std::path::PathBuf;

    #[cfg(target_os = "linux")]
    pub fn observe(shell_pid: u32) -> Obs {
        fn children(pid: u32) -> Vec<u32> {
            std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
                .ok()
                .map(|s| s.split_whitespace().filter_map(|x| x.parse().ok()).collect())
                .unwrap_or_default()
        }
        let mut cur = shell_pid;
        while let Some(&c) = children(cur).first() {
            cur = c;
        }
        if cur == shell_pid {
            return Obs::default();
        }
        let cmd = std::fs::read(format!("/proc/{cur}/cmdline")).ok().map(|b| {
            b.split(|&c| c == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        });
        Obs {
            foreground: true,
            cmd: cmd.filter(|s| !s.is_empty()),
        }
    }

    #[cfg(target_os = "linux")]
    pub fn proc_cwd(shell_pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{shell_pid}/cwd")).ok()
    }

    #[cfg(target_os = "macos")]
    mod libproc {
        use std::os::raw::{c_int, c_void};

        // macOS has no `/proc`; these libproc(3) syscalls are the supported way
        // to walk the process tree and read a process's executable path.
        unsafe extern "C" {
            pub fn proc_listchildpids(ppid: c_int, buffer: *mut c_void, buffersize: c_int) -> c_int;
            pub fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        }

        /// Direct children of `pid`. The buffer is zero-initialized and every
        /// non-zero slot read back, which sidesteps the documented ambiguity in
        /// `proc_listchildpids`'s return value (bytes vs. count).
        pub fn children(pid: u32) -> Vec<u32> {
            let mut buf = vec![0i32; 1024];
            let n = unsafe {
                proc_listchildpids(
                    pid as c_int,
                    buf.as_mut_ptr() as *mut c_void,
                    (buf.len() * std::mem::size_of::<i32>()) as c_int,
                )
            };
            if n <= 0 {
                return Vec::new();
            }
            buf.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect()
        }

        /// Absolute path of `pid`'s executable, if readable.
        pub fn path(pid: u32) -> Option<String> {
            const MAX: usize = 4096; // PROC_PIDPATHINFO_MAXSIZE
            let mut buf = vec![0u8; MAX];
            let n = unsafe {
                proc_pidpath(pid as c_int, buf.as_mut_ptr() as *mut c_void, MAX as u32)
            };
            if n <= 0 {
                return None;
            }
            buf.truncate(n as usize);
            String::from_utf8(buf).ok().filter(|s| !s.is_empty())
        }
    }

    #[cfg(target_os = "macos")]
    pub fn observe(shell_pid: u32) -> Obs {
        // Descend to the deepest foreground descendant of the shell.
        let mut cur = shell_pid;
        while let Some(&c) = libproc::children(cur).first() {
            cur = c;
        }
        if cur == shell_pid {
            return Obs::default();
        }
        Obs {
            foreground: true,
            cmd: libproc::path(cur),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn proc_cwd(shell_pid: u32) -> Option<PathBuf> {
        // No `/proc`; `lsof` reports the cwd file descriptor. This only runs on
        // an explicit "use current working dir" click, so the subprocess cost
        // is irrelevant.
        let out = std::process::Command::new("lsof")
            .args(["-a", "-d", "cwd", "-Fn", "-p", &shell_pid.to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix('n').map(PathBuf::from))
    }

    // Any other OS: introspection isn't wired up, so report "idle" / unknown.
    // The app stays fully usable; only the running-marker and use-cwd dim out.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn observe(_shell_pid: u32) -> Obs {
        Obs::default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn proc_cwd(_shell_pid: u32) -> Option<PathBuf> {
        None
    }
}

// ===========================================================================
// terminal session plumbing (unchanged core, generalized to take a cwd)
// ===========================================================================

/// Userland event from a PTY parser thread. Each event carries the id of the
/// session it originated from so the GUI thread can route it.
#[derive(Debug, Clone)]
enum UserEvent {
    Wakeup,
    Exit(u64),
    Title(u64, String),
    ResetTitle(u64),
    /// Glyph-coverage fonts (emoji + OS CJK/symbol faces) finished loading on a
    /// worker thread — swap them into the renderer so later frames cover more.
    FallbackFonts(Vec<fontdue::Font>),
    /// Bytes the terminal must write *back* down the PTY in reply to a query
    /// from the running program (OSC color reports, device-status reports, …).
    /// `u64` is the owning session id (routed via `id_of`).
    PtyWrite(u64, Vec<u8>),
    /// A program asked to put text on the system clipboard (OSC 52 write).
    /// The clipboard is global, so no session id is needed.
    ClipboardStore(String),
}

/// `EventListener` impl handed to `alacritty_terminal`. Forwards the events we
/// care about onto the winit loop. `Clone + Send` (the PTY runs on its own
/// thread); `id` ties events back to the owning session.
#[derive(Clone)]
enum Listener {
    /// Live session: forward PTY events onto the winit loop.
    Winit {
        proxy: EventLoopProxy<UserEvent>,
        id: u64,
    },
    /// Headless (testkit): a `Term` with no event loop behind it. Events are
    /// dropped — the harness drives the parser synchronously and renders on
    /// demand, so nothing needs waking.
    Null,
}

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let Listener::Winit { proxy, id } = self else {
            return;
        };
        let _ = match event {
            TermEvent::Wakeup => proxy.send_event(UserEvent::Wakeup),
            TermEvent::Exit | TermEvent::ChildExit(_) => {
                proxy.send_event(UserEvent::Exit(*id))
            }
            TermEvent::Title(t) => proxy.send_event(UserEvent::Title(*id, t)),
            TermEvent::ResetTitle => proxy.send_event(UserEvent::ResetTitle(*id)),
            // A program queried a terminal color (e.g. OSC 11 background).
            // Resolve the index against our palette, let the supplied formatter
            // build the escape reply, and route it back to the PTY. Without
            // this the query times out and apps assume a light terminal,
            // emitting dark text that's unreadable over our dark backdrop.
            TermEvent::ColorRequest(index, formatter) => {
                let reply = formatter(crate::ui::osc_color(index));
                proxy.send_event(UserEvent::PtyWrite(*id, reply.into_bytes()))
            }
            // Other PTY replies the emulation layer generates (DSR, DA, …).
            TermEvent::PtyWrite(text) => {
                proxy.send_event(UserEvent::PtyWrite(*id, text.into_bytes()))
            }
            // OSC 52: a program copies to the system clipboard (vim/tmux yank).
            // Only the *write* is honored; OSC 52 reads are intentionally not
            // implemented (they let any program exfiltrate the clipboard).
            TermEvent::ClipboardStore(_, text) => {
                proxy.send_event(UserEvent::ClipboardStore(text))
            }
            _ => Ok(()),
        };
    }
}

/// Grid geometry. `alacritty_terminal` needs this to size the terminal and the
/// PTY (`Dimensions` for `Term`, `WindowSize` for the kernel PTY ioctl).
#[derive(Clone, Copy)]
struct TermSize {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A rasterized glyph: coverage bitmap plus placement metrics.
struct Glyph {
    w: usize,
    h: usize,
    left: i32,
    top: i32, // pixels from cell top to glyph top
    bitmap: Vec<u8>,
}

/// Which embedded face to draw a cell with, derived from its VT attributes.
/// The discriminant doubles as the index into `Renderer::styles`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FontStyle {
    Regular = 0,
    Bold = 1,
    Italic = 2,
    BoldItalic = 3,
}

/// Map a cell's `Flags` to the face it should be drawn with — "draw bold text
/// in bold font" and "allow italic text", in iTerm's terms.
fn font_style(flags: Flags) -> FontStyle {
    match (flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC)) {
        (true, true) => FontStyle::BoldItalic,
        (true, false) => FontStyle::Bold,
        (false, true) => FontStyle::Italic,
        (false, false) => FontStyle::Regular,
    }
}

struct Renderer {
    /// The four embedded JetBrains Mono faces, indexed by `FontStyle as usize`.
    /// `styles[0]` (Regular) defines the cell metrics and the shared baseline.
    styles: [fontdue::Font; 4],
    /// OS fallback faces (regular only), consulted in order for glyphs the
    /// primary family lacks (emoji, CJK, rare symbols).
    fallbacks: Vec<fontdue::Font>,
    cell_w: usize,
    cell_h: usize,
    /// Rasterized-glyph cache keyed by `(char, style, pixel-size-bits)`. Keying
    /// on the size lets the same renderer serve both the logical 1× pass and
    /// the crisp Retina pass (glyphs rasterized at `FONT_PX × scale`).
    cache: HashMap<(char, u8, u32), Glyph>,
    /// When `Some`, `draw_text` records (rather than rasterizes) each chrome
    /// string in *logical* coordinates instead of drawing it. The GUI uses this
    /// to keep text out of the logical frame so it can be replayed crisply at
    /// device resolution after the shape layer is upscaled — the same Retina
    /// fix the terminal grid already gets. `None` = draw immediately (headless).
    text_log: Option<Vec<TextCmd>>,
}

/// A deferred chrome-text draw, captured in logical coordinates.
struct TextCmd {
    x: usize,
    y: usize,
    max_w: usize,
    text: String,
    color: u32,
}

impl Renderer {
    fn new() -> Self {
        let load = |b: &[u8]| {
            fontdue::Font::from_bytes(b.to_vec(), fontdue::FontSettings::default())
                .expect("embedded font failed to parse")
        };
        let styles = [
            load(UBUNTU_MONO_REGULAR),
            load(UBUNTU_MONO_BOLD),
            load(UBUNTU_MONO_ITALIC),
            load(UBUNTU_MONO_BOLD_ITALIC),
        ];
        // The fallback chain (embedded emoji + OS faces for CJK/rare symbols) is
        // *not* loaded here: parsing those fonts — and the `fc-match` subprocesses
        // that locate the OS ones — cost ~100ms+ and would block the window from
        // appearing at all. They're only ever consulted lazily by `glyph()` for a
        // char the primary family lacks, so `load_fallback_fonts()` runs on a
        // worker thread and the result is swapped in via `UserEvent::FallbackFonts`.
        let fallbacks: Vec<fontdue::Font> = Vec::new();

        let lm = styles[0]
            .horizontal_line_metrics(FONT_PX)
            .expect("font line metrics");
        let cell_h = lm.new_line_size.ceil() as usize;
        // Monospace: every cell is the advance width of a representative glyph.
        let cell_w = styles[0].metrics('M', FONT_PX).advance_width.ceil() as usize;

        Self {
            styles,
            fallbacks,
            cell_w: cell_w.max(1),
            cell_h: cell_h.max(1),
            cache: HashMap::new(),
            text_log: None,
        }
    }

    /// Rasterize (or fetch from cache) glyph `c` in `style` at `px` pixels.
    /// Falls back styled-face → regular primary → OS fallbacks for coverage,
    /// but always positions on the primary's baseline so faces stay aligned.
    fn glyph(&mut self, c: char, style: FontStyle, px: f32) -> &Glyph {
        let key = (c, style as u8, px.to_bits());
        let styles = &self.styles;
        let fallbacks = &self.fallbacks;
        self.cache.entry(key).or_insert_with(|| {
            let styled = &styles[style as usize];
            let font = if styled.lookup_glyph_index(c) != 0 {
                styled
            } else if styles[0].lookup_glyph_index(c) != 0 {
                &styles[0]
            } else {
                fallbacks
                    .iter()
                    .find(|f| f.lookup_glyph_index(c) != 0)
                    .unwrap_or(&styles[0])
            };
            // One baseline (the primary's ascent) for every face/fallback.
            let ascent = styles[0]
                .horizontal_line_metrics(px)
                .map(|m| m.ascent)
                .unwrap_or(px * 0.8);
            let (m, mut bitmap) = font.rasterize(c, px);
            // "Thin strokes on Retina Displays": at hi-dpi pixel sizes, lighten
            // partial coverage so stems read thinner and cleaner, the way macOS
            // renders text natively (a no-op at the logical 1× size).
            if px >= FONT_PX * 1.5 {
                for v in bitmap.iter_mut() {
                    *v = ((*v as f32 / 255.0).powf(1.45) * 255.0).round() as u8;
                }
            }
            let top = (ascent - (m.height as f32 + m.ymin as f32)).round() as i32;
            Glyph {
                w: m.width,
                h: m.height,
                left: m.xmin,
                top,
                bitmap,
            }
        })
    }
}

// The primary family is **embedded in the binary** — Ubuntu Mono derivative
// Powerline (the font from the iTerm setup), all four faces. Shipping the fonts
// instead of discovering them means the app renders identically on every OS,
// needs no fontconfig (the previous Linux-only dependency that made it panic on
// a clean macOS), and keeps screenshots reproducible. Bundling
// Bold/Italic/BoldItalic is what lets cells render in the correct face per their
// VT attributes; the "Powerline" patch carries the segment/separator glyphs.
const UBUNTU_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-Regular.ttf");
const UBUNTU_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-Bold.ttf");
const UBUNTU_MONO_ITALIC: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-Italic.ttf");
const UBUNTU_MONO_BOLD_ITALIC: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-BoldItalic.ttf");

/// Embedded monochrome emoji face (Noto Emoji, SIL OFL). fontdue rasterizes
/// outline glyphs only, so colour-bitmap emoji (Apple Color Emoji) can't be
/// drawn — this gives crisp single-colour emoji that work on every platform.
const NOTO_EMOJI: &[u8] = include_bytes!("../assets/fonts/NotoEmoji-Regular.ttf");

/// OS-specific fallback font paths for glyph coverage beyond the primary face.
/// Pure list of candidate paths; non-existent ones are skipped by `load_fonts`.
/// Build the lazy fallback chain: embedded Noto Emoji (monochrome) first so
/// emoji render identically on every OS, then OS faces (`fallback_font_paths`)
/// for anything the primary family lacks (CJK, rare symbols). Runs off the
/// critical path on a worker thread — see `Renderer::new`.
fn load_fallback_fonts() -> Vec<fontdue::Font> {
    let mut fallbacks: Vec<fontdue::Font> = Vec::new();
    if let Ok(f) = fontdue::Font::from_bytes(NOTO_EMOJI.to_vec(), fontdue::FontSettings::default()) {
        fallbacks.push(f);
    }
    fallbacks.extend(
        fallback_font_paths()
            .iter()
            .filter_map(|p| std::fs::read(p).ok())
            .filter_map(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok()),
    );
    fallbacks
}

fn fallback_font_paths() -> Vec<String> {
    #[cfg(target_os = "macos")]
    {
        // Symbol/CJK/emoji faces shipped with every macOS. Apple Color Emoji
        // is a bitmap (sbix) face; fontdue reads its outline layer, which is
        // enough to avoid blank cells for the common pictographs.
        [
            "/System/Library/Fonts/Apple Symbols.ttf",
            "/System/Library/Fonts/Symbol.ttf",
            "/System/Library/Fonts/Apple Color Emoji.ttc",
            "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Elsewhere (Linux/BSD), ask fontconfig for symbol/emoji coverage if
        // it's present; absence is fine, the embedded primary still renders.
        let patterns = [
            "Noto Sans Symbols2",
            "Symbola",
            "Noto Sans Symbols",
            "DejaVu Sans",
            "Noto Color Emoji",
        ];
        let mut paths: Vec<String> = Vec::new();
        for pat in patterns {
            if let Ok(out) = std::process::Command::new("fc-match")
                .args(["-f", "%{file}", pat])
                .output()
            {
                let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !p.is_empty() && !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        paths
    }
}



/// One terminal session: its own PTY thread + VT state machine + grid size.
/// Reused as-is; `spawn_session` now also returns the shell pid for `/proc`.
struct Tab {
    term: Arc<FairMutex<Term<Listener>>>,
    /// PTY input channel. `None` for a headless harness session (no real
    /// PTY); every write goes through `App::send_to`, which then no-ops.
    pty_tx: Option<EventLoopSender>,
    size: TermSize,
    title: String,
}

/// Spawn a fresh PTY-backed terminal session in `workdir`, sized to the
/// current terminal area. Runs no command — it only lands a shell in the right
/// directory. Returns the session and the shell's pid (for `observe`).
fn spawn_session(
    proxy: &EventLoopProxy<UserEvent>,
    id: u64,
    title: String,
    workdir: Option<PathBuf>,
    size: TermSize,
    cell_w: usize,
    cell_h: usize,
) -> (Tab, u32) {
    let window_size = WindowSize {
        num_cols: size.cols as u16,
        num_lines: size.lines as u16,
        cell_width: cell_w as u16,
        cell_height: cell_h as u16,
    };
    let listener = Listener::Winit {
        proxy: proxy.clone(),
        id,
    };
    let term = Term::new(Config::default(), &size, listener.clone());
    let term = Arc::new(FairMutex::new(term));

    // Don't rely on inheriting TERM: launched from the .desktop entry there
    // is no controlling terminal, so TERM is unset and the shell's rc files
    // fall back to a colourless prompt (and terminfo programs go monochrome).
    // Pin a widely-present 256-colour terminfo so colours work either way.
    let env = || {
        HashMap::from([
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
            // Static "light text on dark background" hint for apps that read
            // the env var instead of (or before) issuing an OSC 11 query.
            ("COLORFGBG".to_string(), "15;0".to_string()),
        ])
    };
    let opts = PtyOptions {
        working_directory: workdir.filter(|p| p.is_dir()),
        env: env(),
        ..PtyOptions::default()
    };
    let pty = tty::new(&opts, window_size, 0)
        .or_else(|_| {
            tty::new(
                &PtyOptions {
                    env: env(),
                    ..PtyOptions::default()
                },
                window_size,
                0,
            )
        })
        .expect("spawn pty");
    // Capture the child shell pid before the event loop takes ownership.
    let pid = pty.child().id();

    let pty_loop =
        PtyEventLoop::new(term.clone(), listener, pty, false, false).expect("create pty event loop");
    let pty_tx = pty_loop.channel();
    pty_loop.spawn();
    (
        Tab {
            term,
            pty_tx: Some(pty_tx),
            size,
            title,
        },
        pid,
    )
}

/// The directory the layout file lives in — the base for resolving relative
/// session `dir`s. Falls back to the current directory when the layout path has
/// no usable parent (e.g. a bare filename).
fn layout_dir(ws_path: &Path) -> PathBuf {
    ws_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| home_dir()))
}

/// Resolve a session's working directory for spawning. A relative `dir` (e.g.
/// `../filex`) resolves against the layout file's directory, so it points at the
/// same place regardless of where `terms` was launched from — and stays portable
/// when the layout is committed and opened on another machine. Absolute paths
/// (including `~`-expanded ones) pass through unchanged. The relative form is
/// kept verbatim in the tree (resolution happens only at spawn time).
fn resolve_workdir(base: &Path, dir: &Path) -> PathBuf {
    if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        base.join(dir)
    }
}

/// A live session bound to a leaf node.
struct Session {
    tab: Tab,
    shell_pid: u32,
}

// ===========================================================================
// shell — window state and the winit application
// ===========================================================================

/// A single entry in a right-click context menu, carrying the action it fires.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CtxAction {
    /// Sidebar: start / stop the right-clicked node's session.
    Start,
    Stop,
    /// Terminal: copy the selection / paste the clipboard.
    Copy,
    Paste,
    /// Terminal: open the link under the pointer in the default browser.
    OpenLink,
    /// Terminal: web-search the selection (or word under the pointer).
    SearchGoogle,
}

impl CtxAction {
    fn label(self) -> &'static str {
        match self {
            CtxAction::Start => "Start",
            CtxAction::Stop => "Stop",
            CtxAction::Copy => "Copy",
            CtxAction::Paste => "Paste",
            CtxAction::OpenLink => "Open Link",
            CtxAction::SearchGoogle => "Search Google",
        }
    }
}

/// State of an open right-click menu: where it was opened, on which node, and the
/// items it offers. `target` carries the URL (for `OpenLink`) or search text (for
/// `SearchGoogle`) captured when the menu opened.
struct CtxMenu {
    x: usize,
    y: usize,
    node: NodeId,
    items: Vec<CtxAction>,
    target: Option<String>,
}

/// Everything that exists once the window is created — or, headlessly, once
/// the test harness builds it without a window at all.
struct State {
    /// `None` when driven headlessly by the test harness (no winit window).
    window: Option<Rc<Window>>,
    /// Off-screen framebuffer composed at *logical* size (`logical_size()`),
    /// packed `0x00RRGGBB`. The whole UI is drawn here at 1× and then
    /// nearest-neighbour upscaled onto the physical surface, so the layout is
    /// pixel-identical at every display density (this is the macOS/Retina
    /// fix — previously everything was laid out in raw physical pixels and
    /// came out half-size on a 2× display) and screenshots are
    /// DPI-independent.
    fb: Vec<u32>,
    /// Physical pixel size of the window (or the harness's virtual target).
    phys: (usize, usize),
    /// Device pixel ratio (winit `scale_factor`). 1.0 headless / non-HiDPI.
    scale: f64,
    renderer: Renderer,
    tree: Tree,
    /// Leaf node -> its live PTY session. Absent = idle/never-started.
    sessions: HashMap<NodeId, Session>,
    /// PTY event id -> owning leaf node, for routing parser-thread events.
    id_of: HashMap<u64, NodeId>,
    selected: NodeId,
    /// The single background "edit layout" tab (a volatile leaf running nano on
    /// the config). Pre-warmed at startup and reused by ⌘, / Ctrl+Shift+, so
    /// there is only ever one; recreated after it auto-closes (nano quit / ⌘W).
    config_node: Option<NodeId>,
    next_id: u64,
    ctx: Option<CtxMenu>,
    clipboard: Option<arboard::Clipboard>,
    mouse: (f64, f64),
    selecting: bool,
    last_click: Option<(std::time::Instant, (f64, f64))>,
    mods: ModifiersState,
    /// Right inspector pane shown.
    inspector: bool,
    /// Left sidebar tree pane shown. Toggled with ⌘B / Ctrl+Shift+B; hidden
    /// gives the terminal the full width (the sidebar's width becomes 0).
    sidebar_visible: bool,
    /// Inspector field currently captured by the keyboard (`None` = the PTY
    /// gets keystrokes as usual).
    focus: Option<Field>,
    /// Caret position (char index) within the focused field.
    caret: usize,
    /// Sub-line wheel remainder, so a high-resolution touchpad's many small
    /// deltas accumulate into whole-line scrolls instead of being dropped.
    scroll_acc: f64,
    /// Active scrollbar-thumb drag: the pointer's grab offset *within* the
    /// thumb (logical px), so dragging stays anchored to where it was grabbed.
    /// `None` when not dragging the bar.
    sbar_drag: Option<f64>,
    /// Cursor icon currently set on the window, so `set_cursor` is only called
    /// when the shape actually changes (e.g. entering/leaving a resize grip)
    /// rather than on every mouse-move event.
    cursor: CursorIcon,
    /// Which window-control "traffic light" the mouse is currently over
    /// (`0`=minimize, `1`=maximize, `2`=close), or `None`. Drives the hover
    /// state that reveals the dots' glyphs, like macOS.
    win_hover: Option<usize>,
    /// Whether the pointer is over the title bar (top `HEADER_H` strip), which
    /// gets a slight highlight so it reads as the draggable region.
    header_hover: bool,
    /// The sidebar row the pointer is currently over (if any), drawn with a
    /// faint hover fill. `None` when off the tree or over the selected row.
    sidebar_hover: Option<NodeId>,
    /// Grid cells of the URL under the pointer while Ctrl is held — drawn
    /// underlined so the link reads as clickable. Empty when no link is
    /// hovered (or Ctrl is up). Ctrl+click on any of these opens the URL.
    /// Keyed by `(line, column)` since `Point` itself isn't `Hash`.
    link_cells: HashSet<(i32, usize)>,
}

impl State {
    /// Logical (device-independent) size the UI is laid out and drawn at:
    /// physical pixels divided by the display scale. All chrome geometry and
    /// hit-testing work in these coordinates.
    fn logical_size(&self) -> (usize, usize) {
        let s = self.scale.max(1.0);
        // Round *up*: the logical→physical upscale maps with the constant device
        // `scale` (not the `pw/lw` stretch ratio — that drifts per resize step and
        // makes chrome edges, e.g. the sidebar, wobble), so the logical buffer
        // must be at least `phys/scale` to fully cover the physical surface with
        // no uninitialised strip along the right/bottom edge.
        (
            ((self.phys.0 as f64 / s).ceil() as usize).max(1),
            ((self.phys.1 as f64 / s).ceil() as usize).max(1),
        )
    }

    /// Pull current physical size + scale off the window (no-op headless).
    fn sync_metrics(&mut self) {
        if let Some(w) = &self.window {
            let s = w.inner_size();
            self.phys = (s.width as usize, s.height as usize);
            self.scale = w.scale_factor().max(1.0);
        }
    }

    /// Ask the windowing system to redraw; a no-op when headless (the harness
    /// renders explicitly via `paint`).
    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Compose the whole UI into `self.fb` at logical (device-independent)
    /// size. No window or surface involved — the GUI path upscales `fb` onto
    /// the real surface afterwards; the test harness reads `fb` straight off.
    fn paint(&mut self) {
        let (pw, ph) = self.logical_size();
        // `shown()` borrows all of `State`; resolve it before we take `fb`.
        let shown = self.shown();
        // Reuse the framebuffer's allocation across frames; taking it out
        // detaches it from `self` so the draw code can freely borrow other
        // fields (renderer glyph cache, sessions, tree) at the same time.
        let mut buf = std::mem::take(&mut self.fb);
        buf.clear();
        buf.resize(pw * ph, BG);

        let cw = self.renderer.cell_w;
        let ch = self.renderer.cell_h;
        // Right edge of the terminal: the inspector pane (if open) eats into it.
        let tr = term_right(pw, self.inspector);
        // Right edge of the cell grid: short of the scrollbar gutter.
        let tcr = term_content_right(pw, self.inspector);
        // Left edge of the terminal: the sidebar (auto-sized; 0 when hidden).
        let sw = self.sidebar_w();

        // --- terminal area --------------------------------------------------
        // Lay the backdrop image behind the grid; cells without their own
        // background let it show through (the dark theme). Covers the gutter too,
        // so the scrollbar sits over the backdrop.
        fill_backdrop(
            &mut buf,
            pw,
            ph,
            sw,
            HEADER_H,
            tr.saturating_sub(sw),
            ph.saturating_sub(HEADER_H),
        );
        // Scrollback state of the shown terminal (idle when nothing is shown), so
        // the scrollbar can always be drawn in its gutter.
        let mut scroll_state = (0usize, 0usize, 1usize); // (offset, history, screen)
        if let Some(node) = shown {
            // Render the grid at logical (1×) size into `buf`. The GUI redraw
            // re-renders it crisply at device resolution on Retina; the test
            // harness reads this buffer directly.
            let lines = self.sessions[&node].tab.size.lines as i32;
            let term = self.sessions[&node].tab.term.lock();
            draw_terminal_cells(
                &mut buf,
                pw,
                ph,
                &mut self.renderer,
                &term,
                lines,
                sw,
                HEADER_H,
                tcr,
                cw,
                ch,
                FONT_PX,
                &self.link_cells,
            );
            let grid = term.grid();
            scroll_state = (grid.display_offset(), grid.history_size(), grid.screen_lines());
            drop(term);
        } else {
            draw_text(
                &mut buf,
                pw,
                ph,
                &mut self.renderer,
                sw + 10,
                HEADER_H + 10,
                tr.saturating_sub(sw + 20),
                NO_SESSION_HINT,
                INK_DIM,
            );
        }

        // --- scrollbar (its own gutter, always present) ---------------------
        let (offset, history, screen) = scroll_state;
        draw_scrollbar(
            &mut buf,
            pw,
            ph,
            scrollbar_rect(pw, ph, self.inspector),
            offset,
            history,
            screen,
            SBAR_MIN,
            self.sbar_drag.is_some(),
        );

        // --- header bar over the terminal ----------------------------------
        // (No title text — the path/status label is intentionally hidden.)
        vgradient(&mut buf, pw, ph, sw, 0, pw - sw, HEADER_H, HEAD_HI, HEAD_LO);
        fill_rect(&mut buf, pw, ph, sw, HEADER_H - 1, pw - sw, 1, BEVEL_DK);
        fill_rect(&mut buf, pw, ph, sw, 0, pw - sw, 1, BEVEL_LT);
        let [bmin, bmax, bclose] = win_btns(pw);

        // Window controls: macOS-style "traffic light" dots, drawn as bitmap
        // circles centred (both axes) in their hit cells — no glyphs, no bevels.
        // Amber = minimize, green = maximize/restore, red = close. Hovering any
        // dot lights the whole cluster and reveals each one's glyph (−, +, ×).
        let hovering = self.win_hover.is_some();
        for (rect, color, glyph) in [
            (bmin, TLIGHT_MIN, TlGlyph::Minus),
            (bmax, TLIGHT_MAX, TlGlyph::Plus),
            (bclose, TLIGHT_CLOSE, TlGlyph::Cross),
        ] {
            let (bx, by, bw, bh) = rect;
            let cx = bx as f32 + bw as f32 / 2.0;
            let cy = by as f32 + bh as f32 / 2.0;
            fill_circle(&mut buf, pw, ph, cx, cy, TLIGHT_R, color);
            if hovering {
                draw_tlight_glyph(&mut buf, pw, ph, cx, cy, TLIGHT_R, glyph);
            }
        }

        // --- sidebar tree ---------------------------------------------------
        let sel = self.selected;
        let rows = self.tree.rows(sel);
        if sw > 0 {
            draw_sidebar(
                &mut buf,
                pw,
                ph,
                &mut self.renderer,
                &rows,
                sel,
                self.sidebar_hover,
                sw,
            );
        }

        // --- title-bar hover highlight -------------------------------------
        // A faint lightening across the whole top strip while the pointer is
        // over it, hinting that it's the draggable region. Skips the 1px bevel
        // lines so they stay crisp.
        if self.header_hover && HEADER_H > 2 {
            for y in 1..HEADER_H - 1 {
                let row = y * pw;
                for x in 0..pw {
                    let i = row + x;
                    buf[i] = blend(0xff_ff_ff, buf[i], 20);
                }
            }
        }

        // --- right inspector ------------------------------------------------
        if self.inspector {
            let can_use_cwd = self.tree.is_leaf(sel) && self.sessions.contains_key(&sel);
            draw_inspector(
                &mut buf,
                pw,
                ph,
                &mut self.renderer,
                &self.tree,
                sel,
                self.focus,
                self.caret,
                can_use_cwd,
            );
        }

        // --- context menu ---------------------------------------------------
        if let Some(m) = &self.ctx {
            let hov = ctx_item_at(m, self.mouse.0, self.mouse.1);
            let labels: Vec<&str> = m.items.iter().map(|a| a.label()).collect();
            draw_ctx_menu(&mut buf, pw, ph, &mut self.renderer, m.x, m.y, &labels, hov);
        }

        self.fb = buf;
    }

    /// Re-render just the terminal grid at full device resolution, directly onto
    /// the physical surface `buf`, *after* the logical frame has been upscaled
    /// into it. This is the Retina text fix: the chrome (pixel-art bevels) is
    /// fine nearest-neighbour-upscaled, but text doubled that way is chunky, so
    /// here the glyphs are rasterized at `FONT_PX × scale` and drawn crisply
    /// over the upscaled copy. A no-op at 1× (the logical frame is already
    /// native) and headless (no surface, so this is never called).
    fn overdraw_terminal_physical(&mut self, buf: &mut [u32], pw: usize, ph: usize) {
        let (lw, lh) = self.logical_size();
        if (lw, lh) == (pw, ph) {
            return; // 1×: `paint` already produced native-resolution text
        }
        let Some(node) = self.shown() else { return };
        // Map with the constant device scale, not `pw/lw`: the latter drifts each
        // resize step under fractional DPI and makes the sidebar/header edges
        // breathe by a pixel. `scale` is fixed, so these boundaries are stable.
        let sx = self.scale.max(1.0);
        let sy = sx;
        let origin_x = (self.sidebar_w() as f64 * sx).round() as usize;
        let origin_y = (HEADER_H as f64 * sy).round() as usize;
        // Full viewport (incl. the scrollbar gutter) vs. the cell grid (short of
        // it). Backdrop refills the whole viewport; cells clip to the content.
        let view_right = ((term_right(lw, self.inspector) as f64 * sx).round() as usize).min(pw);
        let clip_right = ((term_content_right(lw, self.inspector) as f64 * sx).round() as usize).min(pw);
        let cell_w = ((self.renderer.cell_w as f64 * sx).round() as usize).max(1);
        let cell_h = ((self.renderer.cell_h as f64 * sy).round() as usize).max(1);
        let font_px = FONT_PX * sy as f32;
        let lines = self.sessions[&node].tab.size.lines as i32;
        // Re-lay the backdrop image in the terminal viewport so the crisp pass
        // leaves no antialiasing ghosts behind and the image stays under the
        // text; per-cell backgrounds are repainted by `draw_terminal_cells`.
        fill_backdrop(
            buf,
            pw,
            ph,
            origin_x,
            origin_y,
            view_right.saturating_sub(origin_x),
            ph.saturating_sub(origin_y),
        );
        let term = self.sessions[&node].tab.term.lock();
        draw_terminal_cells(
            buf,
            pw,
            ph,
            &mut self.renderer,
            &term,
            lines,
            origin_x,
            origin_y,
            clip_right,
            cell_w,
            cell_h,
            font_px,
            &self.link_cells,
        );
        let grid = term.grid();
        let (offset, history, screen) = (grid.display_offset(), grid.history_size(), grid.screen_lines());
        drop(term);
        // Scrollbar, redrawn crisply: scale the logical track rect by the device
        // scale so it lands exactly over the gutter the backdrop just refilled.
        let (rx, ry, rw, rh) = scrollbar_rect(lw, lh, self.inspector);
        let rect = (
            (rx as f64 * sx).round() as usize,
            (ry as f64 * sy).round() as usize,
            ((rw as f64 * sx).round() as usize).max(1),
            (rh as f64 * sy).round() as usize,
        );
        draw_scrollbar(
            buf,
            pw,
            ph,
            rect,
            offset,
            history,
            screen,
            ((SBAR_MIN as f64 * sy).round() as usize).max(1),
            self.sbar_drag.is_some(),
        );
    }

    /// Which leaf's terminal to show: the selection if it is a leaf with a
    /// session, otherwise the first session-bearing leaf under the selection.
    fn shown(&self) -> Option<NodeId> {
        if self.tree.is_leaf(self.selected) && self.sessions.contains_key(&self.selected) {
            return Some(self.selected);
        }
        self.tree
            .leaves(self.selected)
            .into_iter()
            .find(|l| self.sessions.contains_key(l))
    }

    /// `(display_offset, history_size, screen_lines)` of the shown terminal's
    /// grid — the scrollback geometry the scrollbar draws and drags from. `None`
    /// when nothing is shown.
    fn scroll_metrics(&self) -> Option<(usize, usize, usize)> {
        let node = self.shown()?;
        let term = self.sessions[&node].tab.term.lock();
        let g = term.grid();
        Some((g.display_offset(), g.history_size(), g.screen_lines()))
    }

    /// Drag/click the scrollbar: move the viewport so the thumb's top sits at
    /// logical-y `top`. Maps the thumb position back to a display offset and
    /// scrolls there. No-op without scrollback. Mirrors [`scrollbar_thumb`], so
    /// the grab tracks the drawn thumb exactly.
    fn scroll_to_thumb_top(&mut self, top: f64) {
        let Some(node) = self.shown() else { return };
        let (pw, ph) = self.logical_size();
        let (_, area_y, _, area_h) = scrollbar_rect(pw, ph, self.inspector);
        let mut term = self.sessions[&node].tab.term.lock();
        let (offset, history, screen) = {
            let g = term.grid();
            (g.display_offset(), g.history_size(), g.screen_lines())
        };
        if history == 0 {
            return;
        }
        let (_, thumb_h) = scrollbar_thumb(area_y, area_h, offset, history, screen, SBAR_MIN);
        let span = area_h.saturating_sub(thumb_h) as f64;
        if span <= 0.0 {
            return;
        }
        let frac = ((top - area_y as f64) / span).clamp(0.0, 1.0);
        // `frac == 0` is the top (offset == history); `frac == 1` the bottom.
        let target = (history as f64 * (1.0 - frac)).round() as i32;
        let delta = target - offset as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
            drop(term);
            self.request_redraw();
        }
    }

    /// Logical width of the sidebar pane: `0` when hidden, otherwise the left
    /// label inset plus the longest label plus a fixed right margin (never
    /// narrower than `WBTN_W`). All chrome geometry and hit-testing derive from
    /// this, so the pane grows and shrinks to fit its content and vanishes when
    /// toggled off.
    ///
    /// The width fits the longest label across *every* node — all groups and
    /// leaves, whether or not their parent is currently expanded — so the pane
    /// stays a fixed width as groups fold and unfold. Walks reachable nodes
    /// only, skipping orphaned (closed) leaves still parked in the arena.
    fn sidebar_w(&self) -> usize {
        if !self.sidebar_visible {
            return 0;
        }
        fn widest(t: &Tree, id: NodeId, acc: &mut usize) {
            for &c in &t.nodes[id].children {
                // Volatile tabs (the hidden "Edit Config" session) must not
                // affect the width, so it stays put as that tab comes and goes.
                if t.nodes[c].volatile {
                    continue;
                }
                *acc = (*acc).max(t.nodes[c].name.chars().count());
                widest(t, c, acc);
            }
        }
        let mut chars = 0;
        widest(&self.tree, self.tree.root, &mut chars);
        let label_px = chars * self.renderer.cell_w;
        (SIDEBAR_PAD_L + label_px).max(WBTN_W) + SIDEBAR_MARGIN
    }

    /// Grid size for the terminal area (window minus sidebar, header and —
    /// when open — the right inspector pane).
    fn grid_size(&self, win_w: usize, win_h: usize) -> TermSize {
        let avail = term_content_right(win_w, self.inspector).saturating_sub(self.sidebar_w());
        TermSize {
            cols: (avail / self.renderer.cell_w).max(1),
            lines: (win_h.saturating_sub(HEADER_H) / self.renderer.cell_h).max(1),
        }
    }

    /// Convert the mouse position to a grid `Point` within the shown terminal,
    /// accounting for the sidebar/header offset and scrollback.
    fn pixel_to_point(&self, term: &Term<Listener>, size: TermSize) -> (Point, Side) {
        let cw = self.renderer.cell_w as f64;
        let ch = self.renderer.cell_h as f64;
        let mx = (self.mouse.0 - self.sidebar_w() as f64).max(0.0);
        let my = (self.mouse.1 - HEADER_H as f64).max(0.0);
        let col = ((mx / cw) as usize).min(size.cols.saturating_sub(1));
        let row = ((my / ch) as usize).min(size.lines.saturating_sub(1));
        let offset = term.grid().display_offset() as i32;
        let line = Line(row as i32 - offset);
        let side = if (mx / cw).fract() < 0.5 {
            Side::Left
        } else {
            Side::Right
        };
        (Point::new(line, Column(col)), side)
    }

    /// Hit-test a point against the sidebar tree. Returns the row's node id and
    /// whether the row is a group (a group click folds/unfolds it — there is no
    /// separate expander glyph to target). Mirrors the cell-grid layout in
    /// `draw_sidebar`.
    fn sidebar_hit(&self, x: f64, y: f64, rows: &[Row]) -> Option<(NodeId, bool)> {
        let rh = self.renderer.cell_h;
        let sw = self.sidebar_w();
        if sw == 0 || x >= sw as f64 || y < HEADER_H as f64 {
            return None;
        }
        let off = y as usize - HEADER_H;
        let tops = sidebar_row_tops(rows, rh);
        // Last row whose top is at or above the click; reject clicks that land in
        // a blank spacer between group blocks.
        let i = tops.iter().rposition(|&t| off >= t)?;
        if off >= tops[i] + rh {
            return None;
        }
        let row = rows.get(i)?;
        Some((row.id, row.is_group))
    }

    /// The URL under the pointer, if the pointer is inside the shown terminal's
    /// viewport and sits on a link. Returns the URL text and the grid cells it
    /// spans. Does not consider the modifier state — callers gate on Ctrl.
    fn link_under_cursor(&self) -> Option<(String, Vec<Point>)> {
        let node = self.shown()?;
        let (pw, _) = self.logical_size();
        let sw = self.sidebar_w();
        let tr = term_content_right(pw, self.inspector);
        // Outside the terminal viewport (sidebar, header, inspector, gutter): no link.
        if self.mouse.0 < sw as f64
            || (self.mouse.0 as usize) >= tr
            || self.mouse.1 < HEADER_H as f64
        {
            return None;
        }
        let size = self.sessions[&node].tab.size;
        let term = self.sessions[&node].tab.term.lock();
        let (point, _) = self.pixel_to_point(&term, size);
        url_at(&term, point)
    }

    /// Text to offer "Search Google" on: the active selection if there is one,
    /// otherwise the whitespace-delimited word under the pointer. `None` when
    /// neither yields anything (e.g. the pointer sits on blank cells).
    fn search_target(&self) -> Option<String> {
        let node = self.shown()?;
        let size = self.sessions[&node].tab.size;
        let term = self.sessions[&node].tab.term.lock();
        if let Some(sel) = term.selection_to_string() {
            let sel = sel.trim();
            if !sel.is_empty() {
                return Some(sel.to_string());
            }
        }
        let (point, _) = self.pixel_to_point(&term, size);
        word_at(&term, point)
    }

    /// Recompute the hover link highlight from the current pointer position.
    /// Returns true if the set of highlighted cells changed (so the caller can
    /// repaint). The link under the pointer is always underlined — no modifier
    /// needed; Ctrl is only required to *open* it on click.
    fn refresh_link_hover(&mut self) -> bool {
        let cells: HashSet<(i32, usize)> = self
            .link_under_cursor()
            .map(|(_, pts)| pts.iter().map(|p| (p.line.0, p.column.0)).collect())
            .unwrap_or_default();
        if cells != self.link_cells {
            self.link_cells = cells;
            true
        } else {
            false
        }
    }

    /// The cursor shape the pointer's current position calls for: a hand over a
    /// live (Ctrl-held) link, a resize arrow over a window grip, else the
    /// default arrow.
    fn desired_cursor(&self, pw: usize, ph: usize) -> CursorIcon {
        // A link is always underlined on hover, but the hand cursor only appears
        // while Ctrl is held — that's the state in which a click opens it.
        if self.mods.control_key() && !self.link_cells.is_empty() {
            return CursorIcon::Pointer;
        }
        resize_dir(pw, ph, self.mouse.0, self.mouse.1)
            .map(resize_cursor)
            .unwrap_or(CursorIcon::Default)
    }

    /// Push `desired_cursor` to the OS, but only when the shape changes (a
    /// `set_cursor` per mouse-move is wasteful).
    fn apply_cursor(&mut self) {
        let (pw, ph) = self.logical_size();
        let want = self.desired_cursor(pw, ph);
        if want != self.cursor {
            self.cursor = want;
            if let Some(w) = &self.window {
                w.set_cursor(want);
            }
        }
    }
}

/// URL schemes treated as openable links (longest-first so `https://` wins
/// over the `http://` prefix when both could match at the same spot).
const URL_SCHEMES: [&str; 4] = ["https://", "http://", "file://", "ftp://"];

/// Characters that may sit inside a URL: anything non-blank. The scheme anchors
/// the match; the run then extends over every printable cell up to whitespace.
fn is_url_char(c: char) -> bool {
    !c.is_whitespace() && c != '\0'
}

/// Trailing punctuation trimmed off a detected URL — sentence punctuation and
/// closing brackets that almost always belong to the surrounding prose, not the
/// link (e.g. `see https://x.com.` or `(https://x.com)`).
fn is_url_trailer(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '>')
}

/// Assemble the full logical line `point` sits on, following the `WRAPLINE`
/// flag so a line spilling across rows is stitched back together. Returns the
/// visible chars, a parallel cell -> `Point` map, the per-char OSC 8 hyperlink,
/// and the index of `point` within them. Wide-char spacers carry no glyph and
/// are skipped. `None` if the grid is empty or `point` isn't on it.
fn logical_line(
    term: &Term<Listener>,
    point: Point,
) -> Option<(Vec<char>, Vec<Point>, Vec<Option<Hyperlink>>, usize)> {
    let grid = term.grid();
    let cols = grid.columns();
    if cols == 0 {
        return None;
    }
    let top = grid.topmost_line().0;
    let bottom = grid.bottommost_line().0;
    let last = Column(cols - 1);

    // Walk up to the logical line's first row: a row continues the one above
    // when that row ends with WRAPLINE.
    let mut line = point.line.0;
    while line > top && grid[Line(line - 1)][last].flags.contains(Flags::WRAPLINE) {
        line -= 1;
    }

    let mut chars: Vec<char> = Vec::new();
    let mut points: Vec<Point> = Vec::new();
    let mut hrefs: Vec<Option<Hyperlink>> = Vec::new();
    loop {
        let row = Line(line);
        for c in 0..cols {
            let cell = &grid[row][Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            chars.push(cell.c);
            points.push(Point::new(row, Column(c)));
            hrefs.push(cell.hyperlink());
        }
        if line < bottom && grid[row][last].flags.contains(Flags::WRAPLINE) {
            line += 1;
        } else {
            break;
        }
    }

    let hover = points.iter().position(|&p| p == point)?;
    Some((chars, points, hrefs, hover))
}

/// The whitespace-delimited word under `point`, or `None` when the pointer sits
/// on a blank cell. Used for "Search Google" when there's no active selection.
fn word_at(term: &Term<Listener>, point: Point) -> Option<String> {
    let (chars, _, _, hover) = logical_line(term, point)?;
    if chars[hover].is_whitespace() || chars[hover] == '\0' {
        return None;
    }
    let blank = |c: char| c.is_whitespace() || c == '\0';
    let mut start = hover;
    while start > 0 && !blank(chars[start - 1]) {
        start -= 1;
    }
    let mut end = hover + 1;
    while end < chars.len() && !blank(chars[end]) {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    let word = word.trim().to_string();
    (!word.is_empty()).then_some(word)
}

/// Find the link (if any) under `point` in the terminal grid. Returns the URL
/// and every grid `Point` it covers (for hover highlighting). Two kinds are
/// recognized: an explicit OSC 8 hyperlink the cell carries (what Rust's
/// tooling emits, where the visible text differs from the URL), and a bare URL
/// typed into the text. Logical lines wrapped across rows are stitched back
/// together via the `WRAPLINE` flag so a link spilling onto the next row is
/// still detected as one.
fn url_at(term: &Term<Listener>, point: Point) -> Option<(String, Vec<Point>)> {
    let (chars, points, hrefs, hover) = logical_line(term, point)?;

    // An explicit OSC 8 hyperlink wins: highlight the contiguous run of cells
    // sharing it and open its target URI (not the visible text).
    if let Some(href) = &hrefs[hover] {
        let mut start = hover;
        while start > 0 && hrefs[start - 1].as_ref() == Some(href) {
            start -= 1;
        }
        let mut end = hover + 1;
        while end < hrefs.len() && hrefs[end].as_ref() == Some(href) {
            end += 1;
        }
        return Some((href.uri().to_string(), points[start..end].to_vec()));
    }

    // Otherwise look for a bare URL in the text.
    let (start, end) = find_url(&chars, hover)?;
    Some((chars[start..end].iter().collect(), points[start..end].to_vec()))
}

/// Locate the URL covering character index `hover` within `chars`, returning its
/// `[start, end)` range. A URL is a known scheme followed by a run of non-blank
/// characters, with trailing prose punctuation trimmed. Pure (no terminal
/// types) so it can be unit-tested directly.
fn find_url(chars: &[char], hover: usize) -> Option<(usize, usize)> {
    let n = chars.len();
    for scheme in URL_SCHEMES {
        let s: Vec<char> = scheme.chars().collect();
        let mut i = 0;
        while i + s.len() <= n {
            if chars[i..i + s.len()] == s[..] {
                // Extend over the URL body, then trim trailing prose punctuation.
                let mut end = i + s.len();
                while end < n && is_url_char(chars[end]) {
                    end += 1;
                }
                while end > i + s.len() && is_url_trailer(chars[end - 1]) {
                    end -= 1;
                }
                if (i..end).contains(&hover) {
                    return Some((i, end));
                }
                i = end;
            } else {
                i += 1;
            }
        }
    }
    None
}

/// Open a URL in the user's default handler, detached so the terminal never
/// blocks on it. Best-effort: a missing opener is silently ignored.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(not(target_os = "macos"))]
    let mut cmd = std::process::Command::new("xdg-open");
    let _ = cmd.arg(url).spawn();
}

/// Open a Google web search for `query` in the user's default browser.
fn open_search(query: &str) {
    open_url(&format!("https://www.google.com/search?q={}", urlencode(query)));
}

/// Percent-encode `s` for use in a URL query (RFC 3986 unreserved set kept
/// literal, everything else `%XX`). Spaces become `%20`.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

struct App {
    proxy: EventLoopProxy<UserEvent>,
    state: Option<State>,
    /// The window's pixel surface. Lives here (not on `State`) so the
    /// headless harness can own a `State` with no surface at all.
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    /// Where the workspace tree is loaded from (CLI arg, or the default
    /// `~/.termspace/workspace01`). Read-only — the app never writes it back.
    ws_path: PathBuf,
    /// The window is created hidden and revealed once, after the first frame
    /// is presented, so no blank surface flashes on open.
    revealed: bool,
}

impl App {
    /// Run a session-lifecycle plan: spawn/run/sigint each action in order.
    fn apply(&mut self, acts: Vec<Action>) {
        for act in acts {
            match act {
                Action::Spawn(node) => self.spawn_for(node),
                Action::Run(node) => {
                    if let Some((_, cmd)) = self
                        .state
                        .as_ref()
                        .and_then(|s| s.tree.leaf_spec(node).map(|(w, c)| (w, c.to_string())))
                    {
                        self.send_to(node, format!("{cmd}\r").into_bytes());
                    }
                }
                Action::Sigint(node) => self.send_to(node, vec![0x03]),
            }
        }
        if let Some(st) = &self.state {
            st.request_redraw();
        }
    }

    /// Effect: spawn an idle PTY for leaf `node` in its workdir.
    fn spawn_for(&mut self, node: NodeId) {
        let base = layout_dir(&self.ws_path);
        let Some(st) = self.state.as_mut() else { return };
        if st.sessions.contains_key(&node) {
            return;
        }
        let (Some((workdir, _)), name) = (
            st.tree.leaf_spec(node).map(|(w, c)| (w.to_path_buf(), c)),
            st.tree.nodes[node].name.clone(),
        ) else {
            return;
        };
        let (lw, lh) = st.logical_size();
        let size = st.grid_size(lw, lh);
        let id = st.next_id;
        st.next_id += 1;
        let (tab, pid) = spawn_session(
            &self.proxy,
            id,
            name,
            Some(resolve_workdir(&base, &workdir)),
            size,
            st.renderer.cell_w,
            st.renderer.cell_h,
        );
        st.id_of.insert(id, node);
        st.sessions.insert(node, Session { tab, shell_pid: pid });
    }

    fn send_to(&self, node: NodeId, bytes: Vec<u8>) {
        if let Some(tx) = self
            .state
            .as_ref()
            .and_then(|st| st.sessions.get(&node))
            .and_then(|s| s.tab.pty_tx.as_ref())
        {
            let _ = tx.send(Msg::Input(bytes.into()));
        }
    }

    /// Wheel scrolling for the shown session. `delta` is in text lines,
    /// positive when the wheel moves away from the user (scroll back into
    /// history). Fractional input (touchpads) is accumulated so nothing is
    /// lost. On the primary screen this walks the scrollback buffer; on the
    /// alternate screen (full-screen TUIs — `less`, `man`, `vim`, which keep
    /// no scrollback) it instead sends arrow keys, the usual "alternate
    /// scroll".
    fn scroll(&mut self, delta: f64) {
        let Some(st) = self.state.as_mut() else { return };
        let Some(node) = st.shown() else { return };
        st.scroll_acc += delta;
        let lines = st.scroll_acc.trunc() as i32;
        if lines == 0 {
            return;
        }
        st.scroll_acc -= lines as f64;

        let session = &st.sessions[&node];
        let mut term = session.tab.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            drop(term);
            // Up arrow on scroll-back, Down on scroll-forward.
            let seq: &[u8] = if lines > 0 { b"\x1b[A" } else { b"\x1b[B" };
            let mut bytes = Vec::with_capacity(seq.len() * lines.unsigned_abs() as usize);
            for _ in 0..lines.unsigned_abs() {
                bytes.extend_from_slice(seq);
            }
            if let Some(tx) = &session.tab.pty_tx {
                let _ = tx.send(Msg::Input(bytes.into()));
            }
        } else {
            term.scroll_display(Scroll::Delta(lines));
            drop(term);
            st.request_redraw();
        }
    }

    /// Observe every live session's `/proc` state so the pure planners can
    /// decide what's already running.
    fn observations(&self) -> HashMap<NodeId, Obs> {
        self.state
            .as_ref()
            .map(|st| {
                st.sessions
                    .iter()
                    .map(|(&n, s)| (n, observe(s.shell_pid)))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn start(&mut self, target: NodeId) {
        let obs = self.observations();
        let acts = {
            let st = self.state.as_ref().unwrap();
            let has = |n: NodeId| st.sessions.contains_key(&n);
            plan_start(&st.tree, &has, &obs, target)
        };
        self.apply(acts);
    }

    fn stop(&mut self, target: NodeId) {
        let acts = {
            let st = self.state.as_ref().unwrap();
            let has = |n: NodeId| st.sessions.contains_key(&n);
            plan_stop(&st.tree, &has, target)
        };
        self.apply(acts);
    }

    /// Create a scratch session under the group of the current selection (a
    /// selected group is its own target; Scratch/Transient are valid). It
    /// inherits the selected leaf's directory, else `$HOME`.
    fn new_scratch(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let group = st.tree.group_for_new(st.selected);
        let home = home_dir();
        let cwd = st
            .tree
            .leaf_spec(st.selected)
            .map(|(w, _)| w.to_path_buf())
            .unwrap_or(home);
        let n = st.tree.nodes[group].children.len() + 1;
        let node = st.tree.push(
            Some(group),
            format!("{} {n}", st.tree.nodes[group].name),
            Kind::Leaf {
                workdir: cwd,
                command: String::new(),
            },
            true,
        );
        st.tree.nodes[group].expanded = true;
        st.selected = node;
        self.spawn_for(node);
        if let Some(st) = &self.state {
            st.request_redraw();
        }
    }

    /// Ensure the single "Edit Config" session exists with nano open on the
    /// config file, returning its node. A singleton: if it's already live it is
    /// reused (so ⌘, never opens a second, conflicting editor); otherwise it is
    /// (re)created. It is a standalone *top-level* session (a volatile leaf
    /// directly under the root, titled "Edit Config" — its own separated block
    /// in the sidebar, not inside any section, never written back into the
    /// layout). It runs nano via `exec`, so quitting nano ends the PTY and the
    /// dynamic session auto-closes — after which the next call recreates it.
    /// Pre-warmed at startup, but hidden from the sidebar until it is the active
    /// (selected) tab (see `Tree::rows`), so it stays ready without cluttering.
    fn ensure_config_node(&mut self) -> Option<NodeId> {
        // Reuse the existing session while it is still live.
        if let Some(st) = self.state.as_ref() {
            if let Some(id) = st.config_node {
                if st.sessions.contains_key(&id) {
                    return Some(id);
                }
            }
        }
        let path = self.ws_path.clone();
        let dir = layout_dir(&path);
        // `exec` replaces the shell with nano, so quitting nano ends the PTY →
        // the dynamic session auto-closes (see the `UserEvent::Exit` handler).
        let command = format!("exec nano {}", shell_quote(&path));
        let node = {
            let Some(st) = self.state.as_mut() else { return None };
            let root = st.tree.root;
            let id = st.tree.push(
                Some(root),
                "Edit Config".to_string(),
                Kind::Leaf {
                    workdir: dir,
                    command,
                },
                true,
            );
            st.tree.nodes[id].volatile = true;
            st.config_node = Some(id);
            id
        };
        // Spawn the shell and exec nano in it (the planner does both).
        self.start(node);
        Some(node)
    }

    /// Switch to the background layout-editing tab — nano on the config file —
    /// creating it if needed. The terminal equivalent of an editor's ⌘,: there
    /// is no separate settings UI, the YAML file *is* the layout. Edits load on
    /// the next launch (the running app does not live-apply them, and never
    /// writes the file back — it is yours to edit).
    fn edit_layout(&mut self) {
        let Some(node) = self.ensure_config_node() else { return };
        if let Some(st) = self.state.as_mut() {
            if let Some(p) = st.tree.nodes[node].parent {
                st.tree.nodes[p].expanded = true; // reveal it in the sidebar
            }
            st.selected = node;
            st.focus = None;
            st.request_redraw();
        }
    }

    /// Tear down the selected session. Dynamic (scratch) leaves are removed
    /// from the tree; spec leaves stay so they can be re-started.
    fn close_selected(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let node = st.selected;
        if let Some(s) = st.sessions.remove(&node) {
            if let Some(tx) = &s.tab.pty_tx {
                let _ = tx.send(Msg::Shutdown);
            }
            st.id_of.retain(|_, &mut v| v != node);
        }
        let removed = st.tree.nodes[node].dynamic;
        if removed {
            if let Some(p) = st.tree.nodes[node].parent {
                // Land on the preceding session; fall back to the parent only
                // when there is no sibling before this one.
                let target = st.tree.prev_sibling(node).unwrap_or(p);
                st.tree.nodes[p].children.retain(|&c| c != node);
                st.selected = target;
            }
        }
        st.request_redraw();
    }

    /// Move the sidebar selection by `delta` visible rows — the keyboard
    /// equivalent of clicking the row above/below (⌘↑/⌘↓, Ctrl+Shift+↑/↓
    /// elsewhere). It folds over the same `rows()` the sidebar draws, so up/down
    /// always tracks the picture on screen, and wraps around the ends so the
    /// tabs cycle. A selection that isn't currently visible (its group is
    /// collapsed) snaps back to the first row.
    fn select_relative(&mut self, delta: i32) {
        let Some(st) = self.state.as_mut() else { return };
        let rows = st.tree.rows(st.selected);
        if rows.is_empty() {
            return;
        }
        let next = match rows.iter().position(|r| r.id == st.selected) {
            Some(i) => (i as i32 + delta).rem_euclid(rows.len() as i32) as usize,
            None => 0,
        };
        st.selected = rows[next].id;
        st.focus = None;
        st.request_redraw();
    }

    /// Fold (⌘←/Ctrl+Shift+←) or unfold (⌘→/Ctrl+Shift+→) the selected group.
    /// A no-op when a leaf is selected — leaves have nothing to expand — so the
    /// keystroke is simply swallowed rather than leaking to the PTY.
    fn set_group_expanded(&mut self, open: bool) {
        let Some(st) = self.state.as_mut() else { return };
        if !st.tree.is_group(st.selected) {
            return;
        }
        st.tree.nodes[st.selected].expanded = open;
        st.request_redraw();
    }

    /// Reflow every session's grid to the current terminal area. The window
    /// is shared, so a resize *or* an inspector toggle reflows them all (not
    /// just the visible one) to keep background sessions sane.
    fn relayout(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        st.sync_metrics();
        let (lw, lh) = st.logical_size();
        let size = st.grid_size(lw, lh);
        // Skip the reflow when the cell dimensions are unchanged. A live resize
        // fires many events for sub-cell drags (and the WM's increment snapping
        // isn't guaranteed everywhere), but `term.resize` + a `Msg::Resize`
        // SIGWINCH to every shell is only meaningful when the *grid* actually
        // grew or shrank. Reflowing on every pixel both burns CPU and churns
        // terminal content; gate it on a real change. Chrome still repaints via
        // the caller's `redraw`.
        if st
            .sessions
            .values()
            .next()
            .is_some_and(|s| s.tab.size.cols == size.cols && s.tab.size.lines == size.lines)
        {
            st.request_redraw();
            return;
        }
        let ws = WindowSize {
            num_cols: size.cols as u16,
            num_lines: size.lines as u16,
            cell_width: st.renderer.cell_w as u16,
            cell_height: st.renderer.cell_h as u16,
        };
        for s in st.sessions.values_mut() {
            s.tab.size = size;
            s.tab.term.lock().resize(size);
            if let Some(tx) = &s.tab.pty_tx {
                let _ = tx.send(Msg::Resize(ws));
            }
        }
        st.request_redraw();
    }

    /// Toggle the right inspector pane (and reflow the terminal into the
    /// freed/used space). Hiding it drops keyboard focus back to the PTY.
    /// Currently unwired — the info button that drove it was removed; kept so
    /// re-adding the trigger is a one-liner.
    #[allow(dead_code)]
    fn toggle_inspector(&mut self) {
        if let Some(st) = self.state.as_mut() {
            st.inspector = !st.inspector;
            if !st.inspector {
                st.focus = None;
            }
        }
        self.relayout();
    }

    /// Toggle the left sidebar pane (and reflow the terminal into the
    /// freed/used space). Hiding it gives the terminal the full window width.
    fn toggle_sidebar(&mut self) {
        if let Some(st) = self.state.as_mut() {
            st.sidebar_visible = !st.sidebar_visible;
        }
        self.relayout();
    }

    /// Pin the selected leaf's default working directory to its session's
    /// *current* shell cwd (where you've `cd`'d to).
    fn use_current_cwd(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let sel = st.selected;
        let Some(pid) = st.sessions.get(&sel).map(|s| s.shell_pid) else {
            return;
        };
        if let Some(cwd) = proc_cwd(pid) {
            st.tree.set_workdir(sel, cwd);
            st.request_redraw();
        }
    }

    /// Focus an inspector field, placing the caret under the click. Fields
    /// with no text for the selection (Command/Dir on a group) refuse focus.
    fn focus_field(&mut self, f: Field, click_x: f64) {
        let Some(st) = self.state.as_mut() else { return };
        let Some(text) = st.tree.field_text(st.selected, f) else {
            st.focus = None;
            return;
        };
        let fr = field_rects(st.logical_size().0, st.renderer.cell_h)[f.index()];
        let rel = (click_x - (fr.0 + 5) as f64).max(0.0);
        let idx = (rel / st.renderer.cell_w as f64).round() as usize;
        st.focus = Some(f);
        st.caret = idx.min(text.chars().count());
        st.request_redraw();
    }

    /// Apply one keystroke to the focused inspector field, writing the result
    /// straight back into the tree (the tree is the single source of truth).
    /// Returns `true` if the key was consumed (kept off the PTY).
    fn edit_focused(&mut self, key: &Key, text_in: Option<&str>) -> bool {
        let Some(st) = self.state.as_mut() else { return false };
        let Some(f) = st.focus else { return false };
        let sel = st.selected;
        let e = match key {
            Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                st.focus = None;
                st.request_redraw();
                return true;
            }
            Key::Named(NamedKey::Tab) => {
                // Hop to the next field that exists for this node (groups
                // only have Title), wrapping around.
                let order = Field::ALL;
                let cur = f.index();
                let next = (1..=order.len())
                    .map(|k| order[(cur + k) % order.len()])
                    .find(|&nf| st.tree.field_text(sel, nf).is_some())
                    .unwrap_or(Field::Title);
                st.caret = st
                    .tree
                    .field_text(sel, next)
                    .map(|t| t.chars().count())
                    .unwrap_or(0);
                st.focus = Some(next);
                st.request_redraw();
                return true;
            }
            Key::Named(NamedKey::Backspace) => Edit::Back,
            Key::Named(NamedKey::Delete) => Edit::Del,
            Key::Named(NamedKey::ArrowLeft) => Edit::Left,
            Key::Named(NamedKey::ArrowRight) => Edit::Right,
            Key::Named(NamedKey::Home) => Edit::Home,
            Key::Named(NamedKey::End) => Edit::End,
            Key::Named(NamedKey::Space) => Edit::Ins(' '),
            _ => match text_in.and_then(|t| t.chars().next()) {
                Some(ch) if !ch.is_control() => Edit::Ins(ch),
                _ => return true, // swallow other keys while editing
            },
        };
        let cur = st.tree.field_text(sel, f).unwrap_or_default();
        let (next, caret) = apply_edit(&cur, st.caret, e);
        match f {
            Field::Title => st.tree.nodes[sel].name = next,
            Field::Command => {
                if let Some(c) = st.tree.command_mut(sel) {
                    *c = next;
                }
            }
            // Absolute input is left as-is (stable caret); a typed leading
            // `~` is expanded on commit so `~/foo` still resolves.
            Field::Dir => st.tree.set_workdir(sel, expand_tilde(&next, &home_dir())),
        }
        st.caret = caret;
        st.request_redraw();
        true
    }

    /// Compose the frame and blit it onto the physical window surface. On a
    /// Retina display the chrome's *shapes* (pixel-art bevels/gradients) are
    /// nearest-neighbour upscaled — that keeps them crisp — but all *text*
    /// (chrome and terminal) is rendered fresh at true device resolution on
    /// top, so nothing looks doubled or soft. The harness skips this and reads
    /// `State::fb` directly. Non-HiDPI is a straight copy.
    fn redraw(&mut self) {
        let scaled = if let Some(st) = self.state.as_mut() {
            st.sync_metrics();
            let (lw, lh) = st.logical_size();
            let scaled = (lw, lh) != st.phys;
            // Capture chrome text instead of drawing it into the logical frame,
            // so it doesn't get upscaled-and-chunky — we replay it crisply below.
            st.renderer.text_log = if scaled { Some(Vec::new()) } else { None };
            st.paint();
            scaled
        } else {
            return;
        };
        let App { state, surface, revealed, .. } = self;
        let (Some(st), Some(surface)) = (state.as_mut(), surface.as_mut()) else {
            return;
        };
        // Map the window the first time a frame is about to reach the screen —
        // *before* `present()`, not after. An X11 window has no backing store,
        // so presenting while it's still hidden is discarded, and mapping an
        // un-painted window flashes whatever is behind it. Mapping first, then
        // presenting in the same synchronous block, makes the server process
        // Map→PutImage in order so the compositor's first composite already
        // holds our pixels — no flash, no see-through frame.
        let reveal = |st: &State, revealed: &mut bool| {
            if !*revealed {
                if let Some(w) = &st.window {
                    w.set_visible(true);
                }
                *revealed = true;
            }
        };
        let (pw, ph) = st.phys;
        let (Some(w), Some(h)) = (NonZeroU32::new(pw as u32), NonZeroU32::new(ph as u32)) else {
            return;
        };
        let (lw, lh) = st.logical_size();
        surface.resize(w, h).unwrap();
        let mut buf = surface.buffer_mut().unwrap();
        if !scaled {
            // Non-HiDPI: the logical frame already is the physical frame.
            buf.copy_from_slice(&st.fb);
            reveal(st, revealed);
            buf.present().unwrap();
            return;
        }
        // Nearest-neighbour upscale of the shape layer (text was captured, not
        // drawn, so this doesn't blur any glyphs). Sample by the constant device
        // `scale`, not the `pw/lw` stretch ratio: the stretch ratio drifts a
        // fraction of a percent each resize step under fractional DPI, which
        // jitters every chrome edge (the sidebar's right border most visibly).
        // `logical_size` rounds up, so `px/scale < lw` always holds — full
        // coverage, no clamp-induced edge artefact.
        let s = st.scale.max(1.0);
        for py in 0..ph {
            let ly = ((py as f64 / s) as usize).min(lh - 1);
            let (srow, drow) = (ly * lw, py * pw);
            for px in 0..pw {
                let lx = ((px as f64 / s) as usize).min(lw - 1);
                buf[drow + px] = st.fb[srow + lx];
            }
        }
        // Now render text at true device resolution over the upscaled shapes:
        // the terminal grid first, then the captured chrome strings.
        st.overdraw_terminal_physical(&mut buf[..], pw, ph);
        let (sx, sy) = (s, s);
        if let Some(cmds) = st.renderer.text_log.take() {
            render_text_cmds(&mut buf[..], pw, ph, &mut st.renderer, cmds, sx, sy);
        }
        reveal(st, revealed);
        buf.present().unwrap();
    }

    fn send(&self, bytes: Vec<u8>) {
        let Some(st) = self.state.as_ref() else { return };
        let Some(node) = st.shown() else { return };
        // Typing snaps back to the prompt if we were scrolled up, like xterm.
        let mut term = st.sessions[&node].tab.term.lock();
        if term.grid().display_offset() != 0 {
            term.scroll_display(Scroll::Bottom);
            drop(term);
            st.request_redraw();
        } else {
            drop(term);
        }
        self.send_to(node, bytes);
    }

    fn copy_to_clipboard(&mut self) {
        let Some(st) = self.state.as_mut() else { return };
        let Some(node) = st.shown() else { return };
        let text = st.sessions[&node].tab.term.lock().selection_to_string();
        if let (Some(text), Some(cb)) = (text, st.clipboard.as_mut()) {
            if !text.is_empty() {
                let _ = cb.set_text(text);
            }
        }
    }

    fn paste(&mut self) {
        let text = self
            .state
            .as_mut()
            .and_then(|st| st.clipboard.as_mut())
            .and_then(|cb| cb.get_text().ok());
        if let Some(text) = text {
            self.send(text.replace('\n', "\r").into_bytes());
        }
    }
}


fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Single-quote a path for safe interpolation into a shell command line
/// (handles spaces and other metacharacters; embedded `'` is escaped).
fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}


/// Tell X11/the compositor the window is fully opaque and clear its backing to
/// black, before it's mapped. Without this, a freshly-mapped compositor-redirected
/// window has an undefined (often see-through) pixmap for the frame or two before
/// our first `present()` lands — which shows as a flash of the desktop behind it.
/// Best-effort: any failure (Wayland, no X server, refused request) just falls
/// through to the previous behaviour.
#[cfg(target_os = "linux")]
fn mark_window_opaque(window: &Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, Gravity, PropMode,
    };
    // `change_property32` lives on a separate helper trait.
    use x11rb::wrapper::ConnectionExt as _;

    let xid = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Xlib(h)) => h.window as u32,
        Ok(RawWindowHandle::Xcb(h)) => h.window.get(),
        // Wayland (or anything else): no see-through-on-map problem to solve.
        _ => return,
    };
    let Ok((conn, _screen)) = x11rb::connect(None) else {
        return;
    };
    // Two attributes that together make a live resize flicker-free:
    //
    // - `bit_gravity = NorthWest`: the default is `ForgetGravity`, under which
    //   the server *discards the whole window and re-fills it with
    //   `background_pixel` on every ConfigureNotify* — so each drag step flashes
    //   the entire window to `BG` before our `present()` lands (the "mega
    //   flicker"). NorthWest keeps the existing pixels anchored top-left; only
    //   the genuinely-new L-strip along the grown edge is cleared.
    // - `background_pixel = BG`: that new strip clears to the theme background
    //   rather than black, so the dragged edge doesn't flash a dark band before
    //   our next frame overdraws it.
    let _ = conn.change_window_attributes(
        xid,
        &ChangeWindowAttributesAux::new()
            .bit_gravity(Gravity::NORTH_WEST)
            .background_pixel(BG),
    );
    // `_NET_WM_OPAQUE_REGION = whole window` → the compositor won't alpha-blend
    // the surface against the desktop, even before we've drawn into it.
    if let Ok(reply) = conn
        .intern_atom(false, b"_NET_WM_OPAQUE_REGION")
        .map_err(drop)
        .and_then(|c| c.reply().map_err(drop))
    {
        // Oversized on purpose — the compositor clips the region to the actual
        // window, so this stays correct even if the WM resizes us after map
        // (the pre-map `inner_size()` can't be trusted for that).
        let region = [0u32, 0, 1 << 15, 1 << 15];
        let _ = conn.change_property32(
            PropMode::REPLACE,
            xid,
            reply.atom,
            AtomEnum::CARDINAL,
            &region,
        );
    }
    // Round-trip so the server has applied all of the above before winit maps
    // the window on its own (separate) connection.
    let _ = conn.flush();
    let _ = conn.get_input_focus().map(|c| c.reply());
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        // No OS title bar: we draw our own in the header row to reclaim that
        // strip of screen.
        // `mut` is only used by the Linux WM-class block below.
        #[allow(unused_mut)]
        // Start hidden so the compositor never shows the empty/garbage surface
        // before our first frame lands; `redraw` reveals it after the first
        // `present()`.
        let mut attrs = Window::default_attributes()
            .with_title("termset")
            .with_decorations(false)
            .with_visible(false);
        // Pin a stable WM class / Wayland app_id so the desktop entry's
        // `StartupWMClass=termset` binds the launcher icon to this window
        // (see scripts/install-icon.sh).
        #[cfg(target_os = "linux")]
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            attrs = WindowAttributesExtX11::with_name(attrs, "termset", "termset");
            attrs = WindowAttributesExtWayland::with_name(attrs, "termset", "termset");
        }
        let window = Rc::new(
            event_loop.create_window(attrs).expect("create window"),
        );
        // Mark the X11 window opaque + black-backed *before* it maps, so the
        // compositor's first composite of it is a flat dark frame rather than a
        // see-through hole onto the desktop while our pixels are still in flight.
        #[cfg(target_os = "linux")]
        mark_window_opaque(&window);
        let renderer = Renderer::new();
        // Quantize resizing to whole terminal cells: the WM snaps the drag to
        // multiples of one cell, so the grid is always pixel-exact and we get a
        // resize event per *cell* crossed rather than per pixel. The fixed chrome
        // (header/sidebar) is a constant offset, so cell-stepping the window
        // cell-steps the terminal area too.
        window.set_resize_increments(Some(LogicalSize::new(
            renderer.cell_w as f64,
            renderer.cell_h as f64,
        )));
        // Load the glyph-coverage fallback fonts off the critical path; they
        // arrive back as `UserEvent::FallbackFonts` once parsed.
        {
            let proxy = self.proxy.clone();
            std::thread::spawn(move || {
                let _ = proxy.send_event(UserEvent::FallbackFonts(load_fallback_fonts()));
            });
        }
        let ctx = softbuffer::Context::new(window.clone()).unwrap();
        self.surface = Some(softbuffer::Surface::new(&ctx, window.clone()).unwrap());

        let home = home_dir();
        let mut ws_text = std::fs::read_to_string(&self.ws_path).unwrap_or_default();
        if ws_text.trim().is_empty() {
            ws_text = default_workspace_text();
        }
        let tree = parse_workspace(&ws_text, &home);

        let inner = window.inner_size();
        let st = State {
            phys: (inner.width as usize, inner.height as usize),
            scale: window.scale_factor().max(1.0),
            window: Some(window),
            fb: Vec::new(),
            renderer,
            selected: tree
                .first_leaf(tree.root)
                .unwrap_or(tree.root),
            tree,
            sessions: HashMap::new(),
            id_of: HashMap::new(),
            config_node: None,
            next_id: 0,
            ctx: None,
            clipboard: arboard::Clipboard::new().ok(),
            mouse: (0.0, 0.0),
            selecting: false,
            last_click: None,
            mods: ModifiersState::empty(),
            inspector: false,
            sidebar_visible: true,
            focus: None,
            caret: 0,
            scroll_acc: 0.0,
            sbar_drag: None,
            cursor: CursorIcon::Default,
            win_hover: None,
            header_hover: false,
            sidebar_hover: None,
            link_cells: HashSet::new(),
        };
        self.state = Some(st);

        // Paint and reveal the window *before* spawning any shells: the chrome
        // (sidebar tree, header, backdrop) is fully determined by the parsed
        // workspace, so there's nothing to wait for. Forking a PTY per leaf
        // costs ~100ms and produces no visible content anyway — a terminal only
        // shows what its shell writes, which streams in asynchronously — so it
        // has no business being on the path to first paint.
        //
        // Twice on purpose: the first call maps the window (`set_visible`) then
        // presents; the second guarantees a present lands *after* the map even
        // though winit (map) and softbuffer (present) drive separate X11
        // connections whose request order isn't otherwise synchronised. Cheap
        // (one extra chrome paint at startup) insurance against a see-through
        // first frame.
        self.redraw();
        self.redraw();

        let base = layout_dir(&self.ws_path);
        let st = self.state.as_mut().unwrap();
        let (lw, lh) = st.logical_size();
        let size = st.grid_size(lw, lh);

        // On open: an idle PTY per spec leaf — cwd set, no command run.
        let leaves = st.tree.leaves(st.tree.root);
        for node in leaves {
            let (Some((workdir, _)), name) = (
                st.tree.leaf_spec(node).map(|(w, c)| (w.to_path_buf(), c)),
                st.tree.nodes[node].name.clone(),
            ) else {
                continue;
            };
            let id = st.next_id;
            st.next_id += 1;
            let (tab, pid) = spawn_session(
                &self.proxy,
                id,
                name,
                Some(resolve_workdir(&base, &workdir)),
                size,
                st.renderer.cell_w,
                st.renderer.cell_h,
            );
            st.id_of.insert(id, node);
            st.sessions.insert(node, Session { tab, shell_pid: pid });
        }
        st.request_redraw();

        // Pre-warm the "Edit Config" session (nano on the config) in the
        // background. It's hidden from the sidebar until it's the active tab
        // (see `Tree::rows`), so ⌘, / Ctrl+Shift+, reveals an already-open nano.
        self.ensure_config_node();
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wakeup => {
                if let Some(st) = &self.state {
                    st.request_redraw();
                }
            }
            UserEvent::Exit(id) => {
                if let Some(st) = self.state.as_mut() {
                    if let Some(&node) = st.id_of.get(&id) {
                        if let Some(s) = st.sessions.remove(&node) {
                            if let Some(tx) = &s.tab.pty_tx {
                                let _ = tx.send(Msg::Shutdown);
                            }
                        }
                        st.id_of.remove(&id);
                        // A scratch tab disappears when its shell exits; a
                        // spec leaf stays so Start can bring it back. The layout
                        // file is left untouched — the app never writes it.
                        if st.tree.nodes[node].dynamic {
                            if let Some(p) = st.tree.nodes[node].parent {
                                let target = st.tree.prev_sibling(node).unwrap_or(p);
                                st.tree.nodes[p].children.retain(|&c| c != node);
                                if st.selected == node {
                                    st.selected = target;
                                }
                            }
                        }
                        st.request_redraw();
                    }
                }
            }
            UserEvent::Title(id, title) => {
                if let Some(st) = self.state.as_mut() {
                    if let Some(&node) = st.id_of.get(&id) {
                        if let Some(s) = st.sessions.get_mut(&node) {
                            s.tab.title = title;
                        }
                        st.request_redraw();
                    }
                }
            }
            UserEvent::ResetTitle(id) => {
                if let Some(st) = self.state.as_mut() {
                    if let Some(&node) = st.id_of.get(&id) {
                        let name = st.tree.nodes[node].name.clone();
                        if let Some(s) = st.sessions.get_mut(&node) {
                            s.tab.title = name;
                        }
                        st.request_redraw();
                    }
                }
            }
            UserEvent::PtyWrite(id, bytes) => {
                if let Some(node) =
                    self.state.as_ref().and_then(|st| st.id_of.get(&id).copied())
                {
                    self.send_to(node, bytes);
                }
            }
            UserEvent::ClipboardStore(text) => {
                if let Some(cb) = self.state.as_mut().and_then(|st| st.clipboard.as_mut()) {
                    let _ = cb.set_text(text);
                }
            }
            UserEvent::FallbackFonts(fonts) => {
                if let Some(st) = self.state.as_mut() {
                    st.renderer.fallbacks = fonts;
                    // Glyphs rendered before the fallbacks arrived were cached
                    // using the primary face (tofu for chars it lacks); drop the
                    // cache so they re-rasterize with full coverage.
                    st.renderer.cache.clear();
                    st.request_redraw();
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            // Repaint *synchronously* on every size change. During a resize the
            // OS runs its own modal loop, so a queued `request_redraw` isn't
            // serviced until the drag ends — leaving a frozen, stretched frame.
            // Drawing here makes the content track the window edge live instead.
            WindowEvent::Resized(_) => {
                self.relayout();
                self.redraw();
            }
            // Moving between displays of different density (or an OS zoom
            // change) must reflow: `relayout` re-reads size *and* scale off
            // the window, keeping the logical layout constant.
            WindowEvent::ScaleFactorChanged { .. } => self.relayout(),
            WindowEvent::ModifiersChanged(m) => {
                if let Some(st) = self.state.as_mut() {
                    st.mods = m.state();
                    // The underline doesn't depend on Ctrl, but the hand cursor
                    // does: pressing/releasing Ctrl over a link swaps the cursor
                    // without any mouse-move, so refresh it here.
                    st.apply_cursor();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(st) = self.state.as_mut() {
                    // winit reports physical pixels; the UI lives in logical
                    // space, so divide by the scale before hit-testing.
                    let s = st.scale.max(1.0);
                    st.mouse = (position.x / s, position.y / s);
                    let (pw, _ph) = st.logical_size();
                    // Ctrl+hover link highlight: recompute the underlined link
                    // under the new pointer position and repaint if it changed.
                    if st.refresh_link_hover() {
                        st.request_redraw();
                    }
                    // Cursor shape: a hand over a live link, a resize arrow over
                    // the window's edge/corner grips, the default arrow else.
                    // Only pushed to the OS when the shape actually changes.
                    st.apply_cursor();
                    // Traffic-light hover: reveal the dots' glyphs while the
                    // pointer is over any of them. Repaint only on a change.
                    let hover = win_btns(pw)
                        .iter()
                        .position(|&r| hit(r, st.mouse.0, st.mouse.1));
                    if hover != st.win_hover {
                        st.win_hover = hover;
                        st.request_redraw();
                    }
                    // Title-bar hover highlight (the draggable top strip).
                    let over_header = st.mouse.1 >= 0.0
                        && st.mouse.1 < HEADER_H as f64
                        && st.mouse.0 >= 0.0
                        && (st.mouse.0 as usize) < pw;
                    if over_header != st.header_hover {
                        st.header_hover = over_header;
                        st.request_redraw();
                    }
                    // Sidebar row hover: a faint fill follows the pointer down
                    // the tree. Repaint only when the row under it changes.
                    let rows = st.tree.rows(st.selected);
                    let row_hover = st
                        .sidebar_hit(st.mouse.0, st.mouse.1, &rows)
                        .map(|(id, _)| id);
                    if row_hover != st.sidebar_hover {
                        st.sidebar_hover = row_hover;
                        st.request_redraw();
                    }
                    // Repaint while a context menu is open so its hover bar
                    // tracks the pointer.
                    if st.ctx.is_some() {
                        st.request_redraw();
                    }
                    if st.selecting {
                        if let Some(node) = st.shown() {
                            let size = st.sessions[&node].tab.size;
                            let mut term = st.sessions[&node].tab.term.lock();
                            let (point, side) = st.pixel_to_point(&term, size);
                            if let Some(sel) = term.selection.as_mut() {
                                sel.update(point, side);
                            }
                            drop(term);
                            st.request_redraw();
                        }
                    }
                    // Dragging the scrollbar thumb: track the pointer, keeping
                    // the original grab offset so the thumb stays under the cursor.
                    if let Some(grab) = st.sbar_drag {
                        st.scroll_to_thumb_top(st.mouse.1 - grab);
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(stref) = self.state.as_ref() else { return };
                let (mx, my) = stref.mouse;
                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        let (pw, ph) = self.state.as_ref().unwrap().logical_size();

                        // 1. A click anywhere resolves an open context menu.
                        if let Some(m) = &self.state.as_ref().unwrap().ctx {
                            let pick = ctx_item_at(m, mx, my).map(|i| m.items[i]);
                            let node = m.node;
                            let target = m.target.clone();
                            self.state.as_mut().unwrap().ctx = None;
                            match pick {
                                Some(CtxAction::Start) => self.start(node),
                                Some(CtxAction::Stop) => self.stop(node),
                                Some(CtxAction::Copy) => self.copy_to_clipboard(),
                                Some(CtxAction::Paste) => self.paste(),
                                Some(CtxAction::OpenLink) => {
                                    if let Some(url) = target {
                                        open_url(&url);
                                    }
                                }
                                Some(CtxAction::SearchGoogle) => {
                                    if let Some(q) = target {
                                        open_search(&q);
                                    }
                                }
                                None => {}
                            }
                            if let Some(st) = &self.state {
                                st.request_redraw();
                            }
                            return;
                        }
                        // 2. Window controls: minimize / maximize / close.
                        let [bmin, bmax, bclose] = win_btns(pw);
                        if hit(bclose, mx, my) {
                            event_loop.exit();
                            return;
                        }
                        if hit(bmin, mx, my) {
                            if let Some(w) = &self.state.as_ref().unwrap().window {
                                w.set_minimized(true);
                            }
                            return;
                        }
                        if hit(bmax, mx, my) {
                            if let Some(w) = self.state.as_ref().unwrap().window.clone() {
                                w.set_maximized(!w.is_maximized());
                            }
                            return;
                        }
                        // 3. Inspector pane: focus a field, else defocus.
                        if self.state.as_ref().unwrap().inspector
                            && mx >= panel_x(pw) as f64
                        {
                            let ch = self.state.as_ref().unwrap().renderer.cell_h;
                            let rects = field_rects(pw, ch);
                            if let Some(f) =
                                Field::ALL.into_iter().find(|f| hit(rects[f.index()], mx, my))
                            {
                                self.focus_field(f, mx);
                            } else if hit(usecwd_btn(pw, ch), mx, my) {
                                self.use_current_cwd();
                            } else if let Some(st) = self.state.as_mut() {
                                st.focus = None;
                                st.request_redraw();
                            }
                            return;
                        }
                        // 4. Borderless-window resize grips.
                        if let Some(dir) = resize_dir(pw, ph, mx, my) {
                            if let Some(w) = &self.state.as_ref().unwrap().window {
                                let _ = w.drag_resize_window(dir);
                            }
                            return;
                        }
                        // 5. The header row (anywhere else): double-click
                        // toggles maximize/restore (like a native title bar);
                        // a single click drags the window.
                        if my < HEADER_H as f64 {
                            let now = std::time::Instant::now();
                            let st = self.state.as_ref().unwrap();
                            let double = st.last_click.is_some_and(|(t, p)| {
                                now.duration_since(t).as_millis() < 400
                                    && (p.0 - mx).abs() < 4.0
                                    && (p.1 - my).abs() < 4.0
                            });
                            if double {
                                if let Some(w) = st.window.clone() {
                                    w.set_maximized(!w.is_maximized());
                                }
                                self.state.as_mut().unwrap().last_click = None;
                                return;
                            }
                            self.state.as_mut().unwrap().last_click = Some((now, (mx, my)));
                            if let Some(w) = &self.state.as_ref().unwrap().window {
                                let _ = w.drag_window();
                            }
                            return;
                        }
                        // 5b. Scrollbar gutter: grab the thumb to drag, or click
                        // the track to jump the thumb's centre to the cursor.
                        // Either way begins a drag so the motion continues
                        // smoothly. Swallows the click (the gutter is its own
                        // space) even with no scrollback.
                        {
                            let rect = scrollbar_rect(pw, ph, self.state.as_ref().unwrap().inspector);
                            if hit(rect, mx, my) {
                                let (_, area_y, _, area_h) = rect;
                                let grab = self
                                    .state
                                    .as_ref()
                                    .unwrap()
                                    .scroll_metrics()
                                    .map(|(off, hist, scr)| {
                                        let (top, th) = scrollbar_thumb(area_y, area_h, off, hist, scr, SBAR_MIN);
                                        if my >= top as f64 && my < (top + th) as f64 {
                                            my - top as f64 // anchored drag on the thumb
                                        } else {
                                            th as f64 / 2.0 // jump: centre the thumb on the cursor
                                        }
                                    })
                                    .unwrap_or(0.0);
                                let st = self.state.as_mut().unwrap();
                                st.sbar_drag = Some(grab);
                                st.scroll_to_thumb_top(my - grab);
                                return;
                            }
                        }
                        // 6. Sidebar: a click selects the row; a group click
                        // also folds/unfolds it (there is no separate expander).
                        let sel = self.state.as_ref().unwrap().selected;
                        let rows = self.state.as_ref().unwrap().tree.rows(sel);
                        if let Some((node, is_group)) =
                            self.state.as_ref().unwrap().sidebar_hit(mx, my, &rows)
                        {
                            let st = self.state.as_mut().unwrap();
                            st.selected = node;
                            st.focus = None;
                            if is_group {
                                let e = &mut st.tree.nodes[node].expanded;
                                *e = !*e;
                            }
                            st.request_redraw();
                            return;
                        }
                        // 7. Ctrl+click on a hovered link opens it (and does not
                        // start a selection). The link set is kept current by the
                        // Ctrl+hover tracking above.
                        {
                            let st = self.state.as_ref().unwrap();
                            if st.mods.control_key() && !st.link_cells.is_empty() {
                                if let Some((url, _)) = st.link_under_cursor() {
                                    open_url(&url);
                                    return;
                                }
                            }
                        }
                        // 8. Terminal area: start a text selection (content area
                        // only — the scrollbar gutter was handled in step 5b).
                        let tr = term_content_right(pw, self.state.as_ref().unwrap().inspector);
                        let sw = self.state.as_ref().unwrap().sidebar_w();
                        if mx >= sw as f64
                            && (mx as usize) < tr
                            && my >= HEADER_H as f64
                        {
                            if let Some(node) = self.state.as_ref().unwrap().shown() {
                                let now = std::time::Instant::now();
                                let st = self.state.as_mut().unwrap();
                                st.focus = None;
                                let double = st.last_click.is_some_and(|(t, p)| {
                                    now.duration_since(t).as_millis() < 400
                                        && (p.0 - mx).abs() < 4.0
                                        && (p.1 - my).abs() < 4.0
                                });
                                st.last_click = Some((now, (mx, my)));
                                let ty = if double {
                                    SelectionType::Semantic
                                } else {
                                    SelectionType::Simple
                                };
                                let size = st.sessions[&node].tab.size;
                                let mut term = st.sessions[&node].tab.term.lock();
                                let (point, side) = st.pixel_to_point(&term, size);
                                term.selection = Some(Selection::new(ty, point, side));
                                drop(term);
                                st.selecting = true;
                                st.request_redraw();
                            }
                        }
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        if let Some(st) = self.state.as_mut() {
                            st.selecting = false;
                            st.sbar_drag = None;
                        }
                        self.copy_to_clipboard();
                    }
                    (MouseButton::Right, ElementState::Pressed) => {
                        let sel = self.state.as_ref().unwrap().selected;
                        let rows = self.state.as_ref().unwrap().tree.rows(sel);
                        let (pw, _) = self.state.as_ref().unwrap().logical_size();
                        let sw = self.state.as_ref().unwrap().sidebar_w();
                        let inspector = self.state.as_ref().unwrap().inspector;
                        let tr = term_content_right(pw, inspector);
                        if let Some((node, _)) =
                            self.state.as_ref().unwrap().sidebar_hit(mx, my, &rows)
                        {
                            // Sidebar: Start / Stop for the right-clicked node.
                            let st = self.state.as_mut().unwrap();
                            st.selected = node;
                            st.focus = None;
                            st.ctx = Some(CtxMenu {
                                x: (mx as usize).min(sw),
                                y: my as usize,
                                node,
                                items: vec![CtxAction::Start, CtxAction::Stop],
                                target: None,
                            });
                            st.request_redraw();
                        } else if mx >= sw as f64 && (mx as usize) < tr && my >= HEADER_H as f64 {
                            // Terminal area. A link under the pointer offers
                            // "Open Link"; otherwise plain text offers a web
                            // search of the selection (or word under the pointer).
                            let st = self.state.as_mut().unwrap();
                            let node = st.selected;
                            let (items, target) = if let Some((url, _)) = st.link_under_cursor() {
                                (
                                    vec![CtxAction::OpenLink, CtxAction::Copy, CtxAction::Paste],
                                    Some(url),
                                )
                            } else {
                                let q = st.search_target();
                                let mut items = vec![CtxAction::Copy, CtxAction::Paste];
                                if q.is_some() {
                                    items.push(CtxAction::SearchGoogle);
                                }
                                (items, q)
                            };
                            st.ctx = Some(CtxMenu {
                                x: (mx as usize).min(pw.saturating_sub(CTX_W)),
                                y: my as usize,
                                node,
                                items,
                                target,
                            });
                            st.request_redraw();
                        } else if let Some(st) = self.state.as_mut() {
                            st.ctx = None;
                            st.request_redraw();
                        }
                    }
                    (MouseButton::Middle, ElementState::Pressed) => self.paste(),
                    _ => {}
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                // A wheel notch ≈ 3 lines; a high-resolution touchpad reports
                // pixels, converted to lines by the cell height. Positive =
                // away from the user = back into scrollback.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64 * 3.0,
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.state.as_ref().map(|s| s.renderer.cell_h).unwrap_or(1);
                        p.y / ch.max(1) as f64
                    }
                };
                self.scroll(lines);
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                // While an inspector field is focused, the keyboard edits it
                // instead of feeding the PTY.
                if self
                    .state
                    .as_ref()
                    .is_some_and(|s| s.inspector && s.focus.is_some())
                {
                    let txt = event.text.as_ref().map(|s| s.to_string());
                    if self.edit_focused(&event.logical_key, txt.as_deref()) {
                        return;
                    }
                }
                let kmods = self.state.as_ref().map(|s| s.mods).unwrap_or_default();
                if let Some(sc) = match_shortcut(kmods, &event.logical_key) {
                    let sel = self.state.as_ref().map(|s| s.selected);
                    match sc {
                        Shortcut::Copy => self.copy_to_clipboard(),
                        Shortcut::Paste => self.paste(),
                        Shortcut::NewTab => self.new_scratch(),
                        Shortcut::CloseTab => self.close_selected(),
                        Shortcut::SelectPrev => self.select_relative(-1),
                        Shortcut::SelectNext => self.select_relative(1),
                        Shortcut::Collapse => self.set_group_expanded(false),
                        Shortcut::Expand => self.set_group_expanded(true),
                        Shortcut::Start => {
                            if let Some(n) = sel {
                                self.start(n);
                            }
                        }
                        Shortcut::Stop => {
                            if let Some(n) = sel {
                                self.stop(n);
                            }
                        }
                        Shortcut::ToggleSidebar => self.toggle_sidebar(),
                        Shortcut::EditLayout => self.edit_layout(),
                        Shortcut::Quit => event_loop.exit(),
                    }
                    return;
                }
                // On macOS, swallow any other ⌘-combo so it can't leak into the
                // PTY as stray text. (Ctrl-combos fall through to the shell.)
                #[cfg(target_os = "macos")]
                if kmods.super_key() {
                    return;
                }
                let mods = kmods;
                let (ctrl, alt, shift) = (mods.control_key(), mods.alt_key(), mods.shift_key());
                // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl.
                let m = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
                let csi = |tail: &str| -> Vec<u8> {
                    if m > 1 {
                        format!("\x1b[1;{m}{tail}").into_bytes()
                    } else {
                        format!("\x1b[{tail}").into_bytes()
                    }
                };
                let tilde = |n: u8| -> Vec<u8> {
                    if m > 1 {
                        format!("\x1b[{n};{m}~").into_bytes()
                    } else {
                        format!("\x1b[{n}~").into_bytes()
                    }
                };
                let bytes: Vec<u8> = match &event.logical_key {
                    Key::Named(NamedKey::Enter) => vec![b'\r'],
                    Key::Named(NamedKey::Backspace) => {
                        if ctrl { b"\x17".to_vec() } else { vec![0x7f] }
                    }
                    Key::Named(NamedKey::Tab) => {
                        if shift { b"\x1b[Z".to_vec() } else { vec![b'\t'] }
                    }
                    Key::Named(NamedKey::Escape) => vec![0x1b],
                    Key::Named(NamedKey::ArrowUp) => csi("A"),
                    Key::Named(NamedKey::ArrowDown) => csi("B"),
                    Key::Named(NamedKey::ArrowRight) => csi("C"),
                    Key::Named(NamedKey::ArrowLeft) => csi("D"),
                    Key::Named(NamedKey::Home) => csi("H"),
                    Key::Named(NamedKey::End) => csi("F"),
                    Key::Named(NamedKey::Delete) => tilde(3),
                    Key::Named(NamedKey::PageUp) => tilde(5),
                    Key::Named(NamedKey::PageDown) => tilde(6),
                    Key::Named(NamedKey::Space) => {
                        if ctrl { vec![0] } else { vec![b' '] }
                    }
                    Key::Character(c) if ctrl => match c.chars().next() {
                        Some(ch) if ch.is_ascii() => {
                            let b = (ch as u8).to_ascii_uppercase();
                            let ctl = match b {
                                b'@'..=b'_' => b & 0x1f,
                                b' ' => 0,
                                b'?' => 0x7f,
                                _ => return,
                            };
                            if alt { vec![0x1b, ctl] } else { vec![ctl] }
                        }
                        _ => return,
                    },
                    _ => match event.text {
                        Some(ref t) if alt => {
                            let mut v = vec![0x1b];
                            v.extend_from_slice(t.as_bytes());
                            v
                        }
                        Some(ref t) => t.as_bytes().to_vec(),
                        None => return,
                    },
                };
                self.send(bytes);
            }
            _ => {}
        }
    }
}

/// Real entry point: build the winit event loop and run the GUI app. The
/// binary (`src/main.rs`) is a thin shim over this so the rest of the crate
/// stays a library the integration harness (`testkit`) can drive headlessly.
pub fn run() {
    // Warm the backdrop decode off-thread so its one-time ~15ms cost overlaps
    // event-loop + font setup instead of landing on the first paint. `backdrop()`
    // caches into a `OnceLock`, so the first `paint()` just reads the result.
    std::thread::spawn(|| {
        let _ = backdrop();
    });
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let ws_path = resolve_workspace_path();
    let mut app = App {
        proxy,
        state: None,
        surface: None,
        ws_path,
        revealed: false,
    };
    event_loop.run_app(&mut app).expect("run");
}

// ===========================================================================
// tests — the functional core, exercised without a display
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn url_in(line: &str, hover_on: &str) -> Option<String> {
        let chars: Vec<char> = line.chars().collect();
        let hover = line.find(hover_on).unwrap();
        find_url(&chars, hover).map(|(a, b)| chars[a..b].iter().collect())
    }

    #[test]
    fn detects_urls_under_the_pointer() {
        // Plain link, hovering anywhere inside it.
        assert_eq!(
            url_in("see https://example.com/x for more", "example"),
            Some("https://example.com/x".to_string())
        );
        // Trailing sentence punctuation and wrapping brackets are trimmed.
        assert_eq!(
            url_in("(visit http://a.b/c).", "a.b"),
            Some("http://a.b/c".to_string())
        );
        // file:// scheme is recognized too.
        assert_eq!(
            url_in("open file:///etc/hosts now", "etc"),
            Some("file:///etc/hosts".to_string())
        );
        // Hovering off the link (on surrounding prose) finds nothing.
        assert_eq!(url_in("see https://example.com here", "here"), None);
        // No scheme, no link.
        assert_eq!(url_in("just example.com text", "example"), None);
    }

    #[test]
    fn urlencode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(urlencode("hello world"), "hello%20world");
        assert_eq!(urlencode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(urlencode("cargo build --release"), "cargo%20build%20--release");
        assert_eq!(urlencode("a+b&c=d"), "a%2Bb%26c%3Dd");
    }

    // A fixed fixture in the YAML layout format. Tests must not depend on a
    // real on-disk layout file (a user could edit it at any time), so coupling
    // tests to it makes them flaky.
    const FIXTURE: &str = "\
groups:
  - name: Apps
    sessions:
      - name: qwen
        dir: /home/liam/.apps
        command: ./run-llama.sh
  - name: Music
    sessions:
      - name: Hermes chat
        dir: ~/Music
        command: hermes
      - name: Wanted music
        dir: ~/Music
        command: nano wanted
  - name: Diabetes
    sessions:
      - name: Meal planner
        dir: ~/Documents/projects/diabetes/mealplanner
        command: bun run main.ts
      - name: Sugar tracker
        dir: /home/liam/Documents/projects/diabetes/sugar
        command: ./run.sh
  - name: Morphology
    sessions:
      - name: morpheus
        dir: ~/Documents/projects/morpheus
        command: bash
  - name: HTGAA
    sessions:
      - name: website
        dir: /home/liam/Documents/projects/webpages
        command: bash
  - name: Study
    sessions:
      - name: Papers
        dir: ~/Documents/papers
        command: bash
      - name: Podcasts
        dir: /home/liam/Dropbox/podcast-learn
        command: bash
";

    fn tree() -> Tree {
        parse_workspace(FIXTURE, Path::new("/home/u"))
    }

    fn group(t: &Tree, name: &str) -> NodeId {
        t.nodes
            .iter()
            .position(|n| n.name == name && matches!(n.kind, Kind::Group))
            .unwrap()
    }

    #[test]
    fn parses_groups_and_leaves() {
        let t = tree();
        // Root child groups, in file order. Nothing is synthesized any more.
        let groups: Vec<&str> = t.nodes[t.root]
            .children
            .iter()
            .map(|&c| t.nodes[c].name.as_str())
            .collect();
        assert_eq!(
            groups,
            ["Apps", "Music", "Diabetes", "Morphology", "HTGAA", "Study"]
        );
    }

    #[test]
    fn resolve_workdir_handles_relative_and_absolute() {
        let base = Path::new("/home/u/projects/app");
        // A relative dir (the `../filex` shortcut) resolves against the layout
        // file's directory, not the process cwd. `..`/`.` are left for the OS to
        // collapse when it sets the spawned shell's cwd (join is lexical).
        assert_eq!(
            resolve_workdir(base, Path::new("../filex")),
            Path::new("/home/u/projects/app/../filex"),
        );
        assert_eq!(
            resolve_workdir(base, Path::new("sub")),
            Path::new("/home/u/projects/app/sub"),
        );
        // Absolute and ~-expanded dirs are left untouched.
        assert_eq!(
            resolve_workdir(base, Path::new("/etc")),
            Path::new("/etc"),
        );
    }

    #[test]
    fn relative_dir_parses_as_is() {
        // A relative `dir` parses as-is — the portable `../filex` form is never
        // rewritten to an absolute path.
        let home = Path::new("/home/u");
        let t = parse_workspace(
            "groups:\n  - name: P\n    sessions:\n      - name: s\n        dir: ../filex\n",
            home,
        );
        let leaf = t.leaves(t.root)[0];
        assert_eq!(t.leaf_spec(leaf).unwrap().0, Path::new("../filex"));
    }

    #[test]
    fn leaf_spec_expands_tilde_and_keeps_command() {
        let t = tree();
        let music = group(&t, "Music");
        let leaves = t.leaves(music);
        assert_eq!(leaves.len(), 2);
        let (wd, cmd) = t.leaf_spec(leaves[0]).unwrap();
        assert_eq!(t.nodes[leaves[0]].name, "Hermes chat");
        assert_eq!(wd, Path::new("/home/u/Music")); // ~ expanded
        assert_eq!(cmd, "hermes");
        // Quoted multi-word command survives intact.
        assert_eq!(t.leaf_spec(leaves[1]).unwrap().1, "nano wanted");
    }

    #[test]
    fn group_for_new_resolves_context() {
        let t = tree();
        let music = group(&t, "Music");
        let leaf = t.leaves(music)[0];
        assert_eq!(t.group_for_new(leaf), music); // leaf -> its group
        assert_eq!(t.group_for_new(music), music); // group -> itself
    }

    #[test]
    fn closing_selects_preceding_sibling_then_parent() {
        let t = tree();
        let music = group(&t, "Music");
        let [a, b] = t.leaves(music)[..] else { panic!() };
        // Closing the 2nd session lands on the 1st (the preceding one)...
        assert_eq!(t.prev_sibling(b), Some(a));
        // ...and closing the 1st has no preceding sibling, so the caller
        // falls back to the parent group.
        assert_eq!(t.prev_sibling(a), None);
        assert_eq!(t.prev_sibling(a).unwrap_or(music), music);
    }

    #[test]
    fn already_running_predicate() {
        let idle = Obs::default();
        let busy = Obs {
            foreground: true,
            cmd: Some("/usr/bin/hermes --serve".into()),
        };
        assert!(!already_running(&idle, "hermes")); // no foreground job
        assert!(!already_running(&busy, "")); // a bare shell is never "running"
        assert!(already_running(&busy, "hermes")); // program matches
        assert!(already_running(&Obs { foreground: true, cmd: Some("bun run main.ts".into()) }, "bun run main.ts"));
        assert!(!already_running(&busy, "./run.sh")); // different program
    }

    #[test]
    fn plan_start_is_idempotent() {
        let t = tree();
        let music = group(&t, "Music");
        let [a, b] = t.leaves(music)[..] else { panic!() };

        // Nothing spawned yet: each leaf gets Spawn then Run.
        let none = |_: NodeId| false;
        let acts = plan_start(&t, &none, &HashMap::new(), music);
        assert_eq!(acts.len(), 4);
        assert!(matches!(acts[0], Action::Spawn(x) if x == a));
        assert!(matches!(acts[1], Action::Run(x) if x == a));

        // Sessions exist; `a` is already running its command -> skip it
        // entirely, only `b` is (re)run.
        let all = |_: NodeId| true;
        let mut obs = HashMap::new();
        obs.insert(a, Obs { foreground: true, cmd: Some("hermes".into()) });
        let acts = plan_start(&t, &all, &obs, music);
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], Action::Run(x) if x == b));
    }

    #[test]
    fn plan_stop_targets_only_live_leaves() {
        let t = tree();
        let music = group(&t, "Music");
        let [a, _b] = t.leaves(music)[..] else { panic!() };
        let only_a = move |n: NodeId| n == a;
        let acts = plan_stop(&t, &only_a, music);
        assert_eq!(acts.len(), 1);
        assert!(matches!(acts[0], Action::Sigint(x) if x == a));
    }

    #[test]
    fn text_field_editing_is_pure_and_utf8_safe() {
        // Insert at caret, advancing it.
        let (s, c) = apply_edit("ac", 1, Edit::Ins('b'));
        assert_eq!((s.as_str(), c), ("abc", 2));
        // Backspace removes the char before the caret.
        let (s, c) = apply_edit("abc", 2, Edit::Back);
        assert_eq!((s.as_str(), c), ("ac", 1));
        // Delete removes the char at the caret; Backspace at 0 is a no-op.
        assert_eq!(apply_edit("abc", 0, Edit::Del).0, "bc");
        assert_eq!(apply_edit("abc", 0, Edit::Back), ("abc".to_string(), 0));
        // Home/End/Left/Right move only the caret.
        assert_eq!(apply_edit("abc", 1, Edit::End).1, 3);
        assert_eq!(apply_edit("abc", 3, Edit::Right).1, 3); // clamped
        // Caret is a *char* index, so multibyte content stays valid.
        let (s, c) = apply_edit("é€", 2, Edit::Ins('x'));
        assert_eq!((s.as_str(), c), ("é€x", 3));
        let (s, _) = apply_edit("é€x", 1, Edit::Back);
        assert_eq!(s, "€x");
    }

    #[test]
    fn field_text_reflects_kind() {
        let t = tree();
        let music = group(&t, "Music");
        let leaf = t.leaves(music)[0];
        assert_eq!(t.field_text(leaf, Field::Title).as_deref(), Some("Hermes chat"));
        assert_eq!(t.field_text(leaf, Field::Command).as_deref(), Some("hermes"));
        assert_eq!(
            t.field_text(leaf, Field::Dir).as_deref(),
            Some("/home/u/Music")
        );
        // A group has a title but no command/dir (those render disabled).
        assert_eq!(t.field_text(music, Field::Title).as_deref(), Some("Music"));
        assert_eq!(t.field_text(music, Field::Command), None);
        assert_eq!(t.field_text(music, Field::Dir), None);

        // Editing the directory field writes straight back into the tree (a
        // typed leading `~` expands on commit).
        let mut t = t;
        t.set_workdir(leaf, expand_tilde("~/elsewhere", Path::new("/home/u")));
        assert_eq!(
            t.field_text(leaf, Field::Dir).as_deref(),
            Some("/home/u/elsewhere")
        );
    }

    #[test]
    fn new_workspace_has_project_session_in_cwd() {
        // A brand-new (missing/empty) workspace opens with a single `Project`
        // group holding one session whose workdir is the current directory.
        let t = parse_workspace(&default_workspace_text(), Path::new("/home/u"));
        let groups: Vec<&str> = t.nodes[t.root]
            .children
            .iter()
            .map(|&c| t.nodes[c].name.as_str())
            .collect();
        assert_eq!(groups, ["Project"]);

        let project = group(&t, "Project");
        let leaves = t.leaves(project);
        assert_eq!(leaves.len(), 1, "one starter session");
        let (wd, cmd) = t.leaf_spec(leaves[0]).unwrap();
        assert_eq!(wd, std::env::current_dir().unwrap());
        assert_eq!(cmd, "", "just a shell, no default command");
    }

    #[test]
    fn embedded_font_loads_and_rasterizes() {
        // The primary face is baked into the binary, so this must succeed on a
        // bare machine with no fontconfig and no system fonts — the macOS fix.
        let mut r = Renderer::new();
        assert!(r.cell_w >= 1 && r.cell_h >= 1, "cell metrics must be positive");
        let g = r.glyph('M', FontStyle::Regular, FONT_PX);
        assert!(g.w > 0 && g.h > 0, "a basic glyph must rasterize");
        // Bold and italic faces are distinct from regular (real attribute
        // rendering, not faux-styling): a wide glyph rasterizes in each.
        for s in [FontStyle::Bold, FontStyle::Italic, FontStyle::BoldItalic] {
            assert!(r.glyph('W', s, FONT_PX).w > 0, "styled face must rasterize");
        }
    }

    #[test]
    fn shortcuts_map_per_platform() {
        let c = Key::Character("c".into());
        let dot = Key::Character(".".into());
        let s = Key::Character("s".into());
        let q = Key::Character("q".into());
        let comma = Key::Character(",".into());
        let lt = Key::Character("<".into()); // Shift+`,` on most layouts
        let cmd = ModifiersState::SUPER;
        let ctrl_shift = ModifiersState::CONTROL | ModifiersState::SHIFT;

        // A bare keystroke is never a shortcut on any platform (it goes to the
        // PTY).
        assert_eq!(match_shortcut(ModifiersState::empty(), &c), None);

        if cfg!(target_os = "macos") {
            // ⌘ is the mac modifier; Ctrl+Shift is not.
            assert_eq!(match_shortcut(cmd, &c), Some(Shortcut::Copy));
            assert_eq!(match_shortcut(cmd, &dot), Some(Shortcut::Stop));
            // ⌘S is unbound — the layout is never saved by the app.
            assert_eq!(match_shortcut(cmd, &s), None);
            assert_eq!(match_shortcut(cmd, &comma), Some(Shortcut::EditLayout));
            assert_eq!(match_shortcut(cmd, &q), Some(Shortcut::Quit));
            assert_eq!(match_shortcut(ctrl_shift, &c), None);
        } else {
            // Ctrl+Shift is the modifier elsewhere; ⌘ doesn't exist.
            assert_eq!(match_shortcut(ctrl_shift, &c), Some(Shortcut::Copy));
            // Ctrl+Shift+S is unbound — the layout is never saved by the app.
            assert_eq!(match_shortcut(ctrl_shift, &s), None);
            assert_eq!(match_shortcut(ctrl_shift, &q), Some(Shortcut::Quit));
            // Both `,` and its shifted `<` edit the layout.
            assert_eq!(match_shortcut(ctrl_shift, &comma), Some(Shortcut::EditLayout));
            assert_eq!(match_shortcut(ctrl_shift, &lt), Some(Shortcut::EditLayout));
            assert_eq!(match_shortcut(cmd, &c), None);
        }
    }
}
