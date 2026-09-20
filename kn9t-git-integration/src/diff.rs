//! Unified-diff parsing for the review panel.
//!
//! This is the half of the old TUI `diff_viewer.rs` that is genuinely *data*:
//! turning `git diff` output into files/hunks/lines. Rendering, navigation and
//! comment capture now live in `ui.lua`, which is why none of that appears
//! here.
//!
//! Running `git` belongs in this process: a plugin is a native executable with
//! full OS access, whereas the TUI's Lua is sandboxed (no `io`, no
//! `os.execute`) precisely so a config cannot shell out.

use std::path::Path;

/// One line of a hunk, tagged by its role in the diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

impl LineKind {
    /// Short tag used in the JSON pushed to Lua.
    pub fn tag(&self) -> &'static str {
        match self {
            LineKind::Context => "ctx",
            LineKind::Added => "add",
            LineKind::Removed => "del",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    /// Line number in the *new* file, `None` for removed lines.
    ///
    /// Tracked per-line while parsing rather than derived later as
    /// `hunk.new_start + index`: that shortcut counts removed lines as if they
    /// occupied a new-file line, so in any hunk containing a deletion every
    /// subsequent number drifts. Comments keyed on a drifted number attach to
    /// the wrong line, which is the bug this field exists to prevent.
    pub new_lineno: Option<u32>,
    /// Line number in the *old* file, `None` for added lines.
    pub old_lineno: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: u32,
    pub new_start: u32,
    /// The `@@ ... @@` header text, for display.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// What happened to a file, as reported by the diff header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
}

impl FileStatus {
    pub fn tag(&self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DiffFile {
    pub path: String,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
    pub additions: usize,
    pub deletions: usize,
}

/// Diff target specifier.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DiffTarget {
    /// Working tree vs HEAD (default).
    #[default]
    WorkingTree,
    /// Staged changes (index vs HEAD).
    Staged,
    /// Working tree vs a specific ref (branch, tag, or commit).
    Ref(String),
    /// Compare two refs: base..head.
    RefRange { base: String, head: String },
}

impl DiffTarget {
    pub fn label(&self) -> String {
        match self {
            DiffTarget::WorkingTree => "HEAD".to_string(),
            DiffTarget::Staged => "staged".to_string(),
            DiffTarget::Ref(r) => r.clone(),
            DiffTarget::RefRange { base, head } => format!("{base}..{head}"),
        }
    }
}

/// Collect the working-tree diff for `cwd`.
///
/// `HEAD` with no argument covers tracked modifications; untracked files are
/// absent by design (they have no diff to show, and the status list already
/// reports them).
pub fn collect(cwd: &Path) -> Vec<DiffFile> {
    collect_with_target(cwd, &DiffTarget::WorkingTree)
}

/// Collect diff with a specific target.
pub fn collect_with_target(cwd: &Path, target: &DiffTarget) -> Vec<DiffFile> {
    let args = match target {
        DiffTarget::WorkingTree => vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U3",
            "HEAD",
        ],
        DiffTarget::Staged => vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U3",
            "--cached",
        ],
        DiffTarget::Ref(r) => vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "-U3",
            r.as_str(),
        ],
        DiffTarget::RefRange { base, head } => {
            let range = format!("{base}..{head}");
            return collect_range(cwd, &range);
        }
    };

    let Some(out) = super::git::run_git_raw(cwd, &args) else {
        return Vec::new();
    };
    parse(&out)
}

fn collect_range(cwd: &Path, range: &str) -> Vec<DiffFile> {
    let args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-U3",
        range,
    ];
    let Some(out) = super::git::run_git_raw(cwd, &args) else {
        return Vec::new();
    };
    parse(&out)
}

/// Parse unified diff text into per-file hunks.
pub fn parse(input: &str) -> Vec<DiffFile> {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut cur: Option<DiffFile> = None;
    let mut hunk: Option<Hunk> = None;
    // Running counters, advanced per line so each line records its own real
    // number instead of being back-computed from its index.
    let mut new_no = 0u32;
    let mut old_no = 0u32;

    for line in input.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(mut f) = cur.take() {
                if let Some(h) = hunk.take() {
                    f.hunks.push(h);
                }
                files.push(f);
            }
            cur = Some(DiffFile {
                path: path_from_diff_header(rest),
                status: FileStatus::Modified,
                hunks: Vec::new(),
                additions: 0,
                deletions: 0,
            });
            continue;
        }

        let Some(file) = cur.as_mut() else { continue };

        // `new file` / `deleted file` are authoritative. Inferring status from
        // whether a file has only additions is wrong for a file whose every
        // line genuinely changed.
        if line.starts_with("new file mode") {
            file.status = FileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode") {
            file.status = FileStatus::Deleted;
            continue;
        }
        // Prefer the real post-image path: it survives renames, where the
        // `diff --git` line's own a/ b/ pair is the *old* name.
        if let Some(p) = line.strip_prefix("+++ b/") {
            if p != "/dev/null" {
                file.path = p.to_string();
            }
            continue;
        }
        if line.starts_with("--- ") || line.starts_with("index ") {
            continue;
        }

        if line.starts_with("@@") {
            if let Some(h) = hunk.take() {
                file.hunks.push(h);
            }
            let (old_start, new_start) = parse_hunk_header(line);
            old_no = old_start;
            new_no = new_start;
            hunk = Some(Hunk {
                old_start,
                new_start,
                header: line.to_string(),
                lines: Vec::new(),
            });
            continue;
        }

        let Some(h) = hunk.as_mut() else { continue };

        // "\ No newline at end of file" is metadata, not content.
        if line.starts_with('\\') {
            continue;
        }

        let (kind, text) = match line.as_bytes().first() {
            Some(b'+') => (LineKind::Added, &line[1..]),
            Some(b'-') => (LineKind::Removed, &line[1..]),
            Some(b' ') => (LineKind::Context, &line[1..]),
            // A truly empty line inside a hunk is a context line whose single
            // space git elided.
            None => (LineKind::Context, ""),
            _ => continue,
        };

        let (new_lineno, old_lineno) = match kind {
            LineKind::Added => {
                let n = new_no;
                new_no += 1;
                file.additions += 1;
                (Some(n), None)
            }
            LineKind::Removed => {
                let o = old_no;
                old_no += 1;
                file.deletions += 1;
                (None, Some(o))
            }
            LineKind::Context => {
                let (n, o) = (new_no, old_no);
                new_no += 1;
                old_no += 1;
                (Some(n), Some(o))
            }
        };

        h.lines.push(DiffLine {
            kind,
            text: text.to_string(),
            new_lineno,
            old_lineno,
        });
    }

    if let Some(mut f) = cur.take() {
        if let Some(h) = hunk.take() {
            f.hunks.push(h);
        }
        files.push(f);
    }
    files
}

/// Extract a path from the `diff --git a/X b/Y` tail.
///
/// Uses the b/ side. Paths containing spaces make this ambiguous in general;
/// the `+++ b/` line resolves it properly and overwrites whatever we guess
/// here, so this only has to be right for the common case.
fn path_from_diff_header(rest: &str) -> String {
    if let Some(idx) = rest.find(" b/") {
        return rest[idx + 3..].to_string();
    }
    rest.split_whitespace()
        .next_back()
        .unwrap_or("?")
        .trim_start_matches("b/")
        .to_string()
}

/// Parse `@@ -old,count +new,count @@` into starting line numbers.
fn parse_hunk_header(line: &str) -> (u32, u32) {
    let mut old_start = 1;
    let mut new_start = 1;
    for tok in line.split_whitespace() {
        if let Some(t) = tok.strip_prefix('-') {
            old_start = first_number(t).unwrap_or(1);
        } else if let Some(t) = tok.strip_prefix('+') {
            new_start = first_number(t).unwrap_or(1);
        }
    }
    (old_start, new_start)
}

fn first_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 111..222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,7 +10,8 @@ fn main() {
 context one
-removed line
+added line
+another added
 context two
";

    #[test]
    fn parses_one_file_with_counts() {
        let files = parse(SAMPLE);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/main.rs");
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].additions, 2);
        assert_eq!(files[0].deletions, 1);
    }

    /// The regression the old TUI viewer had: it derived a comment's line as
    /// `new_start + index`, which counts the removed line and so shifts every
    /// following number by one.
    #[test]
    fn removed_lines_do_not_shift_new_file_numbering() {
        let files = parse(SAMPLE);
        let lines = &files[0].hunks[0].lines;

        assert_eq!(lines[0].new_lineno, Some(10)); // context one
        assert_eq!(lines[1].new_lineno, None); // removed: absent from new file
        assert_eq!(lines[2].new_lineno, Some(11)); // added line
        assert_eq!(lines[3].new_lineno, Some(12)); // another added
        assert_eq!(lines[4].new_lineno, Some(13)); // context two

        // Old-file numbering advances only on context/removed.
        assert_eq!(lines[0].old_lineno, Some(10));
        assert_eq!(lines[1].old_lineno, Some(11));
        assert_eq!(lines[2].old_lineno, None);
        assert_eq!(lines[4].old_lineno, Some(12));
    }

    #[test]
    fn detects_added_and_deleted_files() {
        let added = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+one
+two
";
        let files = parse(added);
        assert_eq!(files[0].status, FileStatus::Added);
        assert_eq!(files[0].path, "new.txt");

        let deleted = "\
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-bye
";
        let files = parse(deleted);
        assert_eq!(files[0].status, FileStatus::Deleted);
        // `+++ /dev/null` must not overwrite the path with "/dev/null".
        assert_eq!(files[0].path, "gone.txt");
    }

    #[test]
    fn parses_multiple_files_and_hunks() {
        let input = format!(
            "{SAMPLE}\
diff --git a/b.rs b/b.rs
--- a/b.rs
+++ b/b.rs
@@ -1,2 +1,2 @@
-x
+y
@@ -20,2 +20,2 @@
-p
+q
"
        );
        let files = parse(&input);
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path, "b.rs");
        assert_eq!(files[1].hunks.len(), 2, "both hunks kept");
        assert_eq!(files[1].hunks[1].new_start, 20);
    }

    #[test]
    fn empty_input_is_not_an_error() {
        assert!(parse("").is_empty());
    }

    /// A blank context line is emitted by git without its leading space; it is
    /// still content and must not be dropped or misread as a header.
    #[test]
    fn blank_context_line_is_kept() {
        let input = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1,3 +1,3 @@
 first

-old
+new
";
        let files = parse(input);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1].kind, LineKind::Context);
        assert_eq!(lines[1].text, "");
        assert_eq!(lines[2].kind, LineKind::Removed);
    }

    #[test]
    fn no_newline_marker_is_ignored() {
        let input = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-a
\\ No newline at end of file
+b
";
        let files = parse(input);
        let lines = &files[0].hunks[0].lines;
        assert_eq!(lines.len(), 2, "marker is metadata, not a line");
    }

    #[test]
    fn rename_uses_the_new_path() {
        let input = "\
diff --git a/old/name.rs b/new/name.rs
similarity index 90%
rename from old/name.rs
rename to new/name.rs
--- a/old/name.rs
+++ b/new/name.rs
@@ -1 +1 @@
-a
+b
";
        let files = parse(input);
        assert_eq!(files[0].path, "new/name.rs");
    }
}
