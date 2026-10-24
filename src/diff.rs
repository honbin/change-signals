//! Parse `git diff` output into per-file changed lines, numbered on the new side.

pub(crate) struct ChangedFile {
    pub(crate) path: String,
    /// New-side blob SHA from the `index old..new` header (all-zeros filtered to `None`).
    /// Used only when the new side is read as a `Blob` (`--cached`, ranges). A plain working
    /// diff also carries a non-zero SHA here, but it names unstaged content that isn't a
    /// stored object, so it is ignored and that side is read from the working file instead.
    pub(crate) new_blob: Option<String>,
    /// Changed lines as (new-side line, sign, body-without-+/-). Sign is +1 for additions,
    /// -1 for removals, so a removed branch token subtracts rather than adds.
    pub(crate) changed_bodies: Vec<(usize, i32, String)>,
}

pub(crate) fn parse_diff(diff: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut current: Option<ChangedFile> = None;
    let mut pending_blob: Option<String> = None; // new-side SHA seen before the `+++ b/` line
    let mut new_line = 0usize; // next new-side line number within the current hunk
    // Lines still owed to the current hunk per side. While either is non-zero every line is
    // content, so a body like `++x` (diff line `+++x`) is never mistaken for a file header.
    let (mut old_rem, mut new_rem) = (0usize, 0usize);

    for line in diff.lines() {
        if old_rem > 0 || new_rem > 0 {
            let first = line.as_bytes().first();
            // Content lines always carry a prefix char; anything else means the hunk ended
            // short (malformed input) — fall through and treat it as a header.
            if matches!(first, None | Some(b'+' | b'-' | b' ' | b'\\')) {
                let f = current.as_mut();
                match first {
                    Some(b'+') => {
                        if let Some(f) = f {
                            f.changed_bodies.push((new_line, 1, line[1..].to_string()));
                        }
                        new_line += 1;
                        new_rem = new_rem.saturating_sub(1);
                    }
                    Some(b'-') => {
                        // A removed line has no new-side position, so attribute it to where it
                        // was removed (the next new line); its branch tokens subtract.
                        if let Some(f) = f {
                            f.changed_bodies.push((new_line, -1, line[1..].to_string()));
                        }
                        old_rem = old_rem.saturating_sub(1);
                    }
                    Some(b'\\') => {} // `\ No newline at end of file`
                    // Context; an empty line is context whose leading space was stripped
                    // (`diff.suppressBlankEmpty`).
                    _ => {
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
            pending_blob = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("index ") {
            // `index <old>..<new> <mode>` — keep the new SHA unless it's all zeros (no blob).
            pending_blob = rest
                .split_whitespace()
                .next()
                .and_then(|r| r.split_once(".."))
                .map(|(_, new)| new.to_string())
                .filter(|s| !s.chars().all(|c| c == '0'));
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ b/") {
            // git appends a tab to the header when the path contains a space.
            let path = path.strip_suffix('\t').unwrap_or(path);
            current = Some(ChangedFile {
                path: path.to_string(),
                new_blob: pending_blob.take(),
                changed_bodies: Vec::new(),
            });
            continue;
        }
        if line.starts_with("+++ \"") {
            // C-quoted path (`"`, `\`, control chars survive core.quotePath=false). Not
            // unquoted here; say so rather than drop the file silently.
            eprintln!("chsig: skipping quoted path {}", &line[4..]);
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(h) = parse_hunk_header(rest) {
                new_line = h.new_start;
                old_rem = h.old_count;
                new_rem = h.new_count;
            }
            continue;
        }
        // Remaining lines outside a hunk (`---`/`+++ /dev/null`, mode, rename, binary
        // notices) carry nothing parse_diff uses.
    }
    if let Some(f) = current.take() {
        files.push(f);
    }
    files
}

struct HunkHeader {
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
    let (_, old_count) = range('-')?;
    let (new_start, new_count) = range('+')?;
    Some(HunkHeader {
        old_count,
        new_start,
        new_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_tracks_signed_bodies_and_blob() {
        let diff = "\
diff --git a/src/x.ts b/src/x.ts
index abc1234..def5678 100644
--- a/src/x.ts
+++ b/src/x.ts
@@ -1,3 +1,4 @@
 line1
-old
+new1
+new2
 line3
";
        let files = parse_diff(diff);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, "src/x.ts");
        assert_eq!(f.new_blob, Some("def5678".into())); // new-side blob, read instead of worktree
        // `-old`, `+new1`, `+new2` all land in bodies with signs; removal is -1 at line 2.
        assert_eq!(f.changed_bodies.len(), 3);
        assert_eq!(f.changed_bodies[0], (2, -1, "old".into()));
        assert_eq!(f.changed_bodies[1], (2, 1, "new1".into()));
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
        assert_eq!(
            files[0].changed_bodies,
            vec![
                (2, -1, "-- a sql comment".to_string()),
                (2, 1, "++n;".to_string()),
                (3, 1, "if y {}".to_string()),
            ]
        );
        assert_eq!(files[1].path, "y.rs");
        assert_eq!(files[1].changed_bodies.len(), 2);
    }

    // git appends a tab to `+++ b/<path>` when the path contains a space; `diff.suppressBlankEmpty`
    // emits empty context lines without the leading space — both must keep line numbers right.
    #[test]
    fn parse_diff_strips_header_tab_and_counts_empty_context() {
        let diff = "diff --git a/s p.rs b/s p.rs\nindex abc1234..def5678 100644\n--- a/s p.rs\t\n+++ b/s p.rs\t\n@@ -1,2 +1,3 @@\n fn a() {\n\n+x\n";
        let files = parse_diff(diff);
        assert_eq!(files[0].path, "s p.rs");
        assert_eq!(files[0].changed_bodies, vec![(3, 1, "x".to_string())]);
    }

    #[test]
    fn hunk_header_reads_counts_with_default_one() {
        let h = parse_hunk_header("-10,7 +12,9 @@ fn foo()").unwrap();
        assert_eq!((h.old_count, h.new_start, h.new_count), (7, 12, 9));
        let h = parse_hunk_header("-1 +1 @@").unwrap();
        assert_eq!((h.old_count, h.new_start, h.new_count), (1, 1, 1));
        let h = parse_hunk_header("-3,2 +2,0 @@").unwrap();
        assert_eq!((h.old_count, h.new_start, h.new_count), (2, 2, 0));
    }
}
