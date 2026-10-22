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

// ---- tunables tied to the target codebase ----------------------------------
//
// Everything here encodes assumptions about the repo chsig is pointed at — its stack
// (react-query, SQL) and its shape (React hooks nest one level deep). This is the one
// project-specific seam; the rest of the file is general. Edit these to retarget.
//
// Kept as plain consts, not an external config file: with a single consumer there is no
// second data point to design a schema from. Promote to real config (env/flag, then a
// file) when a second repo actually needs different values — that is when the varying
// axis becomes known. Until then this block *is* the example.

/// Max brace depth at which a line still counts as a symbol definition. 0 = top level,
/// 1 = a method or hook-level binding (`const f = useCallback(...)`); deeper locals are
/// dropped. Tuned to one-level React hook nesting.
const MAX_DEF_DEPTH: i32 = 1;

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

// ---- diff parsing ----------------------------------------------------------

struct ChangedFile {
    path: String,
    /// New-side blob SHA from the `index old..new` header (all-zeros filtered to `None`).
    /// Used only when the new side is read as a `Blob` (`--cached`, ranges). A plain working
    /// diff also carries a non-zero SHA here, but it names unstaged content that isn't a
    /// stored object, so it is ignored and that side is read from the working file instead.
    new_blob: Option<String>,
    /// Changed lines as (new-side line, sign, body-without-+/-). Sign is +1 for additions,
    /// -1 for removals, so a removed branch token subtracts rather than adds.
    changed_bodies: Vec<(usize, i32, String)>,
}

fn parse_diff(diff: &str) -> Vec<ChangedFile> {
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

// ---- symbol resolution + signal aggregation --------------------------------

struct SymbolSignals {
    name: String,
    signals: Signals,
}

#[derive(Default)]
struct Signals {
    /// `None` = refs could not be measured (shown as `refs=?`), distinct from `Some(0)`.
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
    if !parsed.balanced {
        eprintln!(
            "chsig: unbalanced braces in {}; symbol ranges may be off",
            file.path
        );
    }
    let mut symbols = attribute_signals(&parsed.defs, file);
    for sym in &mut symbols {
        sym.signals.refs = ref_line_count(root, &sym.name);
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
/// A changed line goes to the *innermost* def whose `[start, end]` body contains it (greatest
/// start among containing defs). A line outside every handler body but inside the component
/// thus lands on the component, not on the last handler above it. refs is left `None` for the
/// caller to measure.
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
            signals,
        })
        .collect()
}

/// A definition with its body span. Start alone is not enough: to attribute a changed line
/// we must know where the body *ends*, or trailing code (a component's JSX return, module
/// tail) wrongly folds into the last preceding def even when that def never changed.
struct SymbolDef {
    name: String,
    start: usize, // 1-based def line
    end: usize,   // 1-based line where the body closes (== start for a one-liner)
    depth: i32,   // brace depth at the start of the def line
}

struct SymbolDefs {
    defs: Vec<SymbolDef>,
    /// False when depth went below 0 or didn't return to 0 at EOF — spans may then be off.
    /// A miscount that cancels out (a stray `{` and `}`) still passes as balanced.
    balanced: bool,
}

/// Definitions with body spans, ascending by start, for the file's language.
///
/// Only Rust and TS/JS are parsed; any other extension returns empty rather than being
/// run through a parser that doesn't fit it (don't treat unsupported files as supported).
/// Tracks brace nesting: a line is a def only if its depth *at the start of the line*
/// (before this line's braces) is <= MAX_DEF_DEPTH (0 = top level, 1 = a method / hook-level
/// binding; deeper local bindings like `const outcome = ...` are dropped). The same depth
/// counter gives each def's `end` — the line where depth returns to the def's level — so a
/// change outside a def's body attributes to the enclosing def, not to this one. Brace
/// counting is off inside strings/comments (accepted noise, surfaced via `balanced`); no
/// lexer, no tree-sitter — a line-local strip of literals was measured to break more files
/// than it fixed.
fn find_symbol_defs(path: &str, source: &str) -> SymbolDefs {
    let def: fn(&str) -> Option<String> = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => rust_def,
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => ts_def,
        _ => {
            return SymbolDefs {
                defs: Vec::new(),
                balanced: true,
            };
        }
    };
    let mut balanced = true;
    let mut finalized: Vec<SymbolDef> = Vec::new();
    let mut open: Vec<SymbolDef> = Vec::new(); // defs whose body has not closed yet
    let mut depth: i32 = 0;
    for (i, line) in source.lines().enumerate() {
        let line_no = i + 1;
        if depth <= MAX_DEF_DEPTH
            && let Some(name) = def(line)
        {
            open.push(SymbolDef {
                name,
                start: line_no,
                end: line_no,
                depth,
            });
        }
        depth += brace_delta(line);
        if depth < 0 {
            balanced = false;
            depth = 0;
        }
        // A def closes once depth returns to (or below) its own level. A one-liner closes on
        // its own line; a brace def closes at the matching `}`.
        while let Some(mut done) = open.pop_if(|top| depth <= top.depth) {
            done.end = line_no;
            finalized.push(done);
        }
    }
    // Unbalanced braces (EOF inside a body): close the rest at the last line.
    if depth != 0 {
        balanced = false;
    }
    let last = source.lines().count();
    for mut d in open.drain(..) {
        d.end = last;
        finalized.push(d);
    }
    finalized.sort_by_key(|d| d.start);
    SymbolDefs {
        defs: finalized,
        balanced,
    }
}

fn brace_delta(line: &str) -> i32 {
    line.matches('{').count() as i32 - line.matches('}').count() as i32
}

fn rust_def(line: &str) -> Option<String> {
    let mut t = line.trim_start();
    for p in ["pub(crate) ", "pub ", "async ", "const ", "unsafe "] {
        t = t.strip_prefix(p).unwrap_or(t);
    }
    let rest = t.strip_prefix("fn ")?;
    let name = take_ident(rest);
    (!name.is_empty()).then_some(name)
}

fn ts_def(line: &str) -> Option<String> {
    let mut t = line.trim_start();
    for p in ["export ", "default "] {
        t = t.strip_prefix(p).unwrap_or(t);
    }
    for p in ["async function ", "function "] {
        if let Some(rest) = t.strip_prefix(p) {
            let name = take_ident(rest);
            return (!name.is_empty()).then_some(name);
        }
    }
    // Treat `const/let NAME = <function-ish>` as a symbol; skip scalar constants.
    for p in ["const ", "let ", "var "] {
        if let Some(rest) = t.strip_prefix(p) {
            let name = take_ident(rest);
            if name.is_empty() {
                continue;
            }
            let after = rest[name.len()..].trim_start();
            // A type annotation may sit between the name and the assignment:
            //   const Foo: React.FC<Props> = (...) => {
            // Skip it to the ` = ` without parsing the type (no generics handling — if that
            // ever feels needed, it's the sign to stop, not to build a type parser).
            let rhs = if let Some(rhs) = after.strip_prefix('=') {
                rhs
            } else if after.starts_with(':') {
                match after.split_once(" = ") {
                    Some((_, rhs)) => rhs,
                    None => continue,
                }
            } else {
                continue;
            };
            let rhs = rhs.trim_start();
            let looks_fn = rhs.contains("=>")
                || rhs.starts_with("function")
                || rhs.starts_with("async")
                || rhs.ends_with('(')
                || rhs.ends_with('{');
            if looks_fn {
                return Some(name);
            }
        }
    }
    None
}

fn take_ident(s: &str) -> String {
    s.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
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

    // Tests pin *current* heuristic behavior as a regression anchor — including known
    // over-captures — so later tuning can't silently drift. I/O fns (git_*,
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

    fn names(path: &str, src: &str) -> Vec<String> {
        find_symbol_defs(path, src)
            .defs
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    // A change in a component's JSX return, below a prior handler's body, must attribute to the
    // component — not to the handler that is merely the last def above it. Without body-span
    // tracking the handler absorbed branch deltas from unrelated JSX it never contained.
    #[test]
    fn change_outside_handler_body_attributes_to_enclosing_component() {
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
        // Line 7 is `condition && <Child />`, inside the component return, outside the handler.
        let file = ChangedFile {
            path: "c.tsx".into(),
            new_blob: None,
            changed_bodies: vec![(7, 1, "    condition && <Child />".into())],
        };
        let out = attribute(source, "c.tsx", &file);
        assert_eq!(out, vec![("Component".to_string(), 1, vec![])]);
        assert!(!out.iter().any(|(n, _, _)| n == "handleClick"));
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

    // KNOWN DEFECT, pinned as current behavior — not endorsed. `brace_delta` counts a `}` inside a
    // string literal, so the `}` on line 2 closes `alpha` early; the real change on line 3 then
    // falls outside every def and is dropped silently instead of pointing at alpha. A fix should
    // flip this to `[("alpha", 1, [])]`.
    #[test]
    fn stray_brace_in_string_closes_symbol_early_and_drops_change() {
        let source = "\
fn alpha() {
    let s = \"}\";
    if p && q {}
}
fn beta() {
    let x = 1;
}
";
        // Span check documents the early close: alpha's body is truncated at line 2.
        let defs = find_symbol_defs("a.rs", source).defs;
        let alpha = defs.iter().find(|d| d.name == "alpha").unwrap();
        assert_eq!((alpha.start, alpha.end), (1, 2)); // true body is lines 1..=4

        // The real change on line 3 is inside alpha's *true* body but outside the truncated span.
        let file = ChangedFile {
            path: "a.rs".into(),
            new_blob: None,
            changed_bodies: vec![(3, 1, "    if p && q {}".into())],
        };
        let out = attribute(source, "a.rs", &file);
        assert!(out.is_empty()); // DEFECT: silently dropped; should be [("alpha", 1, [])]
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

    #[test]
    fn brace_depth_drops_deeper_locals_keeps_hook_level() {
        // Depth 0 (top level) and 1 (hook-level binding) are symbols; depth 2+ is not.
        let src = "\
export const useThing = () => {
  const handleClick = useCallback((x) => {
    const local = doThing((y) => y)
    return local
  }, [])
  return handleClick
}
export const makeRequest = () => new Req()
";
        assert_eq!(
            names("hook.ts", src),
            vec!["useThing", "handleClick", "makeRequest"],
        );
    }

    // Both miscount directions are flagged: a `{` in a comment leaves depth open at EOF, a
    // `}` in a string drives it below 0. Literal braces that cancel out are not detectable.
    #[test]
    fn balance_flags_both_miscount_directions() {
        let b = |src: &str| find_symbol_defs("a.rs", src).balanced;
        assert!(b("fn a() {\n    let x = 1;\n}\n"));
        assert!(!b("fn a() {\n    // {\n}\n"));
        assert!(!b("fn a() {\n    let s = \"}\";\n}\n"));
        assert!(b("fn a() {\n    let s = \"{}\";\n}\n"));
    }

    #[test]
    fn unsupported_language_has_no_symbols() {
        // A .go file must not be run through the TS/JS parser (would accidentally match).
        let src = "var f = func() {";
        assert!(find_symbol_defs("main.go", src).defs.is_empty());
    }

    #[test]
    fn rust_defs_and_impl_methods() {
        let src = "\
pub fn alpha() {
}
impl Foo {
    pub async fn method(&self) {}
}
";
        // `impl Foo` is not a symbol; the top-level fn and the depth-1 method are.
        assert_eq!(names("x.rs", src), vec!["alpha", "method"]);
    }

    #[test]
    fn rust_def_strips_qualifiers_and_rejects_non_fn() {
        assert_eq!(rust_def("pub async fn foo() {"), Some("foo".into()));
        assert_eq!(rust_def("    let x = 5;"), None);
        assert_eq!(rust_def("struct S {"), None);
    }

    #[test]
    fn ts_def_accepts_functions_skips_scalars() {
        assert_eq!(ts_def("export function foo() {"), Some("foo".into()));
        assert_eq!(ts_def("const bar = () => {"), Some("bar".into()));
        assert_eq!(
            ts_def("export default function baz() {"),
            Some("baz".into())
        );
        assert_eq!(ts_def("const n = 5"), None);
        // A type annotation between the name and `=` must not hide a typed FC component.
        assert_eq!(
            ts_def("export const Foo: React.FC<Props> = ({ x }) => {"),
            Some("Foo".into())
        );
        // Known over-capture: `= someCall(` is indistinguishable from a function const
        // without type info. Locked as current behavior, not endorsed.
        assert_eq!(ts_def("const q = useQuery("), Some("q".into()));
    }

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

    // Same-named defs in one file (`new` in two impls) are separate symbols; each keeps its own
    // signals instead of being summed under one name.
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
                ("new".to_string(), 1, vec![]),
                ("new".to_string(), 2, vec![])
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
