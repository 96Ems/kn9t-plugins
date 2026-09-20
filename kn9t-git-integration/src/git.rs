//! Runs `git` and parses its output into a small, TUI-friendly shape.
//!
//! Kept free of any plugin-protocol concerns (no `ToolCallCtx`, no JSON
//! serialization here) so the parsing can be unit tested against captured
//! `git` output without spawning a process or a plugin runtime.

use std::path::Path;
use std::process::Command;

/// One line from `git status --porcelain=v2`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// "M" (modified), "A" (added), "D" (deleted), "R" (renamed), "?" (untracked), ...
    pub status: String,
    pub path: String,
}

/// One line from `git log --oneline`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub sha: String,
    pub subject: String,
    pub author: String,
    pub date: String,
    pub refs: Vec<String>,
    pub graph: String,
}

/// A git reference (branch, tag, or remote).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    pub name: String,
    pub kind: RefKind,
    pub is_current: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    LocalBranch,
    RemoteBranch,
    Tag,
}

impl RefKind {
    pub fn tag(&self) -> &'static str {
        match self {
            RefKind::LocalBranch => "local",
            RefKind::RemoteBranch => "remote",
            RefKind::Tag => "tag",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitState {
    /// `None` when `cwd` is not inside a git repository at all — distinct
    /// from `Some(GitState::default())`, which is a clean repo with no
    /// history yet. The tool/UI need to tell these apart to avoid claiming
    /// "clean" for a directory that was never a repo.
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<FileChange>,
    pub recent: Vec<LogEntry>,
    pub refs: Vec<GitRef>,
    pub stashes: Vec<String>,
}

/// How many `git log` entries to fetch. We load all upfront and paginate
/// in the UI (100 commits per page) for smooth scrolling.
pub const LOG_LIMIT: usize = 1000;

/// Collect git state for the repository containing `cwd`, or `None` if
/// `cwd` is not inside a git repository (or `git` is not on PATH).
pub fn collect(cwd: &Path) -> Option<GitState> {
    let branch_out = run_git(cwd, &["status", "--branch", "--porcelain=v2"])?;
    let (branch, ahead, behind, changes) = parse_status(&branch_out);
    // `run_git` already returns `None` on a non-zero exit (the real case for
    // "not a repo" — `git status` there exits 128). This second check is a
    // narrower defense: `git` exiting 0 with output that has no
    // `# branch.head` line, which should not happen with a real `git` but
    // costs nothing to guard against.
    branch.as_ref()?;

    let log_out = run_git(
        cwd,
        &[
            "log",
            "--all",
            &format!("-{LOG_LIMIT}"),
            "--graph",
            "--pretty=format:%h\x1f%s\x1f%an\x1f%cr\x1f%D",
        ],
    )
    .unwrap_or_default();
    let recent = parse_log(&log_out);

    let refs = collect_refs(cwd, branch.as_deref());
    let stashes = collect_stashes(cwd);

    Some(GitState {
        branch,
        ahead,
        behind,
        changes,
        recent,
        refs,
        stashes,
    })
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Like [`run_git`] but tolerant of a non-zero exit.
///
/// `git diff` exits non-zero in normal, non-error situations (notably with
/// `--exit-code` semantics in some configs), and its stdout is still the diff.
/// Treating that as failure would silently show an empty review panel.
///
/// Uses `from_utf8_lossy`: a diff of a file with mixed encodings must degrade
/// to replacement characters rather than discarding the whole hunk.
pub fn run_git_raw(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse `git status --branch --porcelain=v2` output.
///
/// Format reference (porcelain v2, not the human-readable default):
///   `# branch.head <name>`
///   `# branch.ab +<ahead> -<behind>`
///   `1 <xy> ... <path>`        (ordinary changed entry)
///   `2 <xy> ... <path>\t<orig>` (renamed/copied entry — path is after the tab)
///   `? <path>`                  (untracked)
fn parse_status(out: &str) -> (Option<String>, u32, u32, Vec<FileChange>) {
    let mut branch = None;
    let mut ahead = 0;
    let mut behind = 0;
    let mut changes = Vec::new();

    for line in out.lines() {
        if let Some(name) = line.strip_prefix("# branch.head ") {
            branch = Some(name.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            // "+N -M"
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if let Some(rest) = line.strip_prefix("1 ") {
            if let Some(change) = parse_ordinary(rest) {
                changes.push(change);
            }
        } else if let Some(rest) = line.strip_prefix("2 ") {
            if let Some(change) = parse_renamed(rest) {
                changes.push(change);
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            changes.push(FileChange {
                status: "?".to_string(),
                path: path.to_string(),
            });
        }
        // "u " (unmerged) and other line kinds are ignored — a sidebar summary
        // does not need conflict-resolution detail.
    }

    (branch, ahead, behind, changes)
}

/// `<xy> <sub> <mH> <mI> <mW> <hH> <hI> <path>` — we only need `xy` and the
/// trailing path, which is always the last whitespace-separated field for an
/// ordinary (non-renamed) entry.
fn parse_ordinary(rest: &str) -> Option<FileChange> {
    let mut parts = rest.splitn(8, ' ');
    let xy = parts.next()?;
    let path = parts.last()?;
    Some(FileChange {
        status: xy_summary(xy),
        path: path.to_string(),
    })
}

/// Renamed/copied entries carry `<origPath>\t<newPath>`-style suffix after
/// the fixed fields; the path we display is the new one, before the tab.
fn parse_renamed(rest: &str) -> Option<FileChange> {
    let mut parts = rest.splitn(9, ' ');
    let xy = parts.next()?;
    let tail = parts.last()?;
    let path = tail.split('\t').next()?;
    Some(FileChange {
        status: xy_summary(xy),
        path: path.to_string(),
    })
}

/// Porcelain v2's two-character XY code, reduced to the single letter a
/// sidebar actually wants to show. Staged (X) takes priority over unstaged
/// (Y) for display purposes — "modified and staged" is still "modified".
fn xy_summary(xy: &str) -> String {
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    let letter = if x != '.' { x } else { y };
    letter.to_string()
}

fn parse_log(out: &str) -> Vec<LogEntry> {
    out.lines()
        .filter_map(|line| {
            // Graph lines start with characters like * | / \ before the commit info
            // Find where the graph ends and commit info begins
            let (graph, rest) = extract_graph(line);
            
            if rest.is_empty() {
                // Pure graph line (merge connectors)
                if !graph.is_empty() {
                    return Some(LogEntry {
                        sha: String::new(),
                        subject: String::new(),
                        author: String::new(),
                        date: String::new(),
                        refs: vec![],
                        graph,
                    });
                }
                return None;
            }
            
            let mut parts = rest.splitn(5, '\u{1f}');
            let sha = parts.next()?.to_string();
            let subject = parts.next().unwrap_or("").to_string();
            let author = parts.next().unwrap_or("").to_string();
            let date = parts.next().unwrap_or("").to_string();
            let refs_str = parts.next().unwrap_or("");
            
            let refs: Vec<String> = if refs_str.is_empty() {
                vec![]
            } else {
                refs_str
                    .split(", ")
                    .map(|s| s.trim().to_string())
                    .collect()
            };
            
            if sha.is_empty() {
                return None;
            }
            
            Some(LogEntry { sha, subject, author, date, refs, graph })
        })
        .collect()
}

/// Extract graph characters from the beginning of a log line.
/// Returns (graph_part, rest_of_line).
fn extract_graph(line: &str) -> (String, &str) {
    let graph_chars = ['*', '|', '/', '\\', ' ', '_'];
    let mut end = 0;
    
    for (i, c) in line.char_indices() {
        if graph_chars.contains(&c) {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    
    (line[..end].to_string(), line[end..].trim_start())
}

/// Collect all refs (branches, remotes, tags).
fn collect_refs(cwd: &Path, current_branch: Option<&str>) -> Vec<GitRef> {
    let mut refs = Vec::new();

    // Local branches
    if let Some(out) = run_git(cwd, &["branch", "--format=%(refname:short)"]) {
        for name in out.lines() {
            if !name.is_empty() {
                refs.push(GitRef {
                    name: name.to_string(),
                    kind: RefKind::LocalBranch,
                    is_current: current_branch == Some(name),
                });
            }
        }
    }

    // Remote branches
    if let Some(out) = run_git(cwd, &["branch", "-r", "--format=%(refname:short)"]) {
        for name in out.lines() {
            if !name.is_empty() && !name.contains("HEAD") {
                refs.push(GitRef {
                    name: name.to_string(),
                    kind: RefKind::RemoteBranch,
                    is_current: false,
                });
            }
        }
    }

    // Tags
    if let Some(out) = run_git(cwd, &["tag", "--sort=-creatordate"]) {
        for name in out.lines().take(20) {
            if !name.is_empty() {
                refs.push(GitRef {
                    name: name.to_string(),
                    kind: RefKind::Tag,
                    is_current: false,
                });
            }
        }
    }

    refs
}

/// Collect stash entries.
fn collect_stashes(cwd: &Path) -> Vec<String> {
    run_git(cwd, &["stash", "list", "--format=%gd: %s"])
        .unwrap_or_default()
        .lines()
        .take(10)
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_extracts_branch_and_ahead_behind() {
        let out = "# branch.oid abc123\n# branch.head main\n# branch.ab +2 -1\n";
        let (branch, ahead, behind, changes) = parse_status(out);
        assert_eq!(branch, Some("main".to_string()));
        assert_eq!(ahead, 2);
        assert_eq!(behind, 1);
        assert!(changes.is_empty());
    }

    #[test]
    fn parse_status_with_no_branch_head_returns_none() {
        // What `collect` sees when `cwd` is not a git repo at all: git exits
        // non-zero, so `run_git` returns None before this function even
        // runs — but if it somehow got empty/unrelated output, there must be
        // no branch to distinguish "not a repo" from "clean repo".
        let (branch, _, _, _) = parse_status("");
        assert_eq!(branch, None);
    }

    #[test]
    fn parse_status_ordinary_modified_file() {
        let out = "1 .M N... 100644 100644 100644 abc123 def456 src/main.rs\n";
        let (_, _, _, changes) = parse_status(out);
        assert_eq!(
            changes,
            vec![FileChange {
                status: "M".to_string(),
                path: "src/main.rs".to_string(),
            }]
        );
    }

    #[test]
    fn parse_status_staged_takes_priority_over_unstaged() {
        // "MM" = modified in index AND modified in worktree since. The
        // sidebar shows one letter; staged (X, first char) wins.
        let out = "1 MM N... 100644 100644 100644 abc123 def456 src/main.rs\n";
        let (_, _, _, changes) = parse_status(out);
        assert_eq!(changes[0].status, "M");
    }

    #[test]
    fn parse_status_untracked_file() {
        let out = "? new_file.txt\n";
        let (_, _, _, changes) = parse_status(out);
        assert_eq!(
            changes,
            vec![FileChange {
                status: "?".to_string(),
                path: "new_file.txt".to_string(),
            }]
        );
    }

    #[test]
    fn parse_status_renamed_file_uses_new_path() {
        // Porcelain v2 renamed entry: score field, then "R100", then the
        // orig\tnew path pair as the final field.
        let out =
            "2 R. N... 100644 100644 100644 abc123 def456 R100 old_name.rs\told_name.rs\tnew_name.rs\n";
        let (_, _, _, changes) = parse_status(out);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].status, "R");
    }

    #[test]
    fn parse_log_splits_sha_and_subject() {
        let out = "* abc1234\u{1f}Fix the thing\u{1f}dev\u{1f}2h ago\u{1f}HEAD -> main\n* def5678\u{1f}Add another thing\u{1f}dev\u{1f}3h ago\u{1f}\n";
        let entries = parse_log(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sha, "abc1234");
        assert_eq!(entries[0].subject, "Fix the thing");
        assert_eq!(entries[0].author, "dev");
        assert_eq!(entries[0].refs, vec!["HEAD -> main"]);
        assert_eq!(entries[0].graph, "* ");
        assert_eq!(entries[1].sha, "def5678");
        assert!(entries[1].refs.is_empty());
    }

    #[test]
    fn parse_log_with_graph_merges() {
        let out = "* abc123\u{1f}Merge\u{1f}dev\u{1f}1h ago\u{1f}\n|\\\n| * def456\u{1f}Feature\u{1f}dev\u{1f}2h ago\u{1f}origin/feature\n|/\n* ghi789\u{1f}Base\u{1f}dev\u{1f}3h ago\u{1f}\n";
        let entries = parse_log(out);
        // Should have commits and graph-only lines
        assert!(entries.len() >= 3);
        assert_eq!(entries[0].sha, "abc123");
        assert_eq!(entries[0].graph, "* ");
    }

    #[test]
    fn parse_log_ignores_blank_lines() {
        let out = "* abc1234\u{1f}One\u{1f}dev\u{1f}1h ago\u{1f}\n\n* def5678\u{1f}Two\u{1f}dev\u{1f}2h ago\u{1f}\n";
        let entries: Vec<_> = parse_log(out).into_iter().filter(|e| !e.sha.is_empty()).collect();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn xy_summary_prefers_x_over_y() {
        assert_eq!(xy_summary("M."), "M");
        assert_eq!(xy_summary(".D"), "D");
        assert_eq!(xy_summary(".."), ".");
    }

    /// End-to-end against a real temp repo — proves `collect` actually shells
    /// out correctly, not just that the parsers handle canned strings.
    #[test]
    fn collect_against_a_real_temp_repo() {
        let dir = std::env::temp_dir().join(format!(
            "kn9t_git_status_test_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git must be on PATH for this test")
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "initial commit"]);
        std::fs::write(dir.join("a.txt"), "hello world").unwrap();
        std::fs::write(dir.join("b.txt"), "new file").unwrap();

        let state = collect(&dir).expect("must detect a real repo");
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert_eq!(state.recent.len(), 1);
        assert_eq!(state.recent[0].subject, "initial commit");

        let statuses: Vec<&str> = state.changes.iter().map(|c| c.status.as_str()).collect();
        assert!(statuses.contains(&"M"), "a.txt must show modified");
        assert!(statuses.contains(&"?"), "b.txt must show untracked");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A directory that is not a git repo at all must yield `None`, not an
    /// empty-but-"clean" `GitState` — the UI needs to tell these apart.
    #[test]
    fn collect_outside_any_repo_returns_none() {
        let dir = std::env::temp_dir().join(format!(
            "kn9t_git_status_notrepo_{}_{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(collect(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
