//! Integration tests: run the built `chsig` against a real temp git repo. They exercise
//! source selection — working tree vs staged blob — which the pure unit tests leave out and
//! which directly decides whether a signal is attributed to the right symbol.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn chsig(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_chsig"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn chsig");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn temp_repo(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("chsig-it-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t.test"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["config", "commit.gpgsign", "false"]);
    dir
}

#[test]
fn working_diff_attributes_against_the_working_file() {
    let dir = temp_repo("work");
    std::fs::write(dir.join("a.rs"), "fn alpha() {\n    let x = 1;\n}\n").unwrap();
    git(&dir, &["add", "a.rs"]);
    git(&dir, &["commit", "-qm", "init"]);
    // Unstaged edit — a plain `git diff` must read the working file (its new-side SHA is not a
    // stored object). If chsig tried to read a blob here it would skip and print nothing.
    std::fs::write(
        dir.join("a.rs"),
        "fn alpha() {\n    let x = 1;\n    if x > 0 {}\n}\n",
    )
    .unwrap();

    let out = chsig(&dir, &[]);
    assert!(out.contains("alpha"), "expected alpha in:\n{out}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cached_diff_attributes_against_the_staged_blob_not_the_worktree() {
    let dir = temp_repo("cached");
    std::fs::write(dir.join("b.rs"), "fn beta() {\n}\n").unwrap();
    git(&dir, &["add", "b.rs"]);
    git(&dir, &["commit", "-qm", "init"]);
    // Stage a version that adds `gamma`, then revert the worktree so staged != working.
    std::fs::write(
        dir.join("b.rs"),
        "fn gamma() {\n}\nfn beta() {\n    let y = 1;\n}\n",
    )
    .unwrap();
    git(&dir, &["add", "b.rs"]);
    std::fs::write(dir.join("b.rs"), "fn beta() {\n}\n").unwrap();

    let out = chsig(&dir, &["--cached"]);
    // `gamma` exists only in the staged blob; it appears only if the staged side was read
    // (reading the reverted worktree would attribute that line to `beta` instead).
    assert!(
        out.contains("gamma"),
        "expected gamma (staged side) in:\n{out}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
