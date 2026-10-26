//! Find the symbols of a source file — its outermost named functions and methods — with
//! their line spans.

use tree_sitter::{Language, Node, Parser};

/// A definition with its body span. Start alone is not enough: to attribute a changed line
/// we must know where the body *ends*, or trailing code (a component's JSX return, module
/// tail) wrongly folds into the last preceding def even when that def never changed.
pub(crate) struct SymbolDef {
    /// Shown name: the identifier, qualified by its owner when it has one (`Foo::new`,
    /// `Client.fetch`), so same-named methods in one file stay distinguishable.
    pub(crate) name: String,
    /// `None` for an anonymous `export default`, shown as `default`, which is no identifier.
    pub(crate) ident: Option<String>,
    pub(crate) start: usize, // 1-based def line
    pub(crate) end: usize,   // 1-based line where the body closes (== start for a one-liner)
}

#[derive(Default)]
pub(crate) struct SymbolDefs {
    pub(crate) defs: Vec<SymbolDef>,
    /// 1-based lines, ascending, where the parser recovered from syntax it could not read; spans
    /// around them may be off.
    pub(crate) error_lines: Vec<usize>,
    /// (1-based line, callee) for every call in the file, placed on the line of the callee's
    /// name.
    pub(crate) calls: Vec<(usize, String)>,
    /// 1-based line of each branch point, placed on the token that introduces it.
    pub(crate) branches: Vec<usize>,
}

/// Named functions and methods with their spans, ascending by start, for the file's language.
///
/// Only Rust and TS/JS are parsed; any other extension returns empty rather than being run
/// through a grammar that doesn't fit it. JS is read with the TSX grammar, which also covers
/// JSX in `.js`.
///
/// Only the outermost named function is a symbol: a named function inside another one
/// (a component's `handleClick`, a local helper) is not, and its changes attribute to the
/// enclosing function. Listing inner ones mislabels changes with local names that a reader
/// can't locate; the cost is that a component's handlers no longer show separately.
pub(crate) fn find_symbol_defs(path: &str, source: &str) -> SymbolDefs {
    let Some((language, lang)) = grammar_for(path) else {
        return SymbolDefs::default();
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .expect("grammar built for this tree-sitter version");
    let tree = parser
        .parse(source, None)
        .expect("parse without a timeout or cancellation returns a tree");
    let mut defs = Vec::new();
    collect_defs(tree.root_node(), source, lang, None, &mut defs);
    // A def's span starts at its decorators, so a function in their arguments
    // (`@UseInterceptors(X({ destination: () => … }))`) lands inside it; it folds into the
    // def like any nested function.
    let spans: Vec<(usize, usize)> = defs.iter().map(|d| (d.start, d.end)).collect();
    defs.retain(|d| {
        !spans
            .iter()
            .any(|&(s, e)| s <= d.start && d.end <= e && (s, e) != (d.start, d.end))
    });
    defs.sort_by_key(|d| d.start);
    let mut calls = Vec::new();
    collect_calls(tree.root_node(), source, &mut calls);
    let mut branches = Vec::new();
    collect_branches(tree.root_node(), &mut branches);
    SymbolDefs {
        defs,
        error_lines: error_lines(tree.root_node()),
        calls,
        branches,
    }
}

/// Where the parse went wrong: each ERROR node, whose contents are not descended into, and each
/// token the parser assumed MISSING.
fn error_lines(root: Node) -> Vec<usize> {
    fn walk(node: Node, out: &mut Vec<usize>) {
        if node.is_error() || node.is_missing() {
            out.push(node.start_position().row + 1);
            return;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor).filter(|c| c.has_error()) {
            walk(child, out);
        }
    }
    let mut out = Vec::new();
    if root.has_error() {
        walk(root, &mut out);
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Branch points: an `if` (an `else if` counts once, as its `if`), a final `else`, a `case`,
/// `default` or match arm, a ternary, `&&` / `||` / `??`, and Rust's `let ... else`. Loops are not counted, `while
/// let` included: a loop's condition decides whether to go on, not which way to go. Each point
/// sits on the line of its own keyword or operator, so a changed line carries exactly the
/// branches written on it, even when a formatter splits a condition across lines.
fn collect_branches(node: Node, out: &mut Vec<usize>) {
    // A let chain's `&&` (`if let Some(x) = a && b`) is a bare token of the chain, not a
    // binary_expression.
    if node.kind() == "let_chain" {
        let mut cursor = node.walk();
        out.extend(
            node.children(&mut cursor)
                .filter(|c| c.kind() == "&&")
                .map(|c| c.start_position().row + 1),
        );
    }
    let at = match node.kind() {
        "if_statement" | "if_expression" | "switch_case" | "switch_default" | "match_arm" => {
            Some(node)
        }
        "else_clause" => node
            .named_child(0)
            .filter(|body| !matches!(body.kind(), "if_statement" | "if_expression"))
            .map(|_| node),
        "ternary_expression" => {
            let mut cursor = node.walk();
            node.children(&mut cursor).find(|c| c.kind() == "?")
        }
        "binary_expression" => node
            .child_by_field_name("operator")
            .filter(|op| matches!(op.kind(), "&&" | "||" | "??")),
        "let_declaration" => {
            let mut cursor = node.walk();
            node.children(&mut cursor).find(|c| c.kind() == "else")
        }
        _ => None,
    };
    if let Some(at) = at {
        out.push(at.start_position().row + 1);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_branches(child, out);
    }
}

/// Whether `find_symbol_defs` can find anything in `path`, so callers can skip reading files
/// it would discard.
pub(crate) fn is_supported(path: &str) -> bool {
    grammar_for(path).is_some()
}

fn grammar_for(path: &str) -> Option<(Language, Lang)> {
    let grammar = match path.rsplit_once('.').map(|(_, ext)| ext)? {
        "rs" => (tree_sitter_rust::LANGUAGE.into(), Lang::Rust),
        "ts" | "mts" | "cts" => (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), Lang::Ts),
        "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            (tree_sitter_typescript::LANGUAGE_TSX.into(), Lang::Ts)
        }
        _ => return None,
    };
    Some(grammar)
}

#[derive(Clone, Copy)]
enum Lang {
    Rust,
    Ts,
}

impl Lang {
    fn owner_sep(self) -> &'static str {
        match self {
            Lang::Rust => "::",
            Lang::Ts => ".",
        }
    }
}

/// Walk `node`, recording each named function and not descending into it. `owner` is the
/// name of the nearest enclosing impl, trait, class, or bound object literal.
fn collect_defs(node: Node, src: &str, lang: Lang, owner: Option<&str>, out: &mut Vec<SymbolDef>) {
    let qualify = |name: &str| match owner {
        Some(o) => format!("{o}{}{name}", lang.owner_sep()),
        None => name.to_string(),
    };
    if let Some(def) = def_name(node, src, lang) {
        let ident = match def {
            DefName::Ident(i) => Some(i),
            DefName::AnonymousDefault => None,
        };
        out.push(SymbolDef {
            name: qualify(ident.as_deref().unwrap_or("default")),
            ident,
            start: start_with_attributes(node),
            end: node.end_position().row + 1,
        });
        return;
    }
    // Owners nest: a method in `const api = { users: { list() {} } }` is `api.users.list`.
    let own = owner_name(node, src, lang).map(|o| qualify(&o));
    let owner = own.as_deref().or(owner);
    // `@Dec export class B {}` hangs the decorators on the export, not the class; they are
    // still the class's, as they are without `export`.
    let decorated = node
        .child_by_field_name("declaration")
        .filter(|_| node.kind() == "export_statement")
        .and_then(|d| owner_name(d, src, lang))
        .map(|o| qualify(&o));
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let owner = match child.kind() {
            "decorator" => decorated.as_deref().or(owner),
            _ => owner,
        };
        collect_defs(child, src, lang, owner, out);
    }
}

/// The 1-based line a def's span starts on: its first attribute or decorator, since those
/// change what it does (routing, `cfg`, `#[test]`), but not a doc comment above them, which
/// doesn't. A comment between an attribute and the def falls inside the span regardless.
fn start_with_attributes(def: Node) -> usize {
    let mut start = def.start_position().row;
    let mut sibling = def.prev_named_sibling();
    while let Some(s) = sibling {
        match s.kind() {
            "attribute_item" | "decorator" => start = s.start_position().row,
            "line_comment" | "block_comment" | "comment" => {}
            _ => break,
        }
        sibling = s.prev_named_sibling();
    }
    start + 1
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    &src[node.byte_range()]
}

/// A property name as written, without the quotes of a string key (`'on-open'() {}`).
fn key_text<'a>(node: Node, src: &'a str) -> &'a str {
    text(node, src).trim_matches(['\'', '"'])
}

enum DefName {
    Ident(String),
    AnonymousDefault,
}

fn def_name(node: Node, src: &str, lang: Lang) -> Option<DefName> {
    let field_text = |field: &str| node.child_by_field_name(field).map(|n| key_text(n, src));
    let value_binds_function = || binds_function(node.child_by_field_name("value"));
    let ident = match (lang, node.kind()) {
        (Lang::Rust, "function_item" | "function_signature_item") => field_text("name"),
        (
            Lang::Ts,
            "function_declaration"
            | "generator_function_declaration"
            | "function_signature"
            | "method_definition"
            | "abstract_method_signature",
        ) => field_text("name"),
        // A body-less method in a class is an overload or a `declare class` member; elsewhere
        // (interfaces, object types) it is a type, not a definition.
        (Lang::Ts, "method_signature")
            if node.parent().is_some_and(|p| p.kind() == "class_body") =>
        {
            field_text("name")
        }
        (Lang::Ts, "variable_declarator") => node
            .child_by_field_name("name")
            .filter(|n| n.kind() == "identifier")
            .filter(|_| value_binds_function())
            .map(|n| text(n, src)),
        (Lang::Ts, "public_field_definition") => {
            field_text("name").filter(|_| value_binds_function())
        }
        (Lang::Ts, "pair") => field_text("key").filter(|_| value_binds_function()),
        // `export default function () {}`, `export default makeHandler(async () => {})`:
        // bound, but to no name.
        (Lang::Ts, "export_statement") if value_binds_function() => {
            return Some(DefName::AnonymousDefault);
        }
        _ => None,
    }?;
    (!ident.is_empty()).then(|| DefName::Ident(ident.to_string()))
}

/// Every call's callee, named as written at the call: `recv.method`, a bare `name`,
/// `Path::name`, `new Name`, or `name!` for a Rust macro. No vocabulary on purpose: which calls
/// cross a boundary (a database, the network) depends on the libraries a repo uses, and is the
/// reader's judgment, not a fact chsig can state.
fn collect_calls(node: Node, src: &str, out: &mut Vec<(usize, String)>) {
    if let Some((name, at)) = callee(node, src) {
        out.push((at.start_position().row + 1, name));
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_calls(child, src, out);
    }
}

fn callee<'t>(node: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    match node.kind() {
        "call_expression" => match misread_await(node) {
            Some(awaited) => call_target(awaited, src),
            None => call_target(node.child_by_field_name("function")?, src),
        },
        "new_expression" => {
            let (name, at) = call_target(node.child_by_field_name("constructor")?, src)?;
            Some((format!("new {name}"), at))
        }
        "macro_invocation" => {
            let (name, at) = call_target(node.child_by_field_name("macro")?, src)?;
            Some((format!("{name}!"), at))
        }
        _ => None,
    }
}

/// tree-sitter-typescript reads `await f<T>()` as `(await f)<T>()`, without a parse error: a
/// call with type arguments whose callee is the `await`. Read correctly an awaited callee is
/// parenthesized (`(await f)()`), and a Rust call carries no type arguments of its own, so the
/// shape comes only from the misreading. Returns `f`, the callee the source wrote.
fn misread_await(call: Node) -> Option<Node> {
    let f = call.child_by_field_name("function")?;
    (f.kind() == "await_expression" && call.child_by_field_name("type_arguments").is_some())
        .then(|| f.named_child(0))
        .flatten()
}

/// The name a call goes to, and the node holding it (where the call is placed). A call whose
/// target is itself an expression (`fns[i]()`, `(a || b)()`) has no name and is skipped.
fn call_target<'t>(f: Node<'t>, src: &str) -> Option<(String, Node<'t>)> {
    match f.kind() {
        "identifier" => Some((text(f, src).to_string(), f)),
        "generic_function" => call_target(f.child_by_field_name("function")?, src),
        "scoped_identifier" => {
            let name = f.child_by_field_name("name")?;
            let full = match f.child_by_field_name("path") {
                Some(path) => format!("{}::{}", last_segment(path, src), text(name, src)),
                None => text(name, src).to_string(),
            };
            Some((full, name))
        }
        "member_expression" | "field_expression" => {
            let (object, property) = member_parts(f)?;
            let recv = receiver_name(object, src).unwrap_or("*");
            Some((format!("{recv}.{}", text(property, src)), property))
        }
        _ => None,
    }
}

/// (object, property) of `a.b` in either grammar.
fn member_parts(n: Node) -> Option<(Node, Node)> {
    let (object, property) = match n.kind() {
        "member_expression" => ("object", "property"),
        "field_expression" => ("value", "field"),
        _ => return None,
    };
    Some((
        n.child_by_field_name(object)?,
        n.child_by_field_name(property)?,
    ))
}

/// The receiver's own name (`tx`, `this`, `SETTINGS` in `env.SETTINGS`), or, when the receiver
/// is a call (`db.select().from`), the name its call chain starts from.
fn receiver_name<'s>(n: Node, src: &'s str) -> Option<&'s str> {
    match n.kind() {
        "member_expression" | "field_expression" => Some(text(member_parts(n)?.1, src)),
        "call_expression" => chain_root(n.child_by_field_name("function")?, src),
        "scoped_identifier" => Some(last_segment(n, src)),
        k if is_name(k) => Some(text(n, src)),
        _ => receiver_name(wrapped(n)?, src),
    }
}

fn chain_root<'s>(n: Node, src: &'s str) -> Option<&'s str> {
    match n.kind() {
        "member_expression" | "field_expression" => chain_root(member_parts(n)?.0, src),
        "call_expression" | "generic_function" => {
            chain_root(n.child_by_field_name("function")?, src)
        }
        "scoped_identifier" => Some(last_segment(n, src)),
        k if is_name(k) => Some(text(n, src)),
        _ => chain_root(wrapped(n)?, src),
    }
}

/// The value a node only wraps (`(x)`, `await x`, `x!`, `x?`, `x as T`, `&x`, `*x`), so a
/// receiver is named through it. Any other expression (`a + b`, `c ? d : e`, `!x`) yields a
/// different value and has no name of its own.
fn wrapped(n: Node) -> Option<Node> {
    match n.kind() {
        "parenthesized_expression"
        | "await_expression"
        | "non_null_expression"
        | "as_expression"
        | "satisfies_expression"
        | "try_expression" => n.named_child(0),
        "type_cast_expression" | "reference_expression" => n.child_by_field_name("value"),
        "unary_expression" if n.child(0).is_some_and(|op| op.kind() == "*") => n.named_child(0),
        _ => None,
    }
}

fn is_name(kind: &str) -> bool {
    matches!(kind, "identifier" | "this" | "self" | "type_identifier")
}

/// `b` of `a::b`, or the node's text when it isn't a path.
fn last_segment<'s>(n: Node, src: &'s str) -> &'s str {
    match n
        .child_by_field_name("name")
        .filter(|_| n.kind() == "scoped_identifier")
    {
        Some(name) => text(name, src),
        None => text(n, src),
    }
}

/// Whether a binding's value is a function, or a call handed a function (`memo(() => ...)`,
/// `useCallback(...)`), which is named only by the binding. A call without one
/// (`useQuery({...})`) is a value, not a symbol.
fn binds_function(value: Option<Node>) -> bool {
    let is_fn = |n: Node| {
        matches!(
            n.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        )
    };
    let Some(value) = value else {
        return false;
    };
    if is_fn(value) {
        return true;
    }
    if awaits_whole_chain(value) {
        return false;
    }
    let Some(args) = value
        .child_by_field_name("arguments")
        .filter(|_| value.kind() == "call_expression")
    else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).any(is_fn)
}

/// Whether a misread `await` (see `misread_await`) heads the call chain of `value`. The source's
/// `await` covers the whole chain (`await f<T>().catch(() => null)`), so the value is awaited, not
/// a call handed a function.
fn awaits_whole_chain(value: Node) -> bool {
    let mut node = value;
    loop {
        let next = match node.kind() {
            "call_expression" if misread_await(node).is_some() => return true,
            "call_expression" => node.child_by_field_name("function"),
            "member_expression" => node.child_by_field_name("object"),
            _ => None,
        };
        let Some(next) = next else {
            return false;
        };
        node = next;
    }
}

/// The name `node` lends to the defs inside it: the type of an `impl`, a trait, a class, or the
/// variable or key an object literal is bound to.
fn owner_name(node: Node, src: &str, lang: Lang) -> Option<String> {
    let name = match (lang, node.kind()) {
        (Lang::Rust, "impl_item") => {
            // `impl<T> Foo<T>` and `impl Clone for &Foo` are owned by `Foo`.
            let mut ty = node.child_by_field_name("type")?;
            while matches!(ty.kind(), "generic_type" | "reference_type") {
                ty = ty.child_by_field_name("type")?;
            }
            text(ty, src)
        }
        (Lang::Rust, "trait_item")
        | (Lang::Ts, "class_declaration" | "abstract_class_declaration" | "class") => {
            text(node.child_by_field_name("name")?, src)
        }
        (Lang::Ts, "object") => {
            let parent = node.parent()?;
            match parent.kind() {
                "variable_declarator" => {
                    let name = parent.child_by_field_name("name")?;
                    (name.kind() == "identifier").then(|| text(name, src))?
                }
                "pair" => key_text(parent.child_by_field_name("key")?, src),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(path: &str, src: &str) -> Vec<String> {
        find_symbol_defs(path, src)
            .defs
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    // rustfmt and prettier break long signatures across lines, so the body opens on a later
    // line. Brace counting closed such a def on its first line and dropped its whole body.
    #[test]
    fn multi_line_signature_keeps_its_body() {
        let rs = "\
fn list_line(
    entry: &Entry,
) -> Line {
    if x {}
}
";
        let ts = "\
export function createWidget(
  options: Options,
): Widget {
  if (x) {}
}
";
        for (path, src) in [("a.rs", rs), ("a.ts", ts)] {
            let defs = find_symbol_defs(path, src).defs;
            assert_eq!(defs.len(), 1, "{path}");
            assert_eq!((defs[0].start, defs[0].end), (1, 5), "{path}");
        }
    }

    // Attributes and decorators belong to the def they precede; a doc comment above them
    // doesn't.
    #[test]
    fn attributes_and_decorators_start_the_span_but_doc_comments_do_not() {
        let rs = "\
fn before() {}
/// Docs.
#[cfg(test)]
// note
#[inline]
fn f() {}
";
        let defs = find_symbol_defs("a.rs", rs).defs;
        assert_eq!((defs[0].start, defs[0].end), (1, 1));
        assert_eq!((defs[1].start, defs[1].end), (3, 6));

        let ts = "\
class C {
  /** Docs. */
  @Get('a')
  @Auth()
  h() {}
}
";
        let defs = find_symbol_defs("a.ts", ts).defs;
        assert_eq!(
            (defs[0].name.as_str(), defs[0].start, defs[0].end),
            ("C.h", 3, 5)
        );
    }

    #[test]
    fn nested_named_functions_fold_into_the_outermost() {
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
        assert_eq!(names("hook.ts", src), vec!["useThing", "makeRequest"]);
    }

    #[test]
    fn functions_in_a_methods_decorators_fold_into_the_method() {
        let src = "\
@Module({ providers: [{ useFactory: () => 1 }] })
class C {
  @Use(Intercept({
    destination: (r) => r,
  }))
  upload() {}
}
";
        assert_eq!(names("a.ts", src), vec!["C.useFactory", "C.upload"]);
        let exported = src.replacen("class C", "export class C", 1);
        assert_eq!(names("a.ts", &exported), vec!["C.useFactory", "C.upload"]);
    }

    #[test]
    fn syntax_errors_are_placed_on_their_lines() {
        let e = |src: &str| find_symbol_defs("a.rs", src).error_lines;
        assert!(e("fn a() {\n    let s = \"{\";\n}\n").is_empty());
        assert_eq!(e("fn a() {}\nfn b( {\n}\nfn c() {}\n"), vec![2]);
    }

    // `default` is a placeholder, not a name: grepping it would count every `default` in the
    // repo as this symbol's refs.
    #[test]
    fn anonymous_default_export_has_no_identifier() {
        let defs =
            find_symbol_defs("a.ts", "export default () => {};\nexport function f() {}\n").defs;
        let got: Vec<_> = defs
            .iter()
            .map(|d| (d.name.as_str(), d.ident.as_deref()))
            .collect();
        assert_eq!(got, vec![("default", None), ("f", Some("f"))]);
    }

    fn calls(path: &str, src: &str) -> Vec<String> {
        find_symbol_defs(path, src)
            .calls
            .into_iter()
            .map(|(_, c)| c)
            .collect()
    }

    // Receivers are named as written, a call chain by the name it starts from, and nothing
    // is resolved: `tx` is just a name here.
    #[test]
    fn ts_calls_are_named_as_written() {
        let src = "\
foo();
tx.update(t).set(v);
db.select().from(t).where(c);
env.SETTINGS.get(k);
this.client.request(p);
(await load()).parse();
(a || b).trim();
(c ? d : e).run();
sql`SELECT 1`;
new Date();
fns[0]();
cache.get<string>(k);
";
        assert_eq!(
            calls("a.ts", src),
            vec![
                "foo",
                "tx.set",
                "tx.update",
                "db.where",
                "db.from",
                "db.select",
                "SETTINGS.get",
                "client.request",
                "load.parse",
                "load",
                "*.trim",
                "*.run",
                "sql",
                "new Date",
                "cache.get",
            ]
        );
    }

    #[test]
    fn rust_calls_are_named_as_written() {
        let src = "\
fn f() {
    fs::write(p, b);
    conn.execute(q).await?.rows();
    self.client.get(u);
    Foo::new();
    sqlx::query!(\"SELECT 1\");
    (x as u64).pow(2);
    (&mut buf).clear();
    (a + b).abs();
    (!s.is_empty()).then(f);
    n.parse::<u64>().ok();
    Self::CACHE.clear();
}
";
        assert_eq!(
            calls("a.rs", src),
            vec![
                "fs::write",
                "conn.rows",
                "conn.execute",
                "client.get",
                "Foo::new",
                "sqlx::query!",
                "x.pow",
                "buf.clear",
                "*.abs",
                "*.then",
                "s.is_empty",
                "n.ok",
                "n.parse",
                "CACHE.clear",
            ]
        );
        // Only that a path call as receiver gets a name: which name (`read` or `fs::read`) is
        // not settled yet.
        let path_call = calls("a.rs", "fn f() {\n    fs::read(p).unwrap();\n}\n");
        assert!(
            !path_call.iter().any(|c| c.starts_with("*.")),
            "{path_call:?}"
        );
    }

    // The grammar reads `await f<T>()` as `(await f)<T>()`; the call is still named by `f`, and
    // a function handed to the awaited chain doesn't make its binding a def. A parenthesized
    // `(await pick)()` is read correctly and stays unnamed, like any expression callee.
    #[test]
    fn awaited_generic_calls_are_named_by_their_callee() {
        let src = "\
async function load() {
  const kv = await env.get<string>('k');
  const rows = await client.query<{
    id: number;
  }>(sql);
  const body = await req.json<Body>().catch(() => null);
  const res = await run<Out>(() => 1);
  const ok = (await pick)();
}
";
        assert_eq!(
            calls("a.ts", src),
            vec!["env.get", "client.query", "req.catch", "req.json", "run"]
        );
        assert_eq!(names("a.ts", src), vec!["load"]);
        let top = "\
export const res = await run<Out>(() => 1);
export const body = await req.json<Body>().catch(() => null);
";
        assert!(names("a.ts", top).is_empty());
    }

    // Each call is placed on the line holding its name, so in a chain split across lines
    // every link lands on its own line.
    #[test]
    fn a_call_is_placed_on_the_line_of_its_name() {
        let src = "db\n  .select()\n  .from(t);\n";
        let placed = find_symbol_defs("a.ts", src).calls;
        assert_eq!(
            placed,
            vec![(3, "db.from".to_string()), (2, "db.select".to_string())]
        );
    }

    fn branch_lines(path: &str, src: &str) -> Vec<usize> {
        let mut lines = find_symbol_defs(path, src).branches;
        lines.sort();
        lines
    }

    // Arrow functions, optional chaining and branch words inside strings or comments are not
    // branches; `else if` is one branch, not two.
    #[test]
    fn ts_branches_come_from_syntax() {
        let src = "\
const f = (x) => x?.y;
if (a && b) {
} else if (c) {
} else {
}
const s = 'if (x) else';
switch (k) { case 1: break; default: }
const v = ok
  ? 1
  : d ?? 2;
";
        assert_eq!(branch_lines("a.ts", src), vec![2, 2, 3, 4, 7, 7, 9, 10]);
    }

    // A closure, a `while let` loop and branch words in a comment are not branches.
    #[test]
    fn rust_branches_come_from_syntax() {
        let src = "\
fn f() {
    let g = |x| x;
    if let Some(v) = o {
    } else if a || b {
    } else {
    }
    match k {
        1 => {}
        _ => {}
    }
    while let Some(x) = it.next() {}
    let Some(y) = o
        else { return };
    // if match
    if let Some(a) = o && a > 1
        && let Ok(b) = r && (b > 0 || c) {}
}
";
        assert_eq!(
            branch_lines("a.rs", src),
            vec![3, 4, 4, 5, 8, 9, 13, 15, 15, 16, 16, 16]
        );
    }

    #[test]
    fn unsupported_language_has_no_symbols() {
        // A .go file must not be run through another language's grammar.
        let src = "var f = func() {";
        assert!(find_symbol_defs("main.go", src).defs.is_empty());
    }

    #[test]
    fn rust_symbols_are_fns_and_methods_named_by_owner() {
        let src = "\
pub fn alpha() {}
pub(super) fn beta() {}
const unsafe extern \"C\" fn gamma() {}
struct S { x: u8 }
impl Clone for &Bar {
    fn clone(&self) -> Self {}
}
impl<T> Foo<T> {
    pub async fn method(&self) {
        fn helper() {}
        let f = |x| x;
    }
}
trait Run {
    fn run(&self);
    fn twice(&self) {}
}
mod tests {
    impl Bar {
        fn new() {}
    }
    #[test]
    fn it_works() {}
}
";
        assert_eq!(
            names("x.rs", src),
            vec![
                "alpha",
                "beta",
                "gamma",
                "Bar::clone",
                "Foo::method",
                "Run::run",
                "Run::twice",
                "Bar::new",
                "it_works",
            ]
        );
    }

    #[test]
    fn ts_symbols_are_named_functions_and_methods() {
        let src = "\
export function foo() {}
export default function () {}
function* gen() {}
function over(a: string): void;
const bar = () => {};
const Foo: React.FC<Props> = ({ x }) => <div />;
const Memo = memo(() => <div />);
const n = 5;
const q = useQuery({ queryKey: ['k'] });
const config = { retries: 3 };
const handlers = {
  onOpen() {},
  onClose: () => {},
  nested: { 'on-key'() {} },
};
class Client {
  constructor() {}
  fetch() {}
  handle = () => {};
  count = 0;
}
abstract class Base {
  abstract run(): void;
}
declare class Declared {
  over(a: string): void;
  over(a: number): void;
}
interface Shape {
  area(): number;
}
export default {
  fetch(req) {},
};
export default makeHandler(async (event) => {});
describe('x', () => {
  it('y', () => {});
});
";
        assert_eq!(
            names("x.tsx", src),
            vec![
                "foo",
                "default",
                "gen",
                "over",
                "bar",
                "Foo",
                "Memo",
                "handlers.onOpen",
                "handlers.onClose",
                "handlers.nested.on-key",
                "Client.constructor",
                "Client.fetch",
                "Client.handle",
                "Base.run",
                "Declared.over",
                "Declared.over",
                "fetch",
                "default",
            ]
        );
    }
}
