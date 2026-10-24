//! Parse `git diff` output into per-file changed lines, each numbered on its own side.

pub(crate) struct ChangedFile {
    /// `None` when the file was added.
    pub(crate) old: Option<Side>,
    /// `None` when the file was deleted.
    pub(crate) new: Option<Side>,
    pub(crate) lines: Vec<ChangedLine>,
}

impl ChangedFile {
    /// The path to show: the new one, or the old one for a deleted file.
    pub(crate) fn path(&self) -> &str {
        let side = self.new.as_ref().or(self.old.as_ref());
        side.map_or("", |s| s.path.as_str())
    }
}

pub(crate) struct Side {
    pub(crate) path: String,
    /// Blob SHA from the `index old..new` header. The old one always names a stored object
    /// (index or commit). The new one does too for `--cached` and ranges, but in a plain
    /// working diff it names unstaged content that isn't stored, so that side is read from
    /// the working file instead.
    pub(crate) blob: String,
}

/// A changed line, numbered on the side it exists on.
#[derive(Debug, PartialEq)]
pub(crate) enum ChangedLine {
    Added(usize),
    Removed(usize),
}

/// git's output in a shape `parse_diff` doesn't read, holding the first line that showed it:
/// an option changed the format (`--color`, `--word-diff`, `--stat`, `-R`). Read on, it would
/// yield no changed lines and an empty result indistinguishable from a diff without symbols.
#[derive(Debug)]
pub(crate) struct Unreadable(pub(crate) String);

/// A file the diff names but `parse_diff` doesn't read, by its header path. The caller says so,
/// rather than the file dropping out silently.
#[derive(Debug, PartialEq)]
pub(crate) enum Skipped {
    /// C-quoted (`"`, `\`, control chars survive core.quotePath=false). Kept in its quoted form,
    /// prefix dropped, since it is not unquoted here.
    Quoted(String),
    /// A combined diff, which git shows for a merge or a conflicted worktree file even when
    /// asked for nothing special; its two-column lines read as no single side's.
    Combined(String),
}

pub(crate) struct ParsedDiff {
    pub(crate) files: Vec<ChangedFile>,
    pub(crate) skipped: Vec<Skipped>,
}

/// The hunk being read: the next line number and the lines still owed, per side.
#[derive(Default)]
struct HunkState<'a> {
    old_line: usize,
    new_line: usize,
    old_remaining: usize,
    new_remaining: usize,
    /// The header, until one of the hunk's lines is added or removed: git emits no hunk
    /// without a change, so one that ends unchanged was read wrongly.
    unchanged_header: Option<&'a str>,
}

impl<'a> HunkState<'a> {
    /// While lines are owed every line is content, so a body like `++x` (diff line `+++x`) is
    /// never mistaken for a file header.
    fn active(&self) -> bool {
        self.old_remaining > 0 || self.new_remaining > 0
    }

    fn start(&mut self, header: &'a str, hunk: HunkHeader) {
        self.old_line = hunk.old_start;
        self.new_line = hunk.new_start;
        self.old_remaining = hunk.old_count;
        self.new_remaining = hunk.new_count;
        self.unchanged_header = Some(header);
    }

    fn consume(
        &mut self,
        line: &str,
        changed: Option<&mut Vec<ChangedLine>>,
    ) -> Result<(), Unreadable> {
        match line.as_bytes().first() {
            Some(b'+') => {
                if let Some(changed) = changed {
                    changed.push(ChangedLine::Added(self.new_line));
                }
                self.new_line += 1;
                self.new_remaining = self.new_remaining.saturating_sub(1);
                self.unchanged_header = None;
            }
            Some(b'-') => {
                if let Some(changed) = changed {
                    changed.push(ChangedLine::Removed(self.old_line));
                }
                self.old_line += 1;
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.unchanged_header = None;
            }
            Some(b'\\') => {} // `\ No newline at end of file`
            // Context; an empty line is context whose leading space was stripped
            // (`diff.suppressBlankEmpty`).
            None | Some(b' ') => {
                self.old_line += 1;
                self.new_line += 1;
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
            }
            // A diff's content lines always carry a prefix; `--word-diff` lines don't.
            Some(_) => return Err(Unreadable(line.to_string())),
        }
        if !self.active()
            && let Some(header) = self.unchanged_header
        {
            return Err(Unreadable(header.to_string()));
        }
        Ok(())
    }
}

pub(crate) fn parse_diff(diff: &str) -> Result<ParsedDiff, Unreadable> {
    let unreadable = |line: &str| Err(Unreadable(line.to_string()));
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut seen_header = false;
    let mut current: Option<ChangedFile> = None;
    // Seen before the `+++` line, which completes the file header.
    let mut blobs: (Option<String>, Option<String>) = (None, None);
    let mut old_path: Option<String> = None;
    let mut combined = false;
    let mut hunk = HunkState::default();

    for line in diff.lines() {
        if hunk.active() {
            hunk.consume(line, current.as_mut().map(|f| &mut f.lines))?;
            continue;
        }

        if line.starts_with("diff --git") {
            seen_header = true;
            if let Some(f) = current.take() {
                files.push(f);
            }
            blobs = (None, None);
            old_path = None;
            combined = false;
            continue;
        }
        if let Some(path) = ["diff --cc ", "diff --combined "]
            .iter()
            .find_map(|p| line.strip_prefix(p))
        {
            seen_header = true;
            if let Some(f) = current.take() {
                files.push(f);
            }
            skipped.push(Skipped::Combined(path.to_string()));
            combined = true;
            continue;
        }
        if combined {
            continue;
        }
        if let Some(rest) = line.strip_prefix("index ") {
            // `index <old>..<new> <mode>`; an all-zeros SHA means that side doesn't exist.
            let real = |s: &str| (!s.chars().all(|c| c == '0')).then(|| s.to_string());
            if let Some((old, new)) = rest
                .split_whitespace()
                .next()
                .and_then(|r| r.split_once(".."))
            {
                blobs = (real(old), real(new));
            }
            continue;
        }
        // git appends a tab to a header path that contains a space.
        let header_path = |p: &str| p.strip_suffix('\t').unwrap_or(p).to_string();
        // The pinned prefixes, inside the quotes of a C-quoted path too; anything else (`-R`
        // swaps them) would leave both sides unread.
        let side_path = |rest: &str, prefix: &str| {
            let (quote, unquoted) = match rest.strip_prefix('"') {
                Some(inner) => ("\"", inner),
                None => ("", rest),
            };
            match unquoted.strip_prefix(prefix) {
                Some(path) => Ok(Some(format!("{quote}{}", header_path(path)))),
                None if rest == "/dev/null" => Ok(None),
                None => Err(Unreadable(line.to_string())),
            }
        };
        if let Some(rest) = line.strip_prefix("--- ") {
            old_path = side_path(rest, "a/")?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let new_path = side_path(rest, "b/")?;
            // Only a C-quoted path starts with `"`.
            let quoted = [&old_path, &new_path]
                .into_iter()
                .flatten()
                .find(|p| p.starts_with('"'));
            if let Some(path) = quoted {
                skipped.push(Skipped::Quoted(path.clone()));
                old_path = None;
            } else {
                let (old_blob, new_blob) = std::mem::take(&mut blobs);
                let side = |path: Option<String>, blob: Option<String>| {
                    Some(Side {
                        path: path?,
                        blob: blob?,
                    })
                };
                current = Some(ChangedFile {
                    old: side(old_path.take(), old_blob),
                    new: side(new_path, new_blob),
                    lines: Vec::new(),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(h) = parse_hunk_header(rest) {
                hunk.start(line, h);
            }
            continue;
        }
        // Remaining lines outside a hunk (mode, rename, binary notices) carry nothing
        // parse_diff uses.
    }
    if let Some(f) = current.take() {
        files.push(f);
    }
    // `--stat`, `--name-only` and colored output carry no `diff --git` line at all.
    if !seen_header && let Some(first) = diff.lines().find(|l| !l.trim().is_empty()) {
        return unreadable(first);
    }
    Ok(ParsedDiff { files, skipped })
}

struct HunkHeader {
    old_start: usize,
    old_count: usize,
    new_start: usize,
    new_count: usize,
}

/// `rest` is the part after `@@ `, e.g. "-10,7 +12,9 @@ fn foo()". An omitted count is 1.
fn parse_hunk_header(rest: &str) -> Option<HunkHeader> {
    let range = |prefix: char| -> Option<(usize, usize)> {
        let tok = rest.split_whitespace().find(|t| t.starts_with(prefix))?;
        let mut it = tok[1..].splitn(2, ',');
        let start = it.next()?.parse().ok()?;
        let count = match it.next() {
            Some(c) => c.parse().ok()?,
            None => 1,
        };
        Some((start, count))
    };
    let (old_start, old_count) = range('-')?;
    let (new_start, new_count) = range('+')?;
    Some(HunkHeader {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use ChangedLine::{Added, Removed};

    #[test]
    fn parse_diff_numbers_each_line_on_its_own_side() {
        let diff = "\
diff --git a/src/x.ts b/src/x.ts
index abc1234..def5678 100644
--- a/src/x.ts
+++ b/src/x.ts
@@ -10,4 +20,5 @@
 line1
-old1
-old2
+new1
+new2
+new3
 line3
";
        let files = parse_diff(diff).unwrap().files;
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path(), "src/x.ts");
        assert_eq!(f.old.as_ref().map(|s| s.blob.as_str()), Some("abc1234"));
        assert_eq!(f.new.as_ref().map(|s| s.blob.as_str()), Some("def5678"));
        assert_eq!(
            f.lines,
            vec![Removed(11), Removed(12), Added(21), Added(22), Added(23),]
        );
    }

    // Bodies starting with `++`/`--` produce diff lines `+++x`/`---x`. Inside a hunk they are
    // content, not file headers: they must be kept, keep line numbers aligned, and not end the
    // file. The second file proves the next real header is still recognized.
    #[test]
    fn parse_diff_keeps_header_lookalike_bodies_inside_hunk() {
        let diff = "\
diff --git a/x.rs b/x.rs
index abc1234..def5678 100644
--- a/x.rs
+++ b/x.rs
@@ -1,3 +1,4 @@
 fn a() {
--- a sql comment
+++n;
+if y {}
 }
diff --git a/y.rs b/y.rs
index 1111111..2222222 100644
--- a/y.rs
+++ b/y.rs
@@ -1 +1 @@
-old
+new
";
        let files = parse_diff(diff).unwrap().files;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].lines, vec![Removed(2), Added(2), Added(3),]);
        assert_eq!(files[1].path(), "y.rs");
        assert_eq!(files[1].lines.len(), 2);
    }

    #[test]
    fn no_newline_marker_does_not_consume_a_hunk_line() {
        let diff = "\
diff --git a/x.rs b/x.rs
index abc1234..def5678 100644
--- a/x.rs
+++ b/x.rs
@@ -1 +1 @@
-old
\\ No newline at end of file
+new
diff --git a/y.rs b/y.rs
index 1111111..2222222 100644
--- a/y.rs
+++ b/y.rs
@@ -1 +1 @@
-before
+after
";
        let files = parse_diff(diff).unwrap().files;
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].lines, vec![Removed(1), Added(1)]);
        assert_eq!(files[1].path(), "y.rs");
        assert_eq!(files[1].lines, vec![Removed(1), Added(1)]);
    }

    // git appends a tab to `+++ b/<path>` when the path contains a space; `diff.suppressBlankEmpty`
    // emits empty context lines without the leading space — both must keep line numbers right.
    #[test]
    fn parse_diff_strips_header_tab_and_counts_empty_context() {
        let diff = "diff --git a/s p.rs b/s p.rs\nindex abc1234..def5678 100644\n--- a/s p.rs\t\n+++ b/s p.rs\t\n@@ -1,2 +1,3 @@\n fn a() {\n\n+x\n";
        let files = parse_diff(diff).unwrap().files;
        assert_eq!(files[0].path(), "s p.rs");
        assert_eq!(files[0].old.as_ref().unwrap().path, "s p.rs");
        assert_eq!(files[0].lines, vec![Added(3)]);
    }

    // A deleted file has only an old side and an added one only a new side; a rename keeps
    // both paths.
    #[test]
    fn parse_diff_tracks_sides_of_deleted_renamed_and_added_files() {
        let diff = "\
diff --git a/gone.rs b/gone.rs
deleted file mode 100644
index abc1234..0000000
--- a/gone.rs
+++ /dev/null
@@ -1,2 +0,0 @@
-fn a() {
-}
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
index 1111111..2222222 100644
--- a/old.rs
+++ b/new.rs
@@ -3 +3 @@
-x
+y
diff --git a/born.rs b/born.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/born.rs
@@ -0,0 +1 @@
+fn b() {}
";
        let files = parse_diff(diff).unwrap().files;
        let sides: Vec<_> = files
            .iter()
            .map(|f| {
                (
                    f.old.as_ref().map(|s| s.path.as_str()),
                    f.new.as_ref().map(|s| s.path.as_str()),
                    f.path(),
                )
            })
            .collect();
        assert_eq!(
            sides,
            vec![
                (Some("gone.rs"), None, "gone.rs"),
                (Some("old.rs"), Some("new.rs"), "new.rs"),
                (None, Some("born.rs"), "born.rs"),
            ]
        );
        assert_eq!(files[0].lines, vec![Removed(1), Removed(2)]);
        assert_eq!(files[1].lines, vec![Removed(3), Added(3)]);
    }

    // Output reshaped by an option is refused rather than read into an empty result: `-R`
    // swaps the prefixes, `--word-diff` drops the line prefixes (or, on an indented line,
    // leaves a space that reads as context, so the hunk ends unchanged), and `--stat` has no
    // file header at all.
    #[test]
    fn reshaped_output_is_unreadable() {
        let file = "diff --git a/x.rs b/x.rs\nindex abc1234..def5678 100644\n";
        let headers = format!("{file}--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n");
        let cases = [
            (format!("{file}--- b/x.rs\n+++ a/x.rs\n"), "--- b/x.rs"),
            (format!("{headers}x = [-1-]{{+2+}}\n"), "x = [-1-]{+2+}"),
            (format!("{headers}    x = [-1-]{{+2+}}\n"), "@@ -1 +1 @@"),
            (" x.rs | 2 +-\n".to_string(), " x.rs | 2 +-"),
        ];
        for (diff, at) in cases {
            assert_eq!(parse_diff(&diff).err().map(|u| u.0).as_deref(), Some(at));
        }
    }

    // A combined diff's file is skipped, not read as an option's reshaping; files around it
    // are still read.
    #[test]
    fn combined_diff_files_are_skipped() {
        let diff = "\
diff --cc a.rs
index 1111111,2222222..3333333
--- a/a.rs
+++ b/a.rs
@@@ -1,3 -1,3 +1,4 @@@
  fn a() {
--- x
 -    let x = 2;
++    if x > 1 {}
diff --git a/b.rs b/b.rs
index 1111111..2222222 100644
--- a/b.rs
+++ b/b.rs
@@ -1 +1 @@
-old
+new
";
        let parsed = parse_diff(diff).unwrap();
        assert_eq!(parsed.skipped, vec![Skipped::Combined("a.rs".to_string())]);
        let paths: Vec<_> = parsed.files.iter().map(|f| f.path()).collect();
        assert_eq!(paths, vec!["b.rs"]);
        assert_eq!(parsed.files[0].lines, vec![Removed(1), Added(1)]);
        let alone = parse_diff(&diff[..diff.find("diff --git").unwrap()]).unwrap();
        assert!(alone.files.is_empty());
    }

    // A quoted path is skipped by its quoted form, without the prefix; a reshaped prefix
    // inside the quotes is refused as it is outside them.
    #[test]
    fn quoted_paths_are_skipped_without_their_prefix() {
        let diff = "\
diff --git \"a/q\\\"x.rs\" \"b/q\\\"x.rs\"
index 1111111..2222222 100644
--- \"a/q\\\"x.rs\"
+++ \"b/q\\\"x.rs\"
@@ -1 +1 @@
-old
+new
";
        let parsed = parse_diff(diff).unwrap();
        assert_eq!(
            parsed.skipped,
            vec![Skipped::Quoted("\"q\\\"x.rs\"".to_string())]
        );
        assert!(parsed.files.is_empty());
        let reversed = diff.replace("--- \"a/", "--- \"b/");
        assert!(parse_diff(&reversed).is_err());
    }

    #[test]
    fn hunk_header_reads_counts_with_default_one() {
        let h = parse_hunk_header("-10,7 +12,9 @@ fn foo()").unwrap();
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (10, 7, 12, 9)
        );
        let h = parse_hunk_header("-1 +1 @@").unwrap();
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (1, 1, 1, 1)
        );
        let h = parse_hunk_header("-3,2 +2,0 @@").unwrap();
        assert_eq!(
            (h.old_start, h.old_count, h.new_start, h.new_count),
            (3, 2, 2, 0)
        );
    }
}
