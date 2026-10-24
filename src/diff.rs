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

pub(crate) fn parse_diff(diff: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut current: Option<ChangedFile> = None;
    // Seen before the `+++` line, which completes the file header.
    let mut blobs: (Option<String>, Option<String>) = (None, None);
    let mut old_path: Option<String> = None;
    let mut quoted = false;
    // Next line number within the current hunk, per side.
    let (mut old_line, mut new_line) = (0usize, 0usize);
    // Lines still owed to the current hunk per side. While either is non-zero every line is
    // content, so a body like `++x` (diff line `+++x`) is never mistaken for a file header.
    let (mut old_rem, mut new_rem) = (0usize, 0usize);

    for line in diff.lines() {
        if old_rem > 0 || new_rem > 0 {
            let first = line.as_bytes().first();
            // Content lines always carry a prefix char; anything else means the hunk ended
            // short (malformed input) — fall through and treat it as a header.
            if matches!(first, None | Some(b'+' | b'-' | b' ' | b'\\')) {
                let lines = current.as_mut().map(|f| &mut f.lines);
                match first {
                    Some(b'+') => {
                        if let Some(lines) = lines {
                            lines.push(ChangedLine::Added(new_line));
                        }
                        new_line += 1;
                        new_rem = new_rem.saturating_sub(1);
                    }
                    Some(b'-') => {
                        if let Some(lines) = lines {
                            lines.push(ChangedLine::Removed(old_line));
                        }
                        old_line += 1;
                        old_rem = old_rem.saturating_sub(1);
                    }
                    Some(b'\\') => {} // `\ No newline at end of file`
                    // Context; an empty line is context whose leading space was stripped
                    // (`diff.suppressBlankEmpty`).
                    _ => {
                        old_line += 1;
                        new_line += 1;
                        old_rem = old_rem.saturating_sub(1);
                        new_rem = new_rem.saturating_sub(1);
                    }
                }
                continue;
            }
            old_rem = 0;
            new_rem = 0;
        }

        if line.starts_with("diff --git") {
            if let Some(f) = current.take() {
                files.push(f);
            }
            blobs = (None, None);
            old_path = None;
            quoted = false;
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
        if let Some(path) = line.strip_prefix("--- a/") {
            old_path = Some(header_path(path));
            continue;
        }
        if line.starts_with("--- \"") || line.starts_with("+++ \"") {
            // C-quoted path (`"`, `\`, control chars survive core.quotePath=false). Not
            // unquoted here; say so rather than drop the file silently.
            if !quoted {
                eprintln!("chsig: skipping quoted path {}", &line[4..]);
            }
            quoted = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            let new_path = rest.strip_prefix("b/").map(header_path);
            if !quoted {
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
                old_line = h.old_start;
                new_line = h.new_start;
                old_rem = h.old_count;
                new_rem = h.new_count;
            }
            continue;
        }
        // Remaining lines outside a hunk (mode, rename, binary notices) carry nothing
        // parse_diff uses.
    }
    if let Some(f) = current.take() {
        files.push(f);
    }
    files
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
        let files = parse_diff(diff);
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
        let files = parse_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].lines, vec![Removed(2), Added(2), Added(3),]);
        assert_eq!(files[1].path(), "y.rs");
        assert_eq!(files[1].lines.len(), 2);
    }

    // git appends a tab to `+++ b/<path>` when the path contains a space; `diff.suppressBlankEmpty`
    // emits empty context lines without the leading space — both must keep line numbers right.
    #[test]
    fn parse_diff_strips_header_tab_and_counts_empty_context() {
        let diff = "diff --git a/s p.rs b/s p.rs\nindex abc1234..def5678 100644\n--- a/s p.rs\t\n+++ b/s p.rs\t\n@@ -1,2 +1,3 @@\n fn a() {\n\n+x\n";
        let files = parse_diff(diff);
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
        let files = parse_diff(diff);
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
