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

use diff::{ChangedFile, ChangedLine, ParsedDiff, Side, Skipped, Unreadable, parse_diff};
use source::{SourceSide, read_new_side, read_old_side, source_side};
use symbols::{SymbolDef, SymbolDefs, find_symbol_defs};

mod diff;
mod source;
mod symbols;

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

    let ParsedDiff { files, skipped } = match parse_diff(&diff) {
        Ok(parsed) => parsed,
        Err(Unreadable(line)) => {
            eprintln!(
                "chsig: can't read git's diff output at {line:?}; pass revisions and paths, \
                 not options that change its format (--color, --word-diff, --stat, -R)"
            );
            std::process::exit(1);
        }
    };
    for skip in &skipped {
        match skip {
            Skipped::Quoted(path) => eprintln!("chsig: skipping quoted path {path}"),
            Skipped::Combined(path) => eprintln!(
                "chsig: skipping {path}: a combined diff (merge or conflict) is not read; \
                 diff against one parent, as in `<merge>^1 <merge>` or `HEAD`"
            ),
        }
    }
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
    writeln!(out, "{}", file.path())?;
    for sym in symbols {
        writeln!(out, "  {:<28} {}", sym.name, sym.signals.render())?;
        for line in wrap("      calls: ", &sym.signals.render_calls(), 100) {
            writeln!(out, "{line}")?;
        }
    }
    writeln!(out)
}

/// Lay `items` out after `prefix`, comma-separated, continuing on lines indented to the
/// prefix's width once a line would pass `width`. Nothing is dropped: the longest lists belong
/// to wholly new or rewritten functions, the ones whose calls matter most.
fn wrap(prefix: &str, items: &[String], width: usize) -> Vec<String> {
    let mut bodies: Vec<String> = Vec::new();
    let mut body = String::new();
    for (i, item) in items.iter().enumerate() {
        let sep = if i + 1 < items.len() { "," } else { "" };
        let piece = format!("{item}{sep}");
        if !body.is_empty() && prefix.len() + body.len() + 1 + piece.len() > width {
            bodies.push(std::mem::take(&mut body));
        }
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&piece);
    }
    if !body.is_empty() {
        bodies.push(body);
    }
    let indent = " ".repeat(prefix.len());
    bodies
        .into_iter()
        .enumerate()
        .map(|(i, b)| format!("{}{b}", if i == 0 { prefix } else { &indent }))
        .collect()
}

/// `line 12`, `lines 12, 40`, or the first three then `…`: where to look, not an inventory.
fn render_lines(lines: &[usize]) -> String {
    let shown: Vec<String> = lines.iter().take(3).map(usize::to_string).collect();
    let more = if lines.len() > 3 { ", …" } else { "" };
    let noun = if lines.len() == 1 { "line" } else { "lines" };
    format!("{noun} {}{more}", shown.join(", "))
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
    // prefix, external diff, textconv, relative paths, quoted non-ASCII paths). User args come
    // after, so an explicit flag still wins.
    git_stdout_or_exit(
        Command::new("git")
            .args(["-c", "core.quotePath=false", "diff"])
            .args([
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
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

// ---- signal aggregation ----------------------------------------------------

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
    status: Option<Status>,
    /// Callees on the changed lines, in first-seen order.
    calls: Vec<Call>,
}

struct Call {
    name: String,
    on_added: bool,
    on_removed: bool,
}

/// The symbol's name is defined on only one side of the diff. Matched by name only, so a
/// rename shows as one deleted and one new symbol.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Status {
    New,
    Deleted,
}

impl Signals {
    fn render(&self) -> String {
        let mut parts = vec![match self.refs {
            Some(n) => format!("refs={n}"),
            None => "refs=?".to_string(),
        }];
        if self.branch_delta != 0 {
            let sign = if self.branch_delta > 0 { "+" } else { "" };
            parts.push(format!("branch{sign}{}", self.branch_delta));
        }
        match self.status {
            Some(Status::New) => parts.push("new".to_string()),
            Some(Status::Deleted) => parts.push("deleted".to_string()),
            None => {}
        }
        parts.join(", ")
    }

    /// `+name` called only on added lines, `-name` only on removed ones, `name` on both.
    fn render_calls(&self) -> Vec<String> {
        self.calls
            .iter()
            .map(|c| match (c.on_added, c.on_removed) {
                (true, false) => format!("+{}", c.name),
                (false, true) => format!("-{}", c.name),
                _ => c.name.clone(),
            })
            .collect()
    }
}

/// Read both sides of the file and attribute its changed lines to the symbols enclosing them,
/// then attach `refs`. The attribution itself is pure (`attribute_signals`); only the source
/// reads and the git-backed `refs` live here.
///
/// If a side the diff names can't be read, the file is skipped: with one side missing, every
/// symbol of the other would look new or deleted.
fn collect_symbols(root: &str, file: &ChangedFile, side: SourceSide) -> Vec<SymbolSignals> {
    let sides = file.old.iter().chain(&file.new);
    if !sides.clone().any(|s| symbols::is_supported(&s.path)) {
        return Vec::new(); // don't read sources that would be discarded
    }
    // Read both sides before parsing either, so a skipped file reports only why it was skipped.
    let old_source = match &file.old {
        None => None,
        Some(old) => match read_old_side(old) {
            Some(source) => Some((old, source)),
            None => return Vec::new(),
        },
    };
    let new_source = match &file.new {
        None => None,
        Some(new) => match read_new_side(root, new, side) {
            Some(source) => Some((new, source)),
            None => return Vec::new(),
        },
    };
    let parse = |read: Option<(&Side, String)>, label| {
        read.map_or_else(SymbolDefs::default, |(side, source)| {
            parse_side(side, &source, label)
        })
    };
    let old = parse(old_source, " (old side)");
    let new = parse(new_source, "");
    let mut symbols = attribute_signals(&old, &new, &file.lines);
    for sym in &mut symbols {
        sym.signals.refs = sym.ident.as_deref().and_then(|i| ref_line_count(root, i));
    }
    symbols
}

fn parse_side(side: &Side, source: &str, label: &str) -> SymbolDefs {
    let parsed = find_symbol_defs(&side.path, source);
    if !parsed.error_lines.is_empty() {
        eprintln!(
            "chsig: parse errors in {}{label} at {}; symbol ranges may be off",
            side.path,
            render_lines(&parsed.error_lines)
        );
    }
    parsed
}

/// Attribute changed lines to enclosing symbols, in first-seen order. Pure (no git, no disk)
/// so the attribution is directly testable.
///
/// An added line goes to the new-side def containing it, a removed line to the old-side one.
/// Spans on one side don't nest (`find_symbol_defs` keeps only outermost defs) but can share a
/// line (`fn a() {} fn b() {}`), where the later def wins. refs is left `None` for the caller
/// to measure.
///
/// Signals aggregate per shown name across both sides: an edited function is one row however
/// its lines moved, and so are overloads, which repeat one name. A call is recorded when the
/// line holding its name changed, so an untouched call inside a changed block isn't listed.
fn attribute_signals(
    old: &SymbolDefs,
    new: &SymbolDefs,
    lines: &[ChangedLine],
) -> Vec<SymbolSignals> {
    fn enclosing(defs: &[SymbolDef], line: usize) -> Option<&SymbolDef> {
        defs.iter()
            .filter(|d| d.start <= line && line <= d.end)
            .max_by_key(|d| d.start)
    }
    let defined = |side: &SymbolDefs, name: &str| side.defs.iter().any(|d| d.name == name);

    let mut touched: Vec<SymbolSignals> = Vec::new();
    for changed in lines {
        let (side, line, added) = match *changed {
            ChangedLine::Added(line) => (new, line, true),
            ChangedLine::Removed(line) => (old, line, false),
        };
        let Some(def) = enclosing(&side.defs, line) else {
            continue;
        };
        let i = match touched.iter().position(|s| s.name == def.name) {
            Some(i) => i,
            None => {
                touched.push(SymbolSignals {
                    name: def.name.clone(),
                    ident: def.ident.clone(),
                    signals: Signals {
                        status: match (defined(old, &def.name), defined(new, &def.name)) {
                            (true, false) => Some(Status::Deleted),
                            (false, true) => Some(Status::New),
                            _ => None,
                        },
                        ..Signals::default()
                    },
                });
                touched.len() - 1
            }
        };
        let signals = &mut touched[i].signals;
        let sign = if added { 1 } else { -1 };
        let branches = side.branches.iter().filter(|l| **l == line).count();
        signals.branch_delta += sign * branches as i32;
        for (_, name) in side.calls.iter().filter(|(l, _)| *l == line) {
            let call = match signals.calls.iter().position(|c| c.name == *name) {
                Some(j) => &mut signals.calls[j],
                None => {
                    signals.calls.push(Call {
                        name: name.clone(),
                        on_added: false,
                        on_removed: false,
                    });
                    signals.calls.last_mut().expect("just pushed")
                }
            };
            if added {
                call.on_added = true;
            } else {
                call.on_removed = true;
            }
        }
    }
    touched
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests pin *current* behavior as a regression anchor — including known defects — so
    // later tuning can't silently drift. I/O fns (git_*,
    // ref_line_count, collect_symbols) are left to integration; not exercised here.

    use ChangedLine::{Added, Removed};

    fn attribute(
        path: &str,
        old: &str,
        new: &str,
        lines: &[ChangedLine],
    ) -> Vec<(String, i32, Option<Status>)> {
        let parse = |src: &str| find_symbol_defs(path, src);
        attribute_signals(&parse(old), &parse(new), lines)
            .into_iter()
            .map(|s| (s.name, s.signals.branch_delta, s.signals.status))
            .collect()
    }

    // A change in a component's JSX return, below a handler's body, must attribute to the
    // component. The handler is nested in it, so it is not a symbol of its own either.
    #[test]
    fn change_in_component_attributes_to_the_component_not_a_handler() {
        let new = "\
const Component = () => {
  const handleClick = () => {
    if (something) {}
  }

  return (
    condition && <Child />
  )
}
";
        let old = "const Component = () => {\n  return null\n}\n";
        let lines = [Added(3), Added(7)];
        let out = attribute("c.tsx", old, new, &lines);
        assert_eq!(out, vec![("Component".to_string(), 2, None)]);
    }

    // A removed branch subtracts: swapping `if x {}` for `if y {}` is net zero, not +2.
    #[test]
    fn removed_branches_subtract() {
        let old = "fn foo() {\n    if x {}\n}\n";
        let new = "fn foo() {\n    if y {}\n}\n";
        let lines = [Removed(2), Added(2)];
        let out = attribute("x.rs", old, new, &lines);
        assert_eq!(out, vec![("foo".to_string(), 0, None)]);
    }

    // A `}` inside a string used to close `alpha` early under brace counting, dropping the
    // change below it.
    #[test]
    fn brace_in_string_does_not_end_the_symbol() {
        let new = "\
fn alpha() {
    let s = \"}\";
    if p && q {}
}
fn beta() {
    let x = 1;
}
";
        let old = new.replace("    if p && q {}\n", "");
        let out = attribute("a.rs", &old, new, &[Added(3)]);
        assert_eq!(out, vec![("alpha".to_string(), 2, None)]);
    }

    // Removed lines are placed on the old side. Placed by their new-side position instead,
    // a deleted `foo` vanished and its lines landed on the unchanged `bar` below it.
    #[test]
    fn a_deleted_function_is_shown_deleted_and_its_neighbor_untouched() {
        let old = "fn foo() {\n    if y {}\n}\nfn bar() {\n    let x = 1;\n}\n";
        let new = "fn bar() {\n    let x = 1;\n}\n";
        let lines = [Removed(1), Removed(2), Removed(3)];
        let out = attribute("b.rs", old, new, &lines);
        assert_eq!(out, vec![("foo".to_string(), -1, Some(Status::Deleted))]);
    }

    // Matched by name: moving a function within the file is one edited row, and a rename is a
    // deleted row plus a new one (no rename is inferred).
    #[test]
    fn symbols_match_across_sides_by_name() {
        let old = "fn a() {}\nfn b() {\n    if x {}\n}\nfn gone() {}\n";
        let new = "fn b() {\n    if x {}\n}\nfn a() {}\nfn renamed() {}\n";
        let lines = [
            Added(1),
            Added(2),
            Added(3),
            Removed(2),
            Removed(3),
            Removed(4),
            Removed(5),
            Added(5),
        ];
        let out = attribute("m.rs", old, new, &lines);
        assert_eq!(
            out,
            vec![
                ("b".to_string(), 0, None),
                ("gone".to_string(), 0, Some(Status::Deleted)),
                ("renamed".to_string(), 0, Some(Status::New)),
            ]
        );
    }

    // Same-named defs in one file (`new` in two impls) are separate symbols; each keeps its own
    // signals instead of being summed under one name, and its owner tells them apart.
    #[test]
    fn same_name_defs_stay_separate_rows() {
        let new = "\
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
        let old = "impl A {\n    fn new() {}\n}\nimpl B {\n    fn new() {}\n}\n";
        let lines = [Added(3), Added(8)];
        let out = attribute("n.rs", old, new, &lines);
        assert_eq!(
            out,
            vec![
                ("A::new".to_string(), 1, None),
                ("B::new".to_string(), 2, None)
            ]
        );
    }

    // A call kept across an edit is bare; one only added or only removed carries its side.
    #[test]
    fn calls_carry_the_side_they_were_changed_on() {
        let old = "fn save() {\n    db.query(q);\n    log(a);\n}\n";
        let new = "fn save() {\n    tx.update(q);\n    log(b);\n}\n";
        let lines = [Removed(2), Removed(3), Added(2), Added(3)];
        let parse = |src: &str| find_symbol_defs("s.rs", src);
        let out = attribute_signals(&parse(old), &parse(new), &lines);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].signals.render_calls(),
            vec!["-db.query", "log", "+tx.update"]
        );
    }

    #[test]
    fn wrap_continues_under_the_prefix_and_drops_nothing() {
        let items: Vec<String> = ["+aaaa", "+bbbb", "+cccc", "-dddd"]
            .map(String::from)
            .into();
        assert_eq!(
            wrap("  calls: ", &items, 24),
            vec!["  calls: +aaaa, +bbbb,", "         +cccc, -dddd"]
        );
        assert_eq!(wrap("  calls: ", &items[..1], 24), vec!["  calls: +aaaa"]);
        assert!(wrap("  calls: ", &[], 24).is_empty());
    }

    #[test]
    fn render_lines_names_the_first_three() {
        assert_eq!(render_lines(&[12]), "line 12");
        assert_eq!(render_lines(&[12, 40]), "lines 12, 40");
        assert_eq!(render_lines(&[1, 2, 3, 4]), "lines 1, 2, 3, …");
    }

    #[test]
    fn render_joins_present_signals_only() {
        let bare = Signals {
            refs: Some(3),
            ..Signals::default()
        };
        assert_eq!(bare.render(), "refs=3");
        let pos = Signals {
            refs: Some(3),
            branch_delta: 2,
            ..Signals::default()
        };
        assert_eq!(pos.render(), "refs=3, branch+2");
        let neg = Signals {
            refs: Some(1),
            branch_delta: -1,
            status: Some(Status::Deleted),
            ..Signals::default()
        };
        assert_eq!(neg.render(), "refs=1, branch-1, deleted");
        let unmeasured = Signals {
            status: Some(Status::New),
            ..Signals::default()
        };
        assert_eq!(unmeasured.render(), "refs=?, new");
    }
}
