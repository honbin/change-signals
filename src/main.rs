//! `chsig` — collect factual signals about the symbols a `git diff` changed, so a
//! human can decide *where to read first*.
//!
//! First stage of `signals -> prioritize -> interrogate`. Its one job: gather facts,
//! not judgments. `refs=1` is a fact; "safe to skip" is a judgment — this stage never
//! crosses that line (no skip verdict). So it does not rank by risk; it prints per file
//! in diff order. Ranking is the `prioritize` stage (LLM); drilling is `interrogate` — both
//! downstream of this tool.

use std::io::{self, Write};
use std::process::Command;

use diff::{ChangedFile, parse_diff};
use symbols::{SymbolDef, find_symbol_defs};

mod diff;
mod symbols;

// ---- tunables tied to the target codebase ----------------------------------
//
// Everything here encodes assumptions about the repo chsig is pointed at — its stack
// (react-query, SQL). This is the one project-specific seam; the rest of the file is
// general. Edit these to retarget.
//
// Kept as plain consts, not an external config file: with a single consumer there is no
// second data point to design a schema from. Promote to real config (env/flag, then a
// file) when a second repo actually needs different values — that is when the varying
// axis becomes known. Until then this block *is* the example.

/// Boundary vocabulary: substrings that tag a changed line as touching a known boundary.
/// A marker, not a risk score. "query" is deliberately absent — on the FE it fires on
/// react-query (useQuery/queryKey), not a database.
const DB_VOCAB: &[&str] = &["sql", "SELECT", "INSERT", "UPDATE", "DELETE"];
const CACHE_VOCAB: &[&str] = &["queryKey", "invalidate", "setQueryData", "cache"];
const NET_VOCAB: &[&str] = &["fetch(", "http", "axios"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let root = git_root();

    // Args pass straight to `git diff` (e.g. `chsig main...HEAD`); none = working diff.
    // --relative would override the pinned --no-relative and make every path miss the
    // root-based read, emptying the output; refuse it instead.
    if let Some(a) = args
        .iter()
        .take_while(|a| *a != "--")
        .find(|a| *a == "--relative" || a.starts_with("--relative="))
    {
        eprintln!("chsig: {a} is not supported (paths are read from the repo root)");
        std::process::exit(1);
    }
    let side = source_side(&args);
    let diff = git_diff(&args);
    if diff.trim().is_empty() {
        eprintln!("chsig: no diff");
        return;
    }

    let files = parse_diff(&diff);
    // Handle write errors explicitly: BrokenPipe is expected when the reader exits early.
    let mut out = io::stdout().lock();
    for file in &files {
        let symbols = collect_symbols(&root, file, side);
        if symbols.is_empty() {
            continue;
        }
        if let Err(e) = write_file_signals(&mut out, file, &symbols) {
            exit_on_write_error(e);
        }
    }
    if let Err(e) = out.flush() {
        exit_on_write_error(e);
    }
}

fn write_file_signals(
    out: &mut impl Write,
    file: &ChangedFile,
    symbols: &[SymbolSignals],
) -> io::Result<()> {
    writeln!(out, "{}", file.path)?;
    for sym in symbols {
        writeln!(out, "  {:<28} {}", sym.name, sym.signals.render())?;
    }
    writeln!(out)
}

/// BrokenPipe means the reader went away (`chsig | head`) — exit quietly. Any other write error
/// (disk full, etc.) is real: report it and fail, never swallow it into a success.
fn exit_on_write_error(e: io::Error) -> ! {
    if e.kind() == io::ErrorKind::BrokenPipe {
        std::process::exit(0);
    }
    eprintln!("chsig: error writing output: {e}");
    std::process::exit(1);
}

// ---- git -------------------------------------------------------------------

fn git_root() -> String {
    git_stdout_or_exit(Command::new("git").args(["rev-parse", "--show-toplevel"]))
        .trim()
        .to_string()
}

fn git_diff(args: &[String]) -> String {
    // Pin the output shape parse_diff relies on against user config (color, mnemonic or no
    // prefix, external diff, relative paths, quoted non-ASCII paths). User args come after,
    // so an explicit flag still wins.
    git_stdout_or_exit(
        Command::new("git")
            .args(["-c", "core.quotePath=false", "diff"])
            .args([
                "--no-color",
                "--no-ext-diff",
                "--no-relative",
                "--src-prefix=a/",
                "--dst-prefix=b/",
            ])
            .args(args),
    )
}

/// Exit on failure with git's own stderr: it names the cause (outside a repository, dubious
/// ownership, a bad revision that would otherwise look like an empty diff).
fn git_stdout_or_exit(cmd: &mut Command) -> String {
    let out = match cmd.output() {
        Ok(out) => out,
        Err(e) => {
            eprintln!("chsig: failed to run git: {e}");
            std::process::exit(1);
        }
    };
    if !out.status.success() {
        // User args can make git diff exit non-zero with nothing on stderr (--exit-code,
        // --quiet, --check); say so rather than exit silently.
        if out.stderr.is_empty() {
            eprintln!("chsig: git failed ({})", out.status);
        } else {
            eprint!("{}", String::from_utf8_lossy(&out.stderr));
        }
        std::process::exit(1);
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Count lines matching `name` across the repo's **tracked files in the current working tree** —
/// not the diff's revision, so with a commit range refs is a present-day figure while the
/// changed symbols come from the range (git grep's default omits untracked files). Not
/// resolved references, just matching lines (a crude fact that counts strings and comments
/// too), left visible rather than hidden.
///
/// `None` = could not be measured (git grep failed, or its output didn't parse), kept
/// distinct from a measured zero so a non-fact is never reported as `refs=0`.
fn ref_line_count(root: &str, name: &str) -> Option<usize> {
    // git grep searches only below cwd; run from the root so a subdirectory invocation
    // doesn't report a low count as if it were the whole repo.
    let out = Command::new("git")
        .args(["grep", "--no-color", "-Fw", "-c", "--", name])
        .current_dir(root)
        .output()
        .ok()?;
    // git grep exits 0 (matches) or 1 (no matches) — both real measurements; >=2 is an error.
    if !matches!(out.status.code(), Some(0) | Some(1)) {
        return None;
    }
    // Output is `path:count` lines; sum the counts. Any line that doesn't parse -> None, so
    // a garbled result becomes refs=? rather than a silently-low number.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut total = 0usize;
    for line in stdout.lines() {
        let (_, count) = line.rsplit_once(':')?;
        total = total.checked_add(count.trim().parse::<usize>().ok()?)?;
    }
    Some(total)
}

// ---- signal aggregation --------------------------------------------------

struct SymbolSignals {
    name: String,
    /// The bare identifier `refs` greps for; `name` may carry an owner (`Foo::new`).
    ident: Option<String>,
    signals: Signals,
}

#[derive(Default)]
struct Signals {
    /// `None` = refs could not be measured, or the symbol has no name to grep for (an anonymous
    /// `export default`); shown as `refs=?`, distinct from `Some(0)`.
    refs: Option<usize>,
    branch_delta: i32,
    boundaries: Vec<&'static str>,
}

impl Signals {
    fn render(&self) -> String {
        let refs = match self.refs {
            Some(n) => format!("refs={n}"),
            None => "refs=?".to_string(),
        };
        let mut parts = vec![refs];
        if self.branch_delta != 0 {
            let sign = if self.branch_delta > 0 { "+" } else { "" };
            parts.push(format!("branch{}{}", sign, self.branch_delta));
        }
        for b in &self.boundaries {
            parts.push((*b).to_string());
        }
        parts.join(", ")
    }
}

/// Read the new-side source and attribute its changed lines to the enclosing symbols, then
/// attach `refs`. The attribution itself is pure (`attribute_signals`); only the source read
/// and the git-backed `refs` live here.
fn collect_symbols(root: &str, file: &ChangedFile, side: SourceSide) -> Vec<SymbolSignals> {
    let Some(source) = read_new_side(root, file, side) else {
        return Vec::new(); // deleted, binary, unreadable, or new side absent in the chosen source
    };
    let parsed = find_symbol_defs(&file.path, &source);
    if parsed.has_errors {
        eprintln!(
            "chsig: parse errors in {}; symbol ranges may be off",
            file.path
        );
    }
    let mut symbols = attribute_signals(&parsed.defs, file);
    for sym in &mut symbols {
        sym.signals.refs = sym.ident.as_deref().and_then(|i| ref_line_count(root, i));
    }
    symbols
}

/// Where the new side of the diff lives, decided once from the invocation. The two are never
/// mixed: reading the wrong source attributes changes against content whose line numbers
/// don't match the diff.
#[derive(Clone, Copy)]
enum SourceSide {
    /// Plain `git diff`, `git diff <rev>`, path-limited diff: the new side is the working file.
    Worktree,
    /// `--cached`/`--staged`, a `..`/`...`/`^!`/`^-` range, or two revisions: the new side is a
    /// stored blob.
    Blob,
}

/// Classify the supported git diff forms (the test fixes the exact contract).
/// Pathspecs should follow `--` to avoid ambiguity.
/// This is not a general-purpose git argument parser.
fn source_side(args: &[String]) -> SourceSide {
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
/// is skipped: a missing fact beats a plausible one read from the wrong source.
fn read_new_side(root: &str, file: &ChangedFile, side: SourceSide) -> Option<String> {
    match side {
        SourceSide::Worktree => std::fs::read_to_string(format!("{root}/{}", file.path)).ok(),
        SourceSide::Blob => {
            let blob = file.new_blob.as_ref()?; // no new side (e.g. deletion): expected skip
            let read = Command::new("git")
                .args(["cat-file", "-p", blob])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
            if read.is_none() {
                // Reading the blob failed though one was named — surface the skip, don't hide it.
                eprintln!(
                    "chsig: could not read new-side blob {blob} for {}; skipping",
                    file.path
                );
            }
            read
        }
    }
}

/// Attribute changed lines to enclosing symbols, in first-seen order. Pure (no git, no disk)
/// so the attribution — the part that just got the target wrong — is directly testable.
///
/// A changed line goes to the def whose `[start, end]` span contains it. Spans don't nest
/// (`find_symbol_defs` keeps only outermost defs) but can share a line (`fn a() {} fn b() {}`),
/// where the later def wins. refs is left `None` for the caller to measure.
///
/// Signals aggregate per def, not per name: two `fn new` in different impls of one file stay
/// two rows rather than summing into one.
fn attribute_signals(defs: &[SymbolDef], file: &ChangedFile) -> Vec<SymbolSignals> {
    let enclosing = |line: usize| -> Option<usize> {
        defs.iter()
            .enumerate()
            .filter(|(_, d)| d.start <= line && line <= d.end)
            .max_by_key(|(_, d)| d.start)
            .map(|(i, _)| i)
    };

    let mut touched: Vec<(usize, Signals)> = Vec::new();
    for (line, sign, body) in &file.changed_bodies {
        let Some(def) = enclosing(*line) else {
            continue;
        };
        let i = match touched.iter().position(|(d, _)| *d == def) {
            Some(i) => i,
            None => {
                touched.push((def, Signals::default()));
                touched.len() - 1
            }
        };
        let signals = &mut touched[i].1;
        signals.branch_delta += sign * branch_tokens(body);
        // A boundary is a presence marker: touching it in either direction tags the symbol.
        for b in boundary_tags(body) {
            if !signals.boundaries.contains(&b) {
                signals.boundaries.push(b);
            }
        }
    }

    touched
        .into_iter()
        .map(|(def, signals)| SymbolSignals {
            name: defs[def].name.clone(),
            ident: defs[def].ident.clone(),
            signals,
        })
        .collect()
}

/// Rough count of branch tokens on a changed line (a fact, not a measure). Matched with
/// trailing spaces / operators to avoid hitting identifiers; over- and under-counts a bit.
fn branch_tokens(body: &str) -> i32 {
    const TOKENS: [&str; 9] = [
        "if ", "else", "switch", "case ", "match ", " => ", "&&", "||", "?.",
    ];
    TOKENS
        .iter()
        .map(|tok| body.matches(tok).count() as i32)
        .sum()
}

/// Tag which recognizable boundaries a changed line touches — a marker ("this touches
/// db/cache/network"), not a risk score. Vocabulary lives in the tunables block above.
fn boundary_tags(body: &str) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if DB_VOCAB.iter().any(|k| body.contains(k)) {
        tags.push("db");
    }
    if CACHE_VOCAB.iter().any(|k| body.contains(k)) {
        tags.push("cache");
    }
    if NET_VOCAB.iter().any(|k| body.contains(k)) {
        tags.push("net");
    }
    tags
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests pin *current* behavior as a regression anchor — including known defects — so
    // later tuning can't silently drift. I/O fns (git_*,
    // ref_line_count, collect_symbols) are left to integration; not exercised here.

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

    fn attribute(
        source: &str,
        path: &str,
        file: &ChangedFile,
    ) -> Vec<(String, i32, Vec<&'static str>)> {
        attribute_signals(&find_symbol_defs(path, source).defs, file)
            .into_iter()
            .map(|s| (s.name, s.signals.branch_delta, s.signals.boundaries))
            .collect()
    }

    // A change in a component's JSX return, below a handler's body, must attribute to the
    // component. The handler is nested in it, so it is not a symbol of its own either.
    #[test]
    fn change_in_component_attributes_to_the_component_not_a_handler() {
        let source = "\
const Component = () => {
  const handleClick = () => {
    if (something) {}
  }

  return (
    condition && <Child />
  )
}
";
        let file = ChangedFile {
            path: "c.tsx".into(),
            new_blob: None,
            changed_bodies: vec![
                (3, 1, "    if (something) {}".into()),
                (7, 1, "    condition && <Child />".into()),
            ],
        };
        let out = attribute(source, "c.tsx", &file);
        assert_eq!(out, vec![("Component".to_string(), 2, vec![])]);
    }

    // A removed branch token subtracts: swapping `if true {}` for `if false {}` is net zero,
    // not +2. Additions and removals must carry opposite signs.
    #[test]
    fn removed_branch_tokens_subtract() {
        let source = "\
fn foo() {
    if x {}
}
";
        let file = ChangedFile {
            path: "x.rs".into(),
            new_blob: None,
            changed_bodies: vec![
                (2, 1, "    if y {}".into()),  // added branch
                (2, -1, "    if x {}".into()), // removed branch
            ],
        };
        let out = attribute(source, "x.rs", &file);
        assert_eq!(out, vec![("foo".to_string(), 0, vec![])]);
    }

    // A `}` inside a string used to close `alpha` early under brace counting, dropping the
    // change below it.
    #[test]
    fn brace_in_string_does_not_end_the_symbol() {
        let source = "\
fn alpha() {
    let s = \"}\";
    if p && q {}
}
fn beta() {
    let x = 1;
}
";
        let file = ChangedFile {
            path: "a.rs".into(),
            new_blob: None,
            changed_bodies: vec![(3, 1, "    if p && q {}".into())],
        };
        let out = attribute(source, "a.rs", &file);
        assert_eq!(out, vec![("alpha".to_string(), 2, vec![])]);
    }

    // KNOWN DEFECT, pinned as current behavior — not endorsed. A symbol deleted above `bar` has no
    // new-side line, so its removed lines attribute to the next new-side line — inside the
    // *unchanged* `bar`, which then shows a false `branch-1` while the deleted symbol shows nothing.
    #[test]
    fn pure_deletion_bleeds_onto_unchanged_following_symbol() {
        // New side: `fn foo` (which had `if y`) has been deleted; `bar` is untouched.
        let source = "\
fn bar() {
    let x = 1;
}
";
        // The three removed lines of `foo` all attribute to new_line 1 (start of `bar`).
        let file = ChangedFile {
            path: "b.rs".into(),
            new_blob: None,
            changed_bodies: vec![
                (1, -1, "fn foo() {".into()),
                (1, -1, "    if y {}".into()),
                (1, -1, "}".into()),
            ],
        };
        let out = attribute(source, "b.rs", &file);
        assert_eq!(out, vec![("bar".to_string(), -1, vec![])]); // DEFECT: bar was not changed
    }

    // Same-named defs in one file (`new` in two impls) are separate symbols; each keeps its own
    // signals instead of being summed under one name, and its owner tells them apart.
    #[test]
    fn same_name_defs_stay_separate_rows() {
        let source = "\
impl A {
    fn new() {
        if a {}
    }
}
impl B {
    fn new() {
        if b && c {}
    }
}
";
        let file = ChangedFile {
            path: "n.rs".into(),
            new_blob: None,
            changed_bodies: vec![
                (3, 1, "        if a {}".into()),
                (8, 1, "        if b && c {}".into()),
            ],
        };
        let out = attribute(source, "n.rs", &file);
        assert_eq!(
            out,
            vec![
                ("A::new".to_string(), 1, vec![]),
                ("B::new".to_string(), 2, vec![])
            ]
        );
    }

    #[test]
    fn branch_and_boundary_are_substring_facts() {
        assert_eq!(branch_tokens("if (a && b) {"), 2);
        assert_eq!(branch_tokens("const x = 5"), 0);
        assert_eq!(
            boundary_tags("queryClient.invalidateQueries()"),
            vec!["cache"]
        );
        assert_eq!(boundary_tags("await fetch(url)"), vec!["net"]);
        assert_eq!(boundary_tags("exec(\"SELECT 1\")"), vec!["db"]);
        // "query" is intentionally not a db marker (react-query false-positive).
        assert!(boundary_tags("const q = useQuery(").is_empty());
    }

    #[test]
    fn render_joins_present_signals_only() {
        let bare = Signals {
            refs: Some(3),
            branch_delta: 0,
            boundaries: vec![],
        };
        assert_eq!(bare.render(), "refs=3");
        let full = Signals {
            refs: Some(3),
            branch_delta: 2,
            boundaries: vec!["cache"],
        };
        assert_eq!(full.render(), "refs=3, branch+2, cache");
        let neg = Signals {
            refs: Some(1),
            branch_delta: -1,
            boundaries: vec![],
        };
        assert_eq!(neg.render(), "refs=1, branch-1");
        let unmeasured = Signals {
            refs: None,
            branch_delta: 0,
            boundaries: vec![],
        };
        assert_eq!(unmeasured.render(), "refs=?");
    }
}
