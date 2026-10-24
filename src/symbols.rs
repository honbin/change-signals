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

pub(crate) struct SymbolDefs {
    pub(crate) defs: Vec<SymbolDef>,
    /// The parser recovered from syntax it could not read; spans around it may be off.
    pub(crate) has_errors: bool,
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
    let (language, lang): (Language, Lang) = match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => (tree_sitter_rust::LANGUAGE.into(), Lang::Rust),
        Some("ts" | "mts" | "cts") => {
            (tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), Lang::Ts)
        }
        Some("tsx" | "js" | "jsx" | "mjs" | "cjs") => {
            (tree_sitter_typescript::LANGUAGE_TSX.into(), Lang::Ts)
        }
        _ => {
            return SymbolDefs {
                defs: Vec::new(),
                has_errors: false,
            };
        }
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
    defs.sort_by_key(|d| d.start);
    SymbolDefs {
        defs,
        has_errors: tree.root_node().has_error(),
    }
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
            start: node.start_position().row + 1,
            end: node.end_position().row + 1,
        });
        return;
    }
    // Owners nest: a method in `const api = { users: { list() {} } }` is `api.users.list`.
    let own = owner_name(node, src, lang).map(|o| qualify(&o));
    let owner = own.as_deref().or(owner);
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_defs(child, src, lang, owner, out);
    }
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
    let Some(args) = value
        .child_by_field_name("arguments")
        .filter(|_| value.kind() == "call_expression")
    else {
        return false;
    };
    let mut cursor = args.walk();
    args.named_children(&mut cursor).any(is_fn)
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
    fn syntax_errors_are_flagged() {
        let e = |src: &str| find_symbol_defs("a.rs", src).has_errors;
        assert!(!e("fn a() {\n    let s = \"{\";\n}\n"));
        assert!(e("fn a( {\n}\n"));
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
