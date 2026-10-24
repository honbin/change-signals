# change-signals (`chsig`)

A small CLI that collects **facts** about the symbols a `git diff` changed, so a human can
decide *where to read first*. It explains nothing and judges nothing — it points.

It is **not** a static analyzer or a code-review tool: it runs no semantic analysis and
makes no judgments. It pulls coarse signals out of a `git diff` as raw material for a
human. **v0.x** — symbols are found with a parser (tree-sitter), but the signals attached
to them are crude on purpose; the output is meant to be skimmed and falsified, not trusted.

```
signals  →  prioritize  →  interrogate
 chsig       optional LLM     human / agent
 collect     choose a         understand
 facts       reading order    adversarially
```

## Philosophy

- **Facts, not judgment.** `refs=1` is a fact; "safe to skip" is a judgment. `chsig` never
  crosses that line: it emits no skip verdict and does not rank by risk. It prints per file
  in diff order. Ranking is the next stage's job.
- The numbers exist so a human can **cheaply falsify** them, not to make you trust the tool.
  So `refs` is "lines matching the name", not "resolved references" — crude on purpose.
- Reducing load by *reducing what you read* is the opposite of generating a better
  explanation. If `chsig` ever started explaining, it would be a worse explainer. It stays
  in the "where to look" lane and leaves understanding to the reader.

## Install from source

```sh
cargo install --locked --path .    # puts `chsig` on PATH
```

Needs a Rust toolchain and a C compiler (for the tree-sitter grammars). Run `chsig` from
inside the repo you want to read. The `chsig-prioritize` stage also needs the `claude` CLI
on PATH.

## Stages

### 1. signals — `chsig`

```sh
chsig main...HEAD    # args go straight to `git diff`; run inside the target repo
chsig                # no args = working diff
```

```
src/cache/store.ts
  getOrFetch                   refs=14, branch+5
  evictExpired                 refs=6, branch+1

src/db/records.ts
  loadRecord                   refs=12
```

### 2. prioritize — `chsig-prioritize`

Pipes the signals to `claude -p`, which names the 3–5 symbols to read first with
signal-grounded reasons, and ends by naming one to interrogate.

```sh
./chsig-prioritize main...HEAD
```

This is a thin shell layer, deliberately **not** part of the Rust binary: the LLM call is
non-deterministic I/O and stays outside the deterministic core (`shell = fetch / toolbox =
compute`). No judgment leaks back into the extractor. Requires `claude` on `PATH`.

### 3. interrogate

Read the selected code and challenge it: verify assumptions, consider alternatives, look
for failure conditions — by hand or with a coding agent. Deliberately outside chsig: the
tool picks where to look, not what to conclude.

## Limitations

- Symbols are found for Rust and TypeScript/JavaScript only; other files are skipped.
- A symbol is the outermost named function or method: free functions, methods of an
  `impl`/trait/class/object literal (shown as `Foo::new`, `Client.fetch`), variables
  bound to a function or to a call handed one (`memo(() => ...)`), and an anonymous
  `export default` of either (shown as `default`, with `refs=?` since it has no name to
  grep for). A named function inside another one is not a symbol of its own — a
  component's handlers attribute to the component.
- Changes outside every symbol are not shown: types, imports, module-level statements,
  doc comments and attributes above a function, and code only inside anonymous callbacks
  (most of a `describe`/`it` test file).
- A wholly deleted symbol is not shown, and its removed lines can attribute to the next,
  unchanged symbol.
- When the parser hits syntax it can't read, chsig warns `parse errors in <file>; symbol
  ranges may be off` on stderr. The TypeScript grammar does not read `import('...')` types
  inside type arguments (e.g. `importOriginal<typeof import('x')>()`), nor call signatures
  in an interface or type literal separated only by newlines.
- `refs` is `git grep`-based (tracked files across the whole repo, as in the working tree),
  matching the bare name, not resolved references.
- `branch` is a crude substring count of branch tokens (`if`, `&&`, `?.`, ...) on changed
  lines, strings and comments included.
- No risk score and no safe-to-skip verdict — it only points at where to read.

The signals stay crude on purpose: making them precise means type resolution, which would
turn this into a different, heavier tool. The coarseness keeps a human in the loop.

## Tests

`cargo test` — unit tests pin the current behavior (including known defects) as a
regression anchor. I/O functions (git, disk) are left to integration.

## License

[MIT](LICENSE)
