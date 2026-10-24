//! Integration tests: run the built `chsig` against a real temp git repo. They exercise
//! source selection — working tree vs staged blob — which the pure unit tests leave out and
//! which directly decides whether a signal is attributed to the right symbol.

use std::path::{Path, PathBuf};
use std::process::Command;

// The developer's global/system git config and locale must not leak into what these tests
// observe; config under test is set per repo instead.
fn isolated(cmd: &mut Command) -> &mut Command {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
}

fn git(dir: &Path, args: &[&str]) {
    let out = isolated(&mut Command::new("git"))
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

fn chsig_full(dir: &Path, args: &[&str]) -> (String, String) {
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_chsig")))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn chsig");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn chsig(dir: &Path, args: &[&str]) -> String {
    chsig_full(dir, args).0
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

const ALPHA_BETA: &str = "fn alpha() {\n    let x = 1;\n}\nfn beta() {\n    let y = 2;\n}\n";
// ALPHA_BETA with a branch added inside `alpha`.
const ALPHA_CHANGED: &str =
    "fn alpha() {\n    let x = 1;\n    if x > 0 {}\n}\nfn beta() {\n    let y = 2;\n}\n";

/// Repo with `rel` committed as ALPHA_BETA, then edited (unstaged) to ALPHA_CHANGED.
fn repo_with_alpha_edit(tag: &str, rel: &str) -> PathBuf {
    let dir = temp_repo(tag);
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, ALPHA_BETA).unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    std::fs::write(&path, ALPHA_CHANGED).unwrap();
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

// Each of these user settings changes `git diff` output shape; before chsig pinned its own
// flags, every one of them made it print nothing and exit 0.
#[test]
fn user_diff_config_does_not_change_the_result() {
    for (i, (key, val)) in [
        ("diff.mnemonicPrefix", "true"),
        ("diff.noprefix", "true"),
        ("color.ui", "always"),
        ("diff.relative", "true"),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = repo_with_alpha_edit(&format!("cfg{i}"), "sub/a.rs");
        git(&dir, &["config", key, val]);
        // From a subdirectory so diff.relative actually has something to relativize.
        let out = chsig(&dir.join("sub"), &[]);
        assert!(
            out.contains("sub/a.rs") && out.contains("alpha"),
            "with {key}={val}, expected sub/a.rs / alpha in:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// A space makes git append a tab to the `+++` header; non-ASCII is octal-quoted by default.
// Both used to make the file vanish from the output.
#[test]
fn paths_with_space_and_non_ascii_are_read() {
    for (tag, rel) in [("space", "sp ace.rs"), ("nonascii", "日本/j.rs")] {
        let dir = repo_with_alpha_edit(tag, rel);
        let out = chsig(&dir, &[]);
        assert!(
            out.contains(rel) && out.contains("alpha"),
            "expected {rel} / alpha in:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// Paths git still C-quotes (here: a `"` in the name) are not parsed; the skip must be visible.
#[test]
fn quoted_path_skip_is_reported() {
    let dir = repo_with_alpha_edit("quoted", "q\"x.rs");
    let (_, err) = chsig_full(&dir, &[]);
    assert!(
        err.contains("skipping quoted path"),
        "expected a skip notice on stderr, got:\n{err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// refs counts the whole repo, not just below the invocation directory.
#[test]
fn refs_are_repo_wide_from_a_subdirectory() {
    let dir = repo_with_alpha_edit("subdir", "a.rs");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/u.rs"), "fn other() {}\n").unwrap();
    git(&dir, &["add", "sub/u.rs"]);

    let at_root = chsig(&dir, &[]);
    let in_sub = chsig(&dir.join("sub"), &[]);
    assert!(at_root.contains("refs=1"), "expected refs=1 in:\n{at_root}");
    assert_eq!(in_sub, at_root);
    let _ = std::fs::remove_dir_all(&dir);
}

// `<rev>^!` is a range (commit vs parent): the new side is the commit's blob. Reading the
// worktree — here shifted by three lines — attributed the change to the wrong symbol.
#[test]
fn caret_bang_range_reads_the_commit_not_the_worktree() {
    let dir = temp_repo("caretbang");
    std::fs::write(dir.join("a.rs"), ALPHA_BETA).unwrap();
    git(&dir, &["add", "a.rs"]);
    git(&dir, &["commit", "-qm", "init"]);
    let beta_changed =
        "fn alpha() {\n    let x = 1;\n}\nfn beta() {\n    let y = 2;\n    if y > 0 {}\n}\n";
    std::fs::write(dir.join("a.rs"), beta_changed).unwrap();
    git(&dir, &["commit", "-qam", "beta"]);
    std::fs::write(
        dir.join("a.rs"),
        format!("// 1\n// 2\n// 3\n{beta_changed}"),
    )
    .unwrap();

    let out = chsig(&dir, &["HEAD^!"]);
    assert!(out.contains("beta"), "expected beta in:\n{out}");
    assert!(!out.contains("alpha"), "alpha was not changed:\n{out}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_git_is_not_reported_as_not_a_repository() {
    let dir = temp_repo("nogit");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_chsig")))
        .env("PATH", "")
        .current_dir(&dir)
        .output()
        .expect("spawn chsig");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("failed to run git"), "stderr was:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn outside_a_repository_reports_gits_own_message() {
    let dir = std::env::temp_dir().join(format!("chsig-it-{}-norepo", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_chsig")))
        .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
        .current_dir(&dir)
        .output()
        .expect("spawn chsig");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("fatal: "), "stderr was:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --exit-code makes git diff exit 1 on a non-empty diff with nothing on stderr; chsig must
// not fail silently.
#[test]
fn a_silent_git_failure_is_reported() {
    let dir = repo_with_alpha_edit("exitcode", "a.rs");
    let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_chsig")))
        .arg("--exit-code")
        .current_dir(&dir)
        .output()
        .expect("spawn chsig");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chsig: git failed"), "stderr was:\n{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

// --relative would make every path miss the root-based read; it must fail loudly, not print
// an empty result.
#[test]
fn relative_is_refused_not_silently_empty() {
    let dir = repo_with_alpha_edit("relative", "sub/a.rs");
    for arg in ["--relative", "--relative=sub"] {
        let out = isolated(&mut Command::new(env!("CARGO_BIN_EXE_chsig")))
            .arg(arg)
            .current_dir(dir.join("sub"))
            .output()
            .expect("spawn chsig");
        assert!(!out.status.success(), "{arg} should fail");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("not supported"), "{arg}: stderr was:\n{err}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// Braces in a comment or string used to shift symbol ranges under brace counting; parsed,
// they neither warn nor move the change off `beta`.
#[test]
fn braces_in_comments_and_strings_are_not_miscounted() {
    for (tag, stray) in [("open", "    // {\n"), ("neg", "    let s = \"}\";\n")] {
        let dir = temp_repo(&format!("stray-{tag}"));
        let base =
            format!("fn alpha() {{\n{stray}    let x = 1;\n}}\nfn beta() {{\n    let y = 2;\n}}\n");
        std::fs::write(dir.join("a.rs"), &base).unwrap();
        git(&dir, &["add", "a.rs"]);
        git(&dir, &["commit", "-qm", "init"]);
        std::fs::write(
            dir.join("a.rs"),
            base.replace("let y = 2;", "let y = 2;\n    if y > 0 {}"),
        )
        .unwrap();

        let (out, err) = chsig_full(&dir, &[]);
        assert_eq!(
            out, "a.rs\n  beta                         refs=1, branch+1\n\n",
            "{tag}"
        );
        assert_eq!(err, "", "{tag}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// A file the parser can't fully read is reported once on stderr; stdout keeps its shape.
#[test]
fn syntax_errors_warn_on_stderr_only() {
    let dir = repo_with_alpha_edit("syntax", "a.rs");
    std::fs::write(dir.join("a.rs"), format!("{ALPHA_CHANGED}fn broken( {{\n")).unwrap();
    let (out, err) = chsig_full(&dir, &[]);
    assert!(out.starts_with("a.rs\n"), "stdout was:\n{out}");
    assert_eq!(
        err,
        "chsig: parse errors in a.rs; symbol ranges may be off\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn clean_file_has_no_warning() {
    let dir = repo_with_alpha_edit("balanced", "a.rs");
    let (out, err) = chsig_full(&dir, &[]);
    assert!(out.contains("alpha"), "expected alpha in:\n{out}");
    assert_eq!(err, "");
    let _ = std::fs::remove_dir_all(&dir);
}

// A deleted function is read from the old side (here the index, for a working diff) and shown
// as deleted; its removed lines must not land on the unchanged function below it.
#[test]
fn a_deleted_function_is_shown_deleted() {
    let dir = repo_with_alpha_edit("deleted-fn", "a.rs");
    std::fs::write(dir.join("a.rs"), "fn beta() {\n    let y = 2;\n}\n").unwrap();
    let (out, err) = chsig_full(&dir, &[]);
    assert_eq!(
        out,
        "a.rs\n  alpha                        refs=0, deleted\n\n"
    );
    assert_eq!(err, "");
    let _ = std::fs::remove_dir_all(&dir);
}

// A file deleted in a commit has no new side; its symbols come from the old blob.
#[test]
fn a_deleted_file_shows_its_symbols_deleted() {
    let dir = repo_with_alpha_edit("deleted-file", "a.rs");
    git(&dir, &["checkout", "-q", "--", "a.rs"]);
    git(&dir, &["rm", "-q", "a.rs"]);
    git(&dir, &["commit", "-qm", "rm"]);
    let out = chsig(&dir, &["HEAD^!"]);
    assert_eq!(
        out,
        "a.rs\n  alpha                        refs=0, deleted\n  beta                         refs=0, deleted\n\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// Callees on changed lines follow the symbol's row, marked with the side they changed on.
#[test]
fn changed_calls_follow_the_symbol_row() {
    let dir = temp_repo("calls");
    std::fs::write(dir.join("a.rs"), "fn alpha() {\n    db.query(q);\n}\n").unwrap();
    git(&dir, &["add", "a.rs"]);
    git(&dir, &["commit", "-qm", "init"]);
    std::fs::write(dir.join("a.rs"), "fn alpha() {\n    tx.update(q);\n}\n").unwrap();
    let out = chsig(&dir, &[]);
    assert_eq!(
        out,
        "a.rs\n  alpha                        refs=1\n      calls: -db.query, +tx.update\n\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
