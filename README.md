# change-signals (`chsig`)

A small CLI that collects **facts** about the symbols a `git diff` changed, so a human can
decide *where to read first*. It explains nothing and judges nothing — it points.

It is **not** a static analyzer or a code-review tool: it runs no semantic analysis and
makes no judgments. It pulls coarse, heuristic signals out of a `git diff` as raw material
for a human. **v0.x** — detection is heuristic, not a parser; the output is meant to be
skimmed and falsified, not trusted.

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
cargo install --path .    # puts `chsig` on PATH
```

Needs a Rust toolchain. Run `chsig` from inside the repo you want to read. The
`chsig-prioritize` stage also needs the `claude` CLI on PATH.

## Stages

### 1. signals — `chsig`

```sh
chsig main...HEAD    # args go straight to `git diff`; run inside the target repo
chsig                # no args = working diff
```

```
src/cache/store.ts
  getOrFetch                   refs=14, branch+5, cache
  evictExpired                 refs=6, branch+1

src/db/records.ts
  loadRecord                   refs=12, db
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

- Heuristic symbol detection for Rust and TypeScript/JavaScript only; other files are skipped.
- `refs` is `git grep`-based, not resolved references.
- `branch` and the boundary tags (`db`/`cache`/`net`) are crude substring markers, not
  semantic analysis.
- Nested / multi-line constructs may attribute coarsely to the parent symbol.
- No risk score and no safe-to-skip verdict — it only points at where to read.

These are deliberate: making any of them precise means type resolution or a real parser,
which would turn this into a different, heavier tool. The coarseness keeps a human in the loop.

## Tunables

The one project-specific seam is the const block at the top of `src/main.rs`
(`MAX_DEF_DEPTH`, boundary vocabulary). It encodes the stack and shape of the repo you
point chsig at (e.g. react-query, SQL; React-style one-level hook nesting); edit it to
retarget. Kept as consts rather than an
external config file until a second repo needs different values — only then is the varying
axis known well enough to design a schema. Until then the block *is* the example.

## Tests

`cargo test` — unit tests pin the current heuristic behavior (including known
over-captures) as a regression anchor. I/O functions (git, disk) are left to integration.

## License

[MIT](LICENSE)
