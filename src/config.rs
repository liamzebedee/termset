//! Workspace layout configuration: the on-disk YAML schema, parsing it into the
//! in-memory [`crate::Tree`], tilde expansion, and resolution of which layout
//! file / default template to open. The layout file is read-only to the app —
//! it is never written back; the user edits it by hand.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Kind, Tree, home_dir};

/// Expand `~` / `~/...` to `$HOME`. Pure given `home`.
pub(crate) fn expand_tilde(s: &str, home: &Path) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// The on-disk layout/config schema (YAML). A flat list of named sections, each
/// holding named sessions. This is the *typed* shape `serde` deserializes; the
/// in-memory [`Tree`] is built from it. Kept deliberately simple — two levels
/// (section → session), which is all the UI exposes.
#[derive(Debug, Default, Deserialize)]
struct LayoutCfg {
    /// Top-level sections shown in the sidebar, in order.
    #[serde(default)]
    groups: Vec<GroupCfg>,
}

/// One sidebar section. May be empty (it still shows as a header).
#[derive(Debug, Deserialize)]
struct GroupCfg {
    name: String,
    /// Sessions in this section.
    #[serde(default)]
    sessions: Vec<SessionCfg>,
}

/// One session (a leaf): a working directory and an optional default command.
#[derive(Debug, Deserialize)]
struct SessionCfg {
    name: String,
    /// Working directory (`~` allowed). Empty/absent → `$HOME`.
    #[serde(default)]
    dir: String,
    /// Default command run by Start. Empty → a bare shell.
    #[serde(default)]
    command: String,
}

/// Parse the YAML layout file into a tree. Malformed YAML degrades to an empty
/// layout rather than panicking. Empty sections are kept (they render as bare
/// headers).
pub(crate) fn parse_workspace(text: &str, home: &Path) -> Tree {
    let cfg: LayoutCfg = serde_yaml::from_str(text).unwrap_or_default();

    let mut tree = Tree {
        nodes: Vec::new(),
        root: 0,
    };
    let root = tree.push(None, "workspaces".into(), Kind::Root, false);
    tree.root = root;

    for g in cfg.groups {
        let gid = tree.push(Some(root), g.name, Kind::Group, false);
        for s in g.sessions {
            let workdir = if s.dir.trim().is_empty() {
                home.to_path_buf()
            } else {
                expand_tilde(s.dir.trim(), home)
            };
            tree.push(
                Some(gid),
                s.name,
                Kind::Leaf {
                    workdir,
                    command: s.command,
                },
                false,
            );
        }
    }
    tree
}

/// Which layout file to use, per the `terms` CLI:
///
/// * `terms <file>` → that file (a leading `~` is expanded; relative paths resolve
///   against the current directory).
/// * `terms` (no argument) → the per-directory layout file `./termset.yml` in the
///   current directory — so each project gets its own layout.
///
/// Either way, a missing file just opens the default layout (a `Project` group
/// with one session in the current directory; see [`default_workspace_text`]).
/// The app never writes the file — create or edit it yourself.
pub(crate) fn resolve_workspace_path() -> PathBuf {
    match std::env::args().nth(1) {
        Some(a) if !a.trim().is_empty() => expand_tilde(a.trim(), &home_dir()),
        _ => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("termset.yml"),
    }
}

/// The on-disk template a brand-new (missing/empty) workspace opens with,
/// compiled into the binary at build time. Edit `assets/default-termset.yml`
/// to change the starting layout; `{{name}}`/`{{dir}}` are substituted by
/// [`default_workspace_text`]. Keeping it as an editable file (rather than a
/// `LayoutCfg` literal) means the default can be tweaked without touching code.
const DEFAULT_LAYOUT_TEMPLATE: &str = include_str!("../assets/default-termset.yml");

/// The tree a brand-new (missing/empty) workspace opens with: the bundled
/// [`DEFAULT_LAYOUT_TEMPLATE`] with `{{name}}`/`{{dir}}` filled in for the
/// current working directory. Nothing is written to disk — it only ever lives
/// in memory.
pub(crate) fn default_workspace_text() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session");
    // YAML-escape the substituted scalars so directories/names containing
    // spaces, colons, etc. still produce valid YAML (serde quotes when needed).
    let yaml_scalar = |s: &str| {
        serde_yaml::to_string(&s)
            .unwrap_or_default()
            .trim_end()
            .to_string()
    };
    DEFAULT_LAYOUT_TEMPLATE
        .replace("{{name}}", &yaml_scalar(name))
        .replace("{{dir}}", &yaml_scalar(&cwd.display().to_string()))
}

