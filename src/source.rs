//! Read each side of a changed file from where the diff says it lives: a stored blob, or the
//! working file, which is read only if it hashes to the diff's new-side blob.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::diff::Side;

/// Where the new side of the diff lives, decided once from the invocation. The two are never
/// mixed: reading the wrong source attributes changes against content whose line numbers
/// don't match the diff.
#[derive(Clone, Copy)]
pub(crate) enum SourceSide {
    /// Plain `git diff`, `git diff <rev>`, path-limited diff: the new side is the working file.
    Worktree,
    /// `--cached`/`--staged`, a `..`/`...`/`^!`/`^-` range, or two revisions: the new side is a
    /// stored blob.
    Blob,
}

/// Classify the supported git diff forms (the test fixes the exact contract).
/// Pathspecs should follow `--` to avoid ambiguity.
/// This is not a general-purpose git argument parser.
pub(crate) fn source_side(args: &[String]) -> SourceSide {
    let mut revs = 0;
    for a in args {
        if a == "--" {
            break; // pathspecs follow; they don't change the side
        }
        if a == "--cached" || a == "--staged" {
            return SourceSide::Blob;
        }
        // Only revs carry range syntax; an option's attached value (`-G'[^-]x'`) must not
        // read as one. A value passed as a separate arg is still misread.
        if a.starts_with('-') {
            continue;
        }
        if a.contains("..") || a.contains("^!") || a.contains("^-") {
            return SourceSide::Blob;
        }
        revs += 1;
    }
    if revs >= 2 {
        SourceSide::Blob
    } else {
        SourceSide::Worktree
    }
}

/// Read the new side from the chosen source — no fallback between them. On failure the file
/// is skipped with a notice: a missing fact beats a plausible one read from the wrong source.
/// Invalid UTF-8 is replaced, as for a blob, since it doesn't move line breaks.
pub(crate) fn read_new_side(root: &str, new: &Side, side: SourceSide) -> Option<String> {
    match side {
        SourceSide::Worktree => read_working_file(root, new),
        SourceSide::Blob => read_blob(new, "new"),
    }
}

/// Read a working file only if it is the content the diff's new side describes: the source
/// side is inferred from the arguments, and even when that is right, reading the path can
/// reach other content (a symlink's target, where the diff shows the link). The bytes read are
/// the bytes hashed, with git's filters for the path applied as they were for the diff's blob,
/// so a save in between can't slip unchecked content into the parse.
fn read_working_file(root: &str, new: &Side) -> Option<String> {
    let skip = |why: &str| {
        eprintln!("chsig: {} in the working tree {why}; skipping", new.path);
        None
    };
    let bytes = match std::fs::read(format!("{root}/{}", new.path)) {
        Ok(bytes) => bytes,
        Err(e) => return skip(&format!("could not be read ({e})")),
    };
    match hash_with_filters(root, &new.path, &bytes) {
        None => skip("could not be hashed"),
        Some(sha) if !sha.starts_with(&new.blob) => skip("is not the diff's new side"),
        Some(_) => Some(String::from_utf8_lossy(&bytes).into_owned()),
    }
}

fn hash_with_filters(root: &str, path: &str, bytes: &[u8]) -> Option<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin", &format!("--path={path}")])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // hash-object reads all of stdin before writing its one line, so writing first can't
    // deadlock on a full output pipe.
    child.stdin.take()?.write_all(bytes).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn read_blob(side: &Side, label: &str) -> Option<String> {
    let read = Command::new("git")
        .args(["cat-file", "-p", &side.blob])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    if read.is_none() {
        // A blob the diff named couldn't be read — surface the skip, don't hide it.
        eprintln!(
            "chsig: could not read {label}-side blob {} for {}; skipping",
            side.blob, side.path
        );
    }
    read
}

pub(crate) fn read_old_side(old: &Side) -> Option<String> {
    read_blob(old, "old")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_side_classifies_supported_forms() {
        let s = |a: &[&str]| source_side(&a.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(matches!(s(&[]), SourceSide::Worktree)); // plain working diff
        assert!(matches!(s(&["HEAD"]), SourceSide::Worktree)); // `git diff HEAD` = tree vs HEAD
        assert!(matches!(s(&["--cached"]), SourceSide::Blob));
        assert!(matches!(s(&["main...HEAD"]), SourceSide::Blob));
        assert!(matches!(s(&["HEAD~1", "HEAD"]), SourceSide::Blob));
        assert!(matches!(s(&["HEAD^!"]), SourceSide::Blob)); // commit vs its parent
        assert!(matches!(s(&["HEAD^-"]), SourceSide::Blob));
        assert!(matches!(s(&["HEAD^-2"]), SourceSide::Blob));
        assert!(matches!(s(&["--", "src/foo.ts"]), SourceSide::Worktree)); // pathspec, not a rev
        assert!(matches!(s(&["-G[^-]x"]), SourceSide::Worktree)); // option value, not a range
        assert!(matches!(
            s(&["--word-diff-regex=a..b"]),
            SourceSide::Worktree
        ));
    }
}
