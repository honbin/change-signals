//! `chsig` — collect factual signals about the symbols a `git diff` changed, so a
//! human can decide *where to read first*.
//!
//! First stage of `signals -> prioritize -> interrogate`. Its one job: gather facts,
//! not judgments. `refs=1` is a fact; "safe to skip" is a judgment — this stage never
//! crosses that line (no skip verdict). So it does not rank by risk; it prints per file
//! in diff order. Ranking is `prioritize` (v2 / LLM); drilling is `interrogate` (v3).

use std::collections::BTreeMap;
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

    let root = match git_root() {
        Some(r) => r,
        None => {
            eprintln!("chsig: not a git repository");
            std::process::exit(1);
        }
    };

    // Args pass straight to `git diff` (e.g. `chsig main...HEAD`); none = working diff.
    let side = source_side(&args);
    let diff = git_diff(&args);
    if diff.trim().is_empty() {
        eprintln!("chsig: no diff");
        return;
    }

    let files = parse_diff(&diff);
    for file in &files {
        let symbols = collect_symbols(&root, file, side);
        if symbols.is_empty() {
            continue;
        }
        println!("{}", file.path);
        for sym in &symbols {
            println!("  {:<28} {}", sym.name, sym.signals.render());
        }
        println!();
    }
}

// ---- git -------------------------------------------------------------------

fn git_root() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_diff(args: &[String]) -> String {
    let out = Command::new("git")
        .arg("diff")
        .args(args)
        .output()
        .expect("failed to run git diff");
    // A bad revision fails on stderr with empty stdout; without this it would look like an
    // empty diff and exit 0. Surface git's error and fail.
    if !out.status.success() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
        std::process::exit(1);
    }
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Count lines matching `name` across the **tracked files in the current working tree** —
/// not the diff's revision, so with a commit range refs is a present-day figure while the
/// changed symbols come from the range (git grep's default omits untracked files). Not
/// resolved references, just matching lines (a crude fact that counts strings and comments
/// too), left visible rather than hidden.
///
/// `None` = could not be measured (git grep failed, or its output didn't parse), kept
/// distinct from a measured zero so a non-fact is never reported as `refs=0`.
fn ref_line_count(name: &str) -> Option<usize> {
    let out = Command::new("git")
        .args(["grep", "-Fw", "-c", "--", name])
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

    for line in diff.lines() {
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
            current = Some(ChangedFile {
                path: path.to_string(),
                new_blob: pending_blob.take(),
                changed_bodies: Vec::new(),
            });
            continue;
        }
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -old,s +new,s @@ ... — take the new-side start line.
            new_line = parse_hunk_new_start(rest).unwrap_or(0);
            continue;
        }

        let Some(f) = current.as_mut() else { continue };
        match line.as_bytes().first() {
            Some(b'+') => {
                f.changed_bodies.push((new_line, 1, line[1..].to_string()));
                new_line += 1;
            }
            Some(b'-') => {
                // A removed line has no new-side position, so attribute it to where it was
                // removed (the next new line); its branch tokens subtract.
                f.changed_bodies.push((new_line, -1, line[1..].to_string()));
            }
            Some(b' ') => {
                new_line += 1;
            }
            _ => {} // blank lines, `\ No newline at end of file`, etc.
        }
    }
    if let Some(f) = current.take() {
        files.push(f);
    }
    files
}

fn parse_hunk_new_start(rest: &str) -> Option<usize> {
    // rest e.g. "-10,7 +12,9 @@ fn foo()"
    let plus = rest.split_whitespace().find(|t| t.starts_with('+'))?;
    let digits = &plus[1..];
    let num = digits.split(',').next()?;
    num.parse().ok()
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
    attribute_signals(&source, &file.path, file)
        .into_iter()
        .map(|(name, branch_delta, boundaries)| {
            let refs = ref_line_count(&name);
            SymbolSignals {
                name,
                signals: Signals {
                    refs,
                    branch_delta,
                    boundaries,
                },
            }
        })
        .collect()
}

/// Where the new side of the diff lives, decided once from the invocation. The two are never
/// mixed: reading the wrong source attributes changes against content whose line numbers
/// don't match the diff.
#[derive(Clone, Copy)]
enum SourceSide {
    /// Plain `git diff`, `git diff <rev>`, path-limited diff: the new side is the working file.
    Worktree,
    /// `--cached`/`--staged`, a `..`/`...` range, or two revisions: the new side is a stored blob.
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
        if a == "--cached" || a == "--staged" || a.contains("..") {
            return SourceSide::Blob;
        }
        if !a.starts_with('-') {
            revs += 1;
        }
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
/// thus lands on the component, not on the last handler above it. refs is added by the caller.
fn attribute_signals(
    source: &str,
    path: &str,
    file: &ChangedFile,
) -> Vec<(String, i32, Vec<&'static str>)> {
    let defs = find_symbol_defs(path, source);
    let enclosing = |line: usize| -> Option<&str> {
        defs.iter()
            .filter(|d| d.start <= line && line <= d.end)
            .max_by_key(|d| d.start)
            .map(|d| d.name.as_str())
    };

    let mut order: Vec<String> = Vec::new();
    let mut agg: BTreeMap<String, (i32, Vec<&'static str>)> = BTreeMap::new();
    for (line, sign, body) in &file.changed_bodies {
        let Some(name) = enclosing(*line) else {
            continue;
        };
        if !agg.contains_key(name) {
            order.push(name.to_string());
        }
        let entry = agg.entry(name.to_string()).or_default();
        entry.0 += sign * branch_tokens(body);
        // A boundary is a presence marker: touching it in either direction tags the symbol.
        for b in boundary_tags(body) {
            if !entry.1.contains(&b) {
                entry.1.push(b);
            }
        }
    }

    // Every changed line (including every `+`) is already in `order` from the loop above —
    // a symbol touched only by additions appears with branch 0 / no boundary — so nothing
    // more needs collecting here.
    order
        .into_iter()
        .map(|name| {
            let (branch, boundaries) = agg.remove(&name).unwrap_or_default();
            (name, branch, boundaries)
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

/// Definitions with body spans, ascending by start, for the file's language.
///
/// Only Rust and TS/JS are parsed; any other extension returns empty rather than being
/// run through a parser that doesn't fit it (don't treat unsupported files as supported).
/// Tracks brace nesting: a line is a def only if its depth *at the start of the line*
/// (before this line's braces) is <= MAX_DEF_DEPTH (0 = top level, 1 = a method / hook-level
/// binding; deeper local bindings like `const outcome = ...` are dropped). The same depth
/// counter gives each def's `end` — the line where depth returns to the def's level — so a
/// change outside a def's body attributes to the enclosing def, not to this one. Brace
/// counting is off inside strings/comments (accepted noise); no tree-sitter.
fn find_symbol_defs(path: &str, source: &str) -> Vec<SymbolDef> {
    let def: fn(&str) -> Option<String> = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => rust_def,
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => ts_def,
        _ => return Vec::new(),
    };
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
        depth = (depth + brace_delta(line)).max(0);
        // A def closes once depth returns to (or below) its own level. A one-liner closes on
        // its own line; a brace def closes at the matching `}`.
        while let Some(top) = open.last() {
            if depth <= top.depth {
                let mut done = open.pop().expect("checked by last()");
                done.end = line_no;
                finalized.push(done);
            } else {
                break;
            }
        }
    }
    // Unbalanced braces (EOF inside a body): close the rest at the last line.
    let last = source.lines().count();
    for mut d in open.drain(..) {
        d.end = last;
        finalized.push(d);
    }
    finalized.sort_by_key(|d| d.start);
    finalized
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
    // over-captures — so later tuning (v2) can't silently drift. I/O fns (git_*,
    // ref_line_count, collect_symbols) are left to integration; not exercised here.

    #[test]
    fn source_side_classifies_supported_forms() {
        let s = |a: &[&str]| source_side(&a.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert!(matches!(s(&[]), SourceSide::Worktree)); // plain working diff
        assert!(matches!(s(&["HEAD"]), SourceSide::Worktree)); // `git diff HEAD` = tree vs HEAD
        assert!(matches!(s(&["--cached"]), SourceSide::Blob));
        assert!(matches!(s(&["main...HEAD"]), SourceSide::Blob));
        assert!(matches!(s(&["HEAD~1", "HEAD"]), SourceSide::Blob));
        assert!(matches!(s(&["--", "src/foo.ts"]), SourceSide::Worktree)); // pathspec, not a rev
    }

    fn names(path: &str, src: &str) -> Vec<String> {
        find_symbol_defs(path, src)
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
        let out = attribute_signals(source, "c.tsx", &file);
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
        let out = attribute_signals(source, "x.rs", &file);
        assert_eq!(out, vec![("foo".to_string(), 0, vec![])]);
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

    #[test]
    fn unsupported_language_has_no_symbols() {
        // A .go file must not be run through the TS/JS parser (would accidentally match).
        let src = "var f = func() {";
        assert!(find_symbol_defs("main.go", src).is_empty());
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

    #[test]
    fn hunk_new_start_reads_plus_side() {
        assert_eq!(parse_hunk_new_start("-10,7 +12,9 @@ fn foo()"), Some(12));
        assert_eq!(parse_hunk_new_start("-1 +1 @@"), Some(1));
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
