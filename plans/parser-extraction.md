# Parser Extraction — Standalone `cypher-parser` / `gql-parser` Crate

**Status**: complete  
**Updated**: 2026-05-07

> See also the discussion notes appended at the end of this document for context on the spargebra comparison.

---

## Summary

The parser and AST layers of `rs-polygraph` have **zero coupling** to SPARQL or any downstream translation logic. Extracting them into a standalone crate would let other projects (graph analytics, linters, migration tools, alternative backends) parse openCypher and GQL queries without pulling in `spargebra` or any SPARQL machinery.

**Verdict: Yes, extraction is a good idea.** The architecture already enforces a clean boundary; the work is mostly mechanical.

---

## Dependency Analysis

### What would move to the new crate

| Component | External deps | Internal deps | SPARQL coupled? |
|-----------|--------------|---------------|-----------------|
| `grammars/cypher.pest` | — | — | No |
| `grammars/gql.pest` | — | — | No |
| `src/ast/cypher.rs` | none | none | No |
| `src/ast/gql.rs` | none | `ast::cypher::Clause` | No |
| `src/ast/mod.rs` | none | re-exports | No |
| `src/parser/cypher.rs` | `pest`, `pest_derive` | `ast::cypher`, `error` | No |
| `src/parser/gql.rs` | `pest`, `pest_derive` | `ast::cypher`, `ast::gql`, `error` | No |
| `src/parser/mod.rs` | none | re-exports | No |
| `src/error.rs` (subset) | `thiserror` | none | No (see §Error Split) |

### What stays in `polygraph`

| Component | Key deps |
|-----------|----------|
| `translator/` | `spargebra`, AST (read-only) |
| `rdf_mapping/` | `spargebra` |
| `result_mapping/` | — |
| `sparql_engine/` | — |
| `lib.rs` (transpiler API) | new parser crate + `spargebra` |

### Dependency graph (post-extraction)

```
  ┌──────────────────────────────────────┐
  │  opencypher-parser  (new crate)      │
  │  ┌──────────┐  ┌──────────────────┐  │
  │  │ ast/     │  │ parser/          │  │
  │  │ cypher   │◄─┤ cypher.rs        │  │
  │  │ gql      │  │ gql.rs           │  │
  │  └──────────┘  └──────────────────┘  │
  │  deps: pest, pest_derive, thiserror  │
  └──────────────────┬───────────────────┘
                     │  (re-exported types)
  ┌──────────────────▼───────────────────┐
  │  polygraph  (existing crate)         │
  │  translator/ ─► spargebra            │
  │  rdf_mapping/                        │
  │  result_mapping/                     │
  │  sparql_engine/                      │
  │  deps: opencypher-parser, spargebra, ... │
  └──────────────────────────────────────┘
```

---

## Error Type Split

`PolygraphError` currently has four variants:

```rust
pub enum PolygraphError {
    Parse { span, message },         // parser-only
    UnsupportedFeature { feature },  // parser + translator
    Translation { message },         // translator-only
    ResultMapping { message },       // result-mapping-only
}
```

**Approach**: The new crate defines a `ParseError` enum with `Parse` and `UnsupportedFeature` variants. `polygraph` defines its own `PolygraphError` that wraps or re-exports `ParseError` plus the translator-specific variants. This keeps `From<ParseError> for PolygraphError` trivial and avoids breaking the public API.

```rust
// opencypher-parser/src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("Parse error at {span}: {message}")]
    Syntax { span: String, message: String },

    #[error("Unsupported feature: {feature}")]
    UnsupportedFeature { feature: String },
}
```

```rust
// polygraph/src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum PolygraphError {
    #[error(transparent)]
    Parse(#[from] opencypher_parser::ParseError),

    #[error("Translation error: {message}")]
    Translation { message: String },

    #[error("Result mapping error: {message}")]
    ResultMapping { message: String },
}
```

---

## Naming and Discoverability

### Crate name options

| Crate name | Scope | Searchable for "cypher"? | Notes |
|------------|-------|--------------------------|-------|
| `cypher-parser` | openCypher only | ✅ Yes | Cleanest signal; most likely search term |
| `opencypher-parser` | openCypher only | ✅ Yes | Mirrors the spec name exactly |
| `graph-query-parser` | Both languages | ❌ No | Too generic; won't surface in Cypher searches |
| `polygraph-parser` | Both, branded | ❌ No | Project identity, but invisible to crates.io searches for "cypher" or "gql" |

**Key finding from ecosystem research**: A search for "cypher parser" on crates.io returns 42 results; a search for "opencypher" returns 30. The most-downloaded active competitor (`drasi-query-cypher`, 3k recent downloads) is tied to Microsoft's Drasi execution engine. No standalone, backend-agnostic openCypher parser with a typed AST exists. **The name must contain "cypher" to be discovered.**

**Recommendation**: `opencypher-parser`

- The official spec name is "openCypher" — matches searches for both "cypher parser" and "opencypher"
- Clearly scoped (GQL support is implicit since GQL's AST reuses Cypher types)
- Not tied to any specific backend or project branding
- `polygraph` can still depend on it: `opencypher-parser = { path = "../opencypher-parser" }`

### crates.io metadata (Cargo.toml)

Discoverability on crates.io comes from three sources: crate name, `keywords`, and `categories`. The spec allows up to 5 keywords and maps to a fixed set of categories.

```toml
[package]
name = "opencypher-parser"
description = "Standalone openCypher and ISO GQL parser producing a typed AST. No execution engine required."

keywords = ["cypher", "opencypher", "gql", "graph", "parser"]

categories = ["parser-implementations", "database"]
```

**Why these keywords**:
- `cypher` — matches "cypher parser" search
- `opencypher` — matches "opencypher" search
- `gql` — matches ISO GQL interest
- `graph` — matches graph database tooling searches
- `parser` — matches parser-implementations category browsing

**Comparison with top results on crates.io**:

| Crate | Keywords | Why it surfaces |
|---|---|---|
| `open-cypher` (21 SLoC, abandoned) | `cypher, graph, parser, sql` | Name + keywords |
| `drasi-query-cypher` (active) | `drasi` only | Name contains "cypher" |
| `gdl` (25k downloads) | `graph, gdl, cypher` | High downloads + keywords |

`opencypher-parser` would outrank all of these on relevance for "cypher parser" due to name + full keyword coverage + active maintenance.

---

## Workspace Layout (post-extraction)

Convert the repo to a Cargo workspace:

```
rs-polygraph/
├── Cargo.toml              # [workspace] members = ["crates/*"]
├── crates/
│   ├── opencypher-parser/
│   │   ├── Cargo.toml      # pest, pest_derive, thiserror
│   │   ├── grammars/
│   │   │   ├── cypher.pest
│   │   │   └── gql.pest
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── error.rs
│   │       ├── ast/
│   │       │   ├── mod.rs
│   │       │   ├── cypher.rs
│   │       │   └── gql.rs
│   │       └── parser/
│   │           ├── mod.rs
│   │           ├── cypher.rs
│   │           └── gql.rs
│   └── polygraph/
│       ├── Cargo.toml      # opencypher-parser, spargebra, thiserror
│       └── src/
│           ├── lib.rs
│           ├── error.rs
│           ├── translator/
│           ├── rdf_mapping/
│           ├── result_mapping/
│           └── sparql_engine/
├── tests/                  # stays at workspace root
├── benches/
└── examples/
```

---

## Migration Steps

### Step 1 — Convert to Cargo workspace (non-breaking)

1. Create `crates/opencypher-parser/` directory structure.
2. Move `ast/`, `parser/`, grammar files, and the `Parse`/`UnsupportedFeature` error variants.
3. Add a root `Cargo.toml` `[workspace]` section listing both crates.
4. In `opencypher-parser/Cargo.toml`, depend on `pest`, `pest_derive`, `thiserror`.
5. In `polygraph/Cargo.toml`, add `opencypher-parser = { path = "../opencypher-parser" }`.
6. Re-export `opencypher_parser::*` from `polygraph::ast` and `polygraph::parser` so that all existing public types remain accessible at their current paths.

### Step 2 — Update imports in `polygraph`

1. Replace `crate::ast::*` → `opencypher_parser::ast::*` in `translator/`, `rdf_mapping/`, etc.
2. Replace `crate::parser::*` → `opencypher_parser::parser::*` in `lib.rs`.
3. Wrap `ParseError` into `PolygraphError` via `From` impl.

### Step 3 — Update tests

1. Parser unit tests move into `opencypher-parser`.
2. Integration / TCK tests stay in the workspace root, depending on `polygraph`.
3. Verify `cargo test --workspace` passes with zero regressions.

### Step 4 — Grammar path adjustment

`pest_derive` uses `#[grammar = "..."]` relative to `Cargo.toml`. Update the path attribute in the parser files to point to `grammars/cypher.pest` and `grammars/gql.pest` relative to the new crate root.

### Step 5 — Publish

1. Publish `opencypher-parser` to crates.io (it has no path dependencies).
2. `polygraph` depends on the published version.
3. Third-party projects can now `cargo add opencypher-parser` without pulling in `spargebra`.

---

## Risks & Mitigations

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Breaking public API paths (`polygraph::ast::CypherQuery`) | Medium | Re-export from `polygraph` so old paths still work |
| Grammar `#[grammar = "..."]` path breaks | Low | Test immediately after move; one-line fix |
| GQL's dependency on Cypher AST types forces both into one crate | N/A | Already the plan — both live in `opencypher-parser` |
| Workspace complicates CI / release process | Low | Standard Rust workspace pattern; cargo-release handles it |
| Divergent error types confuse downstream users | Low | `PolygraphError` wraps `ParseError` transparently via `#[from]` |

---

## Who Benefits

| Consumer | Use case | Needs translator? |
|----------|----------|-------------------|
| Query linters / formatters | Parse → AST → validate / pretty-print | No |
| Graph analytics tools | Parse Cypher → custom execution engine | No |
| Migration tools | Parse Cypher → emit SQL / Gremlin / other | No |
| IDE / language server | Parse for syntax highlighting, completion | No |
| This project (`polygraph`) | Parse → translate → SPARQL | Yes (uses both crates) |

---

## Effort Estimate

The refactoring is **mechanical** — no logic changes, no new code:

- **~0 lines of new logic** — only file moves and import rewrites
- **Key files to touch**: `Cargo.toml` (×3), `lib.rs` (×2), `error.rs` (×2), every `use crate::ast` / `use crate::parser` in `translator/`
- **Risk of regressions**: Low — `cargo test --workspace` catches everything

---

## Value-Add APIs for `opencypher-parser`

To be genuinely useful as a standalone crate — comparable to what `spargebra` offers for SPARQL — the extracted crate should include these additions beyond the bare parser/AST.

### Comparison with `spargebra`

`spargebra` is the closest analogue: it parses SPARQL and returns a typed algebra tree with `Display`, `Eq`, `Hash`, and an `on_in_scope_variable` visitor. It does **no constant folding or normalization** — `FILTER (1 + 1 = 2)` is stored as-is. The reason it feels "normalized" is that the SPARQL spec defines its query language directly in terms of algebra operators (`Join`, `LeftJoin`, `Filter`, `Project`…), so there is almost no distance between surface syntax and the algebra. Cypher is the opposite: its high-level syntax implies a rich structure that the translator has to unpack. That gap is what makes transformation passes valuable here.

The key difference in design philosophy: spargebra's AST is an **algebra** (semantically reduced). `opencypher-parser`'s AST is intentionally a **syntax tree** (structurally faithful to what was written). This is a feature — formatters and linters need source fidelity that a pre-normalized tree cannot provide.

### Priority-ordered additions

| Addition | Effort | Value | Notes |
|---|---|---|---|
| `Display` / pretty-printer | Medium | Highest | Enables formatters, round-trip tests, query rewriting |
| Complete `CypherVisitor` / `CypherVisitorMut` traits | Low | High | Every consumer needs tree walks; move out of translator module |
| `variables()` / `bound_variables()` / `projected_variables()` | Low | High | Linters, IDEs, query planners |
| `Eq + Hash` on all AST types | Low | Medium | Caching, deduplication; blocked by `Literal::Float` (use `OrderedFloat`) |
| Serde support (feature-gated) | Low | Medium | Already scaffolded; drop-in derives |
| Semantic validation pass | High | Medium | Variable scope check, aggregate mixing, empty patterns |
| Constant folding pass | Medium | Medium | See §Normalization below |

### Normalization and constant folding

`spargebra` does zero constant folding — this is correct behaviour for a parser crate. But that does **not** mean we should skip it. It means we should implement it as **explicit, opt-in transformation passes** rather than wiring it into `parse_cypher()`.

The model is Rust's `syn` crate: faithful syntax tree by default, separate `visit_mut` passes for rewrites. A formatter must preserve `NOT NOT x` as written; a linter might want to flag `1 + 1` as a constant. Silent eager folding in the parser would destroy source fidelity.

Passes that are **backend-agnostic** (belong in `opencypher-parser`):

```rust
// Implement as CypherVisitorMut — consumer calls explicitly
pub struct ConstantFolder;      // 1 + 1 → 2, true AND x → x
pub struct NegationNormaliser;  // NOT NOT x → x, !(a AND b) → !a OR !b
pub struct AndFlattener;        // (a AND (b AND c)) → (a AND b AND c)
```

Passes that are **backend-specific** (stay in `polygraph` / translator):
- Predicate pushdown — depends on index layout
- Join ordering — depends on cardinality estimates
- Property path merging — depends on SPARQL engine capabilities

### Visitor trait placement

The current `AstVisitor` in `src/translator/visitor.rs` covers only 5 node types and is defined inside the translator module — making it inaccessible to `polygraph-parser` consumers. On extraction:

1. Move a complete read-only `CypherVisitor<Output>` (default no-op for every node) into `opencypher-parser`.
2. Add `CypherVisitorMut` for in-place rewrites.
3. The translator's internal visitor becomes an `opencypher_parser::CypherVisitor` implementor.

---

## Full-Grammar-First Strategy

> **Status: IMPLEMENTED** (2026-04-16) — `grammars/cypher.pest` was rewritten from spec
> and the parser updated; TCK baseline restored to 1632/1748 (93.3%). See implementation
> notes at the end of this section.

### Context

The TCK compliance effort exposed a recurring pattern: a test scenario fails, we trace it to a missing grammar rule, add the rule, move on — only to hit the next gap a few hours later. Each incremental grammar addition is cheap in isolation but the cumulative cost is high: context-switches, re-running the suite, adjusting the AST, updating the translator, repeat. Many of the 3,650 TCK scenarios are blocked purely by parser gaps that could be closed in one pass if the complete grammar were in place from the start.

### Proposed approach

Instead of continuing to grow the grammar incrementally, do a **single upfront pass** to produce a complete, spec-faithful pest grammar before adding any more translator coverage.

The official openCypher ANTLR4 grammar is maintained at:

```
https://github.com/opencypher/openCypher/tree/master/grammar
```

The key file is `cypher.xml` (the canonical EBNF specification) and an auto-generated `Cypher.g4` ANTLR4 grammar derived from it. The grammar covers the full openCypher 9 surface — every clause, expression, operator, literal type, and pattern variant included in the TCK.

### Conversion: ANTLR4 → pest

ANTLR4 and pest differ in a few important ways that require manual attention during conversion; everything else is mechanical:

| Issue | ANTLR4 | pest | Resolution |
|---|---|---|---|
| Left recursion | Supported (implicit) | Forbidden | Rewrite left-recursive expression rules using Pratt parsing or iterative `(op term)*` wrappers |
| Case-insensitive keywords | `options { caseInsensitive=true; }` | Explicit `^"keyword"` or combined rules | Add `ASCII_ALPHA_UPPER \| ASCII_ALPHA_LOWER` alternatives per keyword, or use `^` prefix |
| Whitespace/comments | `WHITESPACE` channel | `WHITESPACE` rule + `_` | Declare `WHITESPACE = _{ … }` and `COMMENT = _{ … }` as silent rules |
| Lookahead predicates (`~`, `?`) | Inline predicates | `!` / `&` | Translate directly to pest prefix operators |
| Lexer fragments | Separate `fragment` rules | Inlined or named rules | Inline small fragments; name reusable ones |
| Unicode identifiers | Built-in | `\u{…}` ranges | Replicate openCypher spec's identifier Unicode ranges explicitly |

The expression precedence hierarchy in openCypher (12 levels, from `OR` down to unary) is the hardest part. The recommended approach is **pest Pratt parser** (`PrattParser` from the `pest` crate), which lets the grammar express operators and their precedence separately from the recursive descent rules — matching how the ANTLR4 grammar handles it.

### Recommended build order

1. **Clone the openCypher repo** and review `grammar/cypher.xml` (authoritative) and `grammar/Cypher.g4` (readable ANTLR4 form).
2. **Produce a complete `cypher.pest`** that parses every construct in the ANTLR4 grammar. All nodes that the translator does not yet handle can map to a single `unimplemented_clause` catch-all rule initially — the grammar must *accept* the input even if the translator returns `UnsupportedFeature`.
3. **Run the full TCK parse-only pass**: for each scenario, invoke the parser only (no translation) and confirm the grammar accepts the input. This gives a clean parser coverage baseline before any translator work.
4. **Replace the existing `grammars/cypher.pest`** with the new complete grammar, updating parser code and AST in the same PR.
5. **Continue TCK translator work** — but now every failure is a translator gap, never a parser gap.

This is a one-time investment that eliminates an entire class of future regressions and lets translator work proceed at higher velocity.

### Implementation Notes (2026-04-16)

Steps 1–4 are now complete. Key additions over the old grammar:

| Feature | Grammar rule | Parser handling |
|---|---|---|
| `UNION` / `UNION ALL` | `single_query` wrapper in `statement` | `build_statement` restructured |
| `ORDER BY/SKIP/LIMIT` inside `WITH` | `projection_body` rule | `build_projection_body` returns 5-tuple |
| Reserved words as label/type/key names | `schema_name` rule | label extraction updated to `schema_name` |
| Namespaced functions (`apoc.text.join`) | `func_name = @{ (ident ~ ".")* ~ ident }` | `name = name_pair.as_str()` |
| `COUNT(n)`, `COUNT(DISTINCT n)` | `count_expr = { COUNT ~ (count_star \| DISTINCT? expr) }` | `build_aggregate_expr` handles `count_expr` |
| `FOREACH (var IN list \| clause+)` | `foreach_clause` | → `UnsupportedFeature` |
| `ON MATCH SET` / `ON CREATE SET` in `MERGE` | `merge_action` | parsed, not yet translated |
| `CALL … YIELD *` / `YIELD items` | `yield_clause`, `yield_star`, `yield_items` | via `build_call_clause` |
| `EXISTS { … }` subquery | `exists_subquery` | → `UnsupportedFeature` |
| `REDUCE(acc = e, v IN l \| expr)` | `reduce_expr` | → `UnsupportedFeature` |
| `shortestPath`, `allShortestPaths` | `shortest_path_atom` | → `UnsupportedFeature` |
| `$param` in expressions | `parameter` atom | → `UnsupportedFeature` |
| Unary `+` | `unary_plus` in `unary_expr` | no-op, returns inner |
| `NOT IN` | `kw_NOT ~ kw_IN ~ add_sub_expr` in `comparison_suffix` | `NotIn` expression variant |

**PEG ordering lessons learned** (tricky correctness constraints):
- `full_arrow = { "<-->" }` **must** appear as the **first** alternative in `rel_pattern`; otherwise `left_arrow` (`<-`) greedily consumes 3 chars of `<-->` leaving `>` unparseable.
- `aggregate_expr` must appear **before** `function_call` in `atom`; otherwise `COUNT(*)` matches `function_call` before `aggregate_expr`.
- `comparison_suffix = { comp_op ~ comparison_expr \| … }` — the RHS must remain `comparison_expr` (not `add_sub_expr`) to preserve right-associative chained comparison `a < b < c` → `a < (b < c)`, which the translator converts to `a < b AND b < c`.
- `properties = { map_literal }` — parameters in node-predicate position (`MATCH (n $p)`) are correctly rejected as syntax errors per the TCK. Do NOT add `\| parameter` here.

### Scope

This work belongs inside the eventual `opencypher-parser` crate but does **not** require the crate extraction to happen first. It can be done in-place against the current `grammars/cypher.pest` and then the completed grammar moves to the extracted crate later.

---

## Decision

Extraction is recommended when any of these triggers occur:

1. Another project wants to use the parser independently.
2. `spargebra` or other heavy deps slow down compile times for parser-only consumers.
3. The project moves toward a crates.io publish (Phase 8 in ROADMAP).

Until then, the existing module boundary is clean enough that extraction can be deferred without accumulating technical debt. The key invariant to maintain: **parser and AST must never import from `translator`, `rdf_mapping`, or `spargebra`**.

When extraction happens, the value-add APIs above should be included in the same PR — a bare parser/AST with no `Display`, no visitor, and no `Eq+Hash` is a significantly less useful crate than one that ships all of those on day one.

### crates.io ecosystem summary (researched 2026-04-15)

Of the 42 "cypher parser" results on crates.io:
- **`open-cypher`** (3.3k downloads): abandoned 3+ years ago, 21 SLoC, no typed AST — just exposes the raw pest grammar.
- **`drasi-query-cypher`** (4.6k total, 3k recent): actively maintained by Microsoft/Drasi but inseparable from the Drasi continuous-query execution engine.
- **`sparrowdb-cypher`**, **`cypherlite-query`**, **`plexus-parser`**: all brand-new (days/weeks old) and tightly coupled to their own backends.
- **GQL (ISO/IEC 39075)**: no parser crates exist at all.

`opencypher-parser` would be the only standalone, backend-agnostic openCypher+GQL parser with a typed AST on crates.io.
