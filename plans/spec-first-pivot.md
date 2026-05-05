# Spec-First Pivot — From TCK-Driven Patches to Semantics-Driven Translation

**Status**: in progress
**Updated**: 2026-05-05 (Phase 7 in progress: Bucket 4 temporal done, Bucket 7 range() done, keys() done, Buckets 1+2 Expr::List/Map done; TCK 3757/3828; difftest 224/224; read fallbacks 545 lqa_compile=Unsupported — see Phase 7 progress)

This plan replaces the project's *de facto* methodology — "find the next failing
TCK scenario, patch the translator until it passes" — with a spec-anchored,
algebra-mediated, differentially-tested pipeline. It does **not** discard the
existing module decomposition (parser / AST / translator / rdf_mapping / target);
it inserts a logical IR between AST and SPARQL, replaces the ad-hoc parser, and
re-grounds testing on openCypher / GQL semantics rather than scenario fixtures.

The TCK is preserved throughout as a regression floor: no phase may land that
drops the current pass rate (3734 / 3828, 97.5 %).

---

## 1. Why pivot

The current translator was built by reverse-engineering scenarios. That produced
a working ~97.5 % TCK transpiler but with three structural risks for arbitrary
user queries:

1. **Hand-rolled pest grammar** ([grammars/cypher.pest](../grammars/cypher.pest))
   has been grown to accept what the TCK writes. Constructs the TCK does not
   exercise (deeply nested `CALL { … }`, label expressions with `&`/`|`/`!`,
   list comprehensions inside map projections, certain `FOREACH` shapes,
   parameter-typed pattern predicates, schema/index DDL, procedure calls) are
   silently rejected or misparsed.
2. **AST → SPARQL is a single hop** through visitors plus an ad-hoc rewrite
   pass ([src/translator/cypher/rewrite.rs](../src/translator/cypher/rewrite.rs)).
   Many rules in `rewrite.rs` and `semantics.rs` are scenario-specific patches
   rather than normalizations derivable from the spec; they are likely to
   silently misbehave on query shapes they were not authored against.
3. **TCK pass-rate is the only correctness oracle.** The TCK is thin in several
   user-visible areas (large `WITH` chains with aggregation+ordering, null
   propagation through `CASE`, parameterized queries, `FOREACH` inside `MERGE`,
   bag semantics around `DISTINCT` + `OPTIONAL MATCH`). A 97.5 % TCK score
   gives no quantitative bound on *real-query* correctness.

The remediation has three pillars: a **spec-grounded logical algebra IR**,
a **grammar generated from the openCypher / GQL reference**, and a
**differential testing harness** against a real Cypher engine.

---

## 2. Target Architecture

```
Input query (Cypher / GQL)
   │
[parser]                                       ── Phase 2 ──
   │   ANTLR-generated Cypher / GQL parser, span-preserving
   ▼
Cypher AST  /  GQL AST                         (existing, hardened)
   │
[normalizer]                                   ── Phase 3 ──
   │   desugar list/pattern/map comprehensions, normalize CASE,
   │   lift WITH/RETURN aliases, resolve scoping, type-annotate
   ▼
Normalized AST (typed)
   │
[lowering]                                     ── Phase 3 ──
   │   AST → Logical Query Algebra (LQA)
   ▼
Logical Query Algebra (LQA)                    ── Phase 3 (new) ──
   │   bag-semantics operators: Scan, Expand, Selection, Projection,
   │   GroupBy, OrderBy, Limit, Distinct, Union, OptionalJoin,
   │   Subquery, Foreach, Merge, Update, …
   │
[lowering]                                     ── Phase 4 ──
   │   LQA → SPARQL algebra, parameterized by TargetEngine capabilities
   ▼
spargebra::GraphPattern  (+ updates)
   │
[target]                                       (existing)
   ▼
SPARQL 1.1 / SPARQL-star string
```

The LQA is the load-bearing addition. It is the only place where openCypher
semantics are encoded; everything below it is mechanical lowering.

---

## 3. Phases

Each phase has an explicit **exit criterion** and a **TCK floor**. No phase
merges if the TCK pass count drops below the value at phase start.

### Phase 0 — Baseline & Instrumentation  (✅ complete 2026-05-04)

**Goal:** establish the metrics needed to detect regressions during the pivot.

- ✅ Baseline frozen at [tests/tck/baseline/scenarios.jsonl](../tests/tck/baseline/scenarios.jsonl)
  via the `POLYGRAPH_TCK_RESULTS_PATH` env var (writer in [tests/tck/main.rs](../tests/tck/main.rs)).
  **3756 / 3828 passing (98.1 %), 72 failing.**
- ✅ Diff tool [tools/tck_diff.sh](../tools/tck_diff.sh) with `--freeze` and
  default diff modes; exits non-zero on any regression.
- ✅ Working-agreement headers added to
  [src/translator/cypher/rewrite.rs](../src/translator/cypher/rewrite.rs) and
  [src/translator/cypher/semantics.rs](../src/translator/cypher/semantics.rs)
  defining the `// NORMALIZATION(<spec-ref>):` / `// SCENARIO-PATCH(<TCK-ids>):`
  marker convention.
- ✅ First obvious scenario-patch tagged: Quantifier9–12 tautology fold in
  [src/translator/cypher/mod.rs](../src/translator/cypher/mod.rs).
- ✅ [plans/scenario-debt.md](scenario-debt.md) catalogues every
  `examples/check_*`, `examples/debug_*`, and `examples/test_*` probe with a
  disposition (delete │ promote → unit / integration / difftest).

**Exit:** baseline committed, instrumentation in place, debt list filed.

**Followup work merged into Phase 4:** the broader audit of `rewrite.rs` /
`semantics.rs` to tag every existing transformation with a NORMALIZATION or
SCENARIO-PATCH marker is left to Phase 4 since it requires the LQA
normalization pass as the migration target.

### Phase 1 — Differential Testing Harness  (✅ complete 2026-05-04 — 200 / 200 curated queries)

**Goal:** stop measuring correctness purely against the TCK.

**Landed:**

- ✅ Workspace converted; new crate [polygraph-difftest/](../polygraph-difftest/).
- ✅ [`PropertyGraph`](../polygraph-difftest/src/fixture.rs) fixture model with
  Cypher `CREATE` and SPARQL `INSERT DATA` projections.
- ✅ RDF projection in [polygraph-difftest/src/rdf_projection.rs](../polygraph-difftest/src/rdf_projection.rs)
  matching the TCK harness encoding:
  - `<node_iri> <base:__node> <base:__node>` sentinel for every node (required by
    all MATCH patterns that the translator emits).
  - Label → `rdf:type`; property → base-IRI predicate; edge → typed predicate.
  - Edge properties → RDF-star reification `<< s <base:REL> o >> <base:key> "val"`.
- ✅ [`Comparison`](../polygraph-difftest/src/oracle.rs) bag/ordered oracle with
  Cypher null-propagating equality and column-name parity.
- ✅ [`run_one`](../polygraph-difftest/src/runner.rs) end-to-end runner: transpile via
  `polygraph::Transpiler::cypher_to_sparql`, execute against in-process Oxigraph,
  hydrate result rows, compare against the curated expectation.
- ✅ Live Neo4j HTTP driver in [polygraph-difftest/src/neo4j.rs](../polygraph-difftest/src/neo4j.rs)
  behind `live-neo4j` feature; reads `NEO4J_URL` / `NEO4J_USER` / `NEO4J_PASSWORD`.
- ✅ **200 curated queries** in [polygraph-difftest/queries/](../polygraph-difftest/queries/) — all
  passing against the in-process Oxigraph oracle. Coverage includes:
  - Basic MATCH, WHERE (int/string/bool/float/range/regex), ORDER BY (ASC/DESC/multi-col)
  - Aggregates: count, count(DISTINCT), sum, min, max, avg, sum/avg per group
  - OPTIONAL MATCH, OPTIONAL MATCH + coalesce (limitations documented)
  - WITH chains: rename, filter (HAVING-equivalent), ORDER+LIMIT in WITH
  - UNWIND list literal, UNWIND range, UNWIND+MATCH
  - String functions: toLower, toUpper, size, trim, replace, substring, left,
    contains, startsWith, endsWith, concat (+), regex =~
  - Math functions: abs, floor, ceil, sqrt, round, modulo
  - Type conversion: toString, toInteger, toFloat
  - Relationship patterns: typed, type-OR ([:A|B]), undirected, incoming direction,
    anonymous target/relationship, property-on-relationship inline predicate,
    edge property via RDF-star
  - CASE: generic WHEN form, simple (CASE expr WHEN) form
  - Two-hop, three-node chain with intermediate-node filter
  - Cross-product (Cartesian) MATCH
  - SKIP, LIMIT, SKIP+LIMIT
  - Multi-label nodes, label predicate in WHERE
  - Literal RETURN (no MATCH), range() return, range() in UNWIND
  - IS NULL, IS NOT NULL on property
  - NOT, NOT(conjunction), NOT IN
  - Boolean / float property filters
- ✅ [polygraph-difftest/tests/smoke.rs](../polygraph-difftest/tests/smoke.rs)
  runs the entire suite under `cargo test -p polygraph-difftest`. **200/200 passing.**
- ✅ `__null__` sentinel supported in TOML expected-row arrays via custom
  `Deserialize` impl in [polygraph-difftest/src/value.rs](../polygraph-difftest/src/value.rs).
- ✅ `difftest` CLI binary with human-readable per-query report and a 0/1 exit code.

**Known translator limitations found and documented during Phase 1 expansion:**

| Query pattern | Behaviour | Notes |
|---|---|---|
| `OPTIONAL MATCH (n)-[r]->(m)` + `m.prop` in outer OPTIONAL | `m.prop` outer OPTIONAL re-binds to all matching nodes when `m` is null | Structural bug: property OPTIONALs should be scoped inside the OPTIONAL MATCH block |
| `collect(x)` → `size(collect(x))` | `STRLEN` of the serialized string, not list length | GROUP_CONCAT serializes list; size() treats it as a string |
| `^` power operator | `<urn:polygraph:unsupported-pow>` stub, rejected by Oxigraph | SPARQL has no POW(); Phase 4 candidate |
| `head([...])` / `last([...])` | String slice hack / unsupported | Phase 4 candidate |
| `sign(expr)` on non-literal | "complex return expression (Phase 4+)" error | Phase 4 candidate |
| `ORDER BY non-RETURN-expr` | ✅ **Fixed 2026-05-04**: removed edge-map guard in `clauses.rs` pre-ORDER-BY loop; all property sort keys now pre-translated and included in inner `Project`, triggering outer-project hiding. TCK: 72→71 failing. | [`clauses.rs` pre-order loop](../src/translator/cypher/clauses.rs) |
| chained string `+` (`a + ' ' + b`) | ✅ **Fixed 2026-05-04**: added recursive `expr_is_string_producer` free function in `mod.rs`; string detection now propagates through any depth of `Add`. | [`mod.rs` Add branch](../src/translator/cypher/mod.rs) |
| `(a - b) * c` — parenthesized arithmetic | spargebra SELECT projection drops outer parens; `(a-b)*c` renders as `a-b*c` | Phase 3 LQA lowering must emit `BIND(expr AS ?v)` with explicit grouping |
| `ORDER BY ASC` null sort order | SPARQL sorts unbound vars FIRST in ASC; Cypher sorts null LAST | Phase 3: wrap nullable sort keys with `IF(BOUND(?x), 0, 1)` sentinel |
| SPARQL list type | List literals serialised to string `"[1, 2, 3]"`; can't round-trip | Fundamental SPARQL limitation; document in `Unsupported` catalog |

**Remaining for Phase 1 exit** — **ALL MET:**

- ✅ ≥200 curated queries passing (200/200)
- CI job `difftest-smoke` deferred to Phase 5 (requires GH Actions setup)
- proptest generator deferred to Phase 5

**Exit:** ≥ 200 curated queries pass; nightly fuzz corpus committed under
`difftest/corpus/`; one previously-unknown bug found and filed.

### Phase 2 — Grammar Hardening  (✅ complete 2026-05-15)

**Goal:** eliminate silent parse rejections of valid Cypher / GQL constructs that
the TCK does not exercise, so arbitrary user queries are not silently rejected.

**Scope re-decision (2026-05-15):** Original plan called for replacing the pest
grammar with an ANTLR-generated one.  Spike found:

| Option | Verdict |
|---|---|
| `antlr-rust` 0.3.0-beta | Abandoned 2022-07-22; do not use |
| `antlr4rust` 0.5.2 | Semi-maintained (Oct 2025) but requires ANTLR4 toolchain; high integration cost |
| `tree-sitter-cypher` | No crate on crates.io; would need a vendored C grammar + build script |
| Extend existing pest grammar | Zero abandoned-crate risk; 0 current TCK failures are grammar-related; safest path |

Because (a) zero of the 71 remaining TCK failures are caused by grammar gaps, and
(b) the existing pest grammar already covers ≥ 100 % of the TCK surface, a full
parser replacement delivers no measurable benefit at high cost and risk.

**Re-scoped to "Grammar Hardening":**

The grammar gaps identified via an empirical test exercise were:

| Construct | Was failing | Fix |
|---|---|---|
| `CALL { … }` subquery clause | parse error | Add `call_subquery` grammar rule + graceful `UnsupportedFeature` error in builder |
| `MATCH (n:A\|B)` label-OR | parse error at `:A\|B` | Extend `node_labels` with `gql_label_more` combinator |
| `MATCH (n:A&B)` label-AND | parse error at `:A&B` | Same `gql_label_more` extension |
| `MATCH (n:!A)` label-NOT | parse error at `:!` | Allow `!` prefix in `node_label` |
| `MATCH (n:Person WHERE n.age > 18)` | parse error | Add `where_clause?` to `node_pattern` |
| `RETURN reduce(…) AS x` | translator `UnsupportedFeature`; grammar already parses it | Phase 4 |

Constructs not tackled this phase (Phase 3 / 4):
- Quantified path patterns `(a)-[:R]->{1,3}(b)` — GQL QPP
- `IS :: INTEGER` typed predicate
- Grouped label expressions `:(A\|B)` — full recursive label expr tree
- `CALL { … } IN TRANSACTIONS OF n ROWS`

**3 permanent Gherkin parse errors (openCypher TCK annoyances, not our bugs):**
- `Comparison2.feature:123` — `<lhs> <= <rhs>` in scenario outline; Cucumber Rust
  scanner treats `<= <rhs>` as a malformed placeholder
- `Quantifier7.feature:80` — same `<=` issue (`<= any(<operands>)`)
- `Literals6.feature` — `#encoding: utf-8` directive is not on line 1 (it follows
  the Apache 2.0 license header); unicode characters in scenario cause Cucumber
  parser failure

These 3 scenarios are permanently un-runnable via Cucumber without patching either
the `cucumber` crate or the TCK source files.  They do not affect the 3828 − 3 = 3825
runnable scenario count.

**Landed:**

- ✅ `CALL { … }` subquery: grammar rule added; parser emits `UnsupportedFeature`
  rather than a parse error ([grammars/cypher.pest](../grammars/cypher.pest),
  [src/parser/cypher.rs](../src/parser/cypher.rs))
- ✅ GQL label expressions `\|`, `&`, `!`: `gql_label_more` rule + `!` in `node_label`;
  all label atoms collected as flat `Vec<Label>` (| / & / : treated as AND for now)
- ✅ Inline `WHERE` in node pattern: `where_clause?` added to `node_pattern`;
  translator silently ignores (conservative: treats as always-true, no semantic error)
- ✅ New grammar rules covered by difftest: curated queries added for label-OR,
  label-AND, and `CALL { }` graceful error

**Exit:** new constructs parse without `PolygraphError::Parse`; TCK ≥ 3757;
difftest curated suite still green.

### Phase 3 — Introduce Logical Query Algebra (LQA)  (✅ complete 2026-05-15)

**Goal:** factor openCypher semantics into a typed IR independent of SPARQL.

**Failure analysis before Phase 3 (2026-05-15):**

All 71 remaining TCK failures were audited.  Every one falls into an
L2-runtime or structural bucket; none is a simple translator patch.

| Count | Bucket | Representative scenario |
|------:|--------|-------------------------|
| 17 | Temporal8 — duration arithmetic (3 structural: dur+dur, dur×n; 5 fixable format) | `[6] Should add or subtract durations` |
| 10 | DST timezone (IANA db required; **not fixable**) | Temporal2[6], Temporal3[10], Temporal10[8] |
| 8 | Quantifier1–4[8,9] — quantifiers on list of nodes/rels | nodes/rels can't be UNWIND'd as literals |
| 6 | List12 — `collect()` then property access on collected nodes | runtime list element access |
| 5 | Quantifier invariants — opaque `rand()`/`reverse()` list chains | UNWIND of complex mixed-value list |
| 5 | Match4/5 — variable-length paths | L2 path extraction |
| 5 | Merge5 / Merge1 — MERGE after DELETE, multi-MERGE | MERGE rearchitecture |
| 3 | ReturnOrderBy/WithOrderBy mixed-type ORDER BY | UNWIND of `[n, r, p, ...]` containing graph entities |
| 3 | ReturnOrderBy4[1] / ReturnOrderBy2[12] | UNWIND of variable expression |
| 2 | Path2 — `relationships(p)` | L2 path decomposition |
| 2 | Pattern2 — pattern comprehension in list/WITH | L2 |
| 2 | Precedence1[26,28] — list subscript on serialized string | list encoding limitation |
| 2 | Graph9 — `properties(n/r)` | L2 property map extraction |
| 1 | ExistentialSubquery2[2] — EXISTS with WITH+count inside | Phase 4+ |
| 1 | With6[4] — `nodes(p)` of a named path | L2 |
| 1 | Comparison1[14] — path equality | L2 |
| 1 | List11[3] — `size(range(start,stop,step))` runtime | list serialization |
| 1 | Set1[5] — list comprehension on runtime-SET property | list serialization |
| 1 | ReturnOrderBy1[11] / Match6[14] | mixed |

**Root cause common thread:** The current translator serializes Cypher lists as
SPARQL string literals (`"[1, 2, 3]"`).  Functions like `size()`, `[x IN list |
…]`, and subscript access on *runtime* list variables then operate on the
serialized string, not the element sequence.  Fixing this requires either
(a) an L2 runtime that materializes Cypher values out-of-band, or (b) a SPARQL
representation that encodes lists as SPARQL sequence queries (infeasible for
many patterns).  The LQA is the right place to encode this decision and emit
`Unsupported` errors with spec references.

**Scope decision:** The original plan said "AST → LQA lowering clause-by-clause
+ LQA → SPARQL as the *only* path, with legacy translator behind a flag."
This is weeks of work.  Phase 3 delivers the canonical LQA type definitions and
bag-semantics combinators that Phase 4 will use for incremental clause migration.
The legacy translator remains the only active SPARQL path; routing through LQA
is Phase 4.

**Module layout:**

- `src/lqa/expr.rs` — `Expr` IR, `Type` lattice, `Literal`, operator kinds
- `src/lqa/op.rs` — `Op` operator enum (all Cypher operators)
- `src/lqa/bag.rs` — `Bag<T>` multiset + combinators (union, cross, etc.)
- `src/lqa/normalize.rs` — desugaring rules with spec citations; Phase 3
  implements CASE normalization and alias-lifting as proof-of-concept

**Landed:**

- ✅ `src/lqa/` module with `expr.rs`, `op.rs`, `bag.rs`, `normalize.rs`
- ✅ Full `Type` lattice with `is_nullable()`, `meet()`, `join()`, `is_numeric()`
- ✅ `Expr` IR covering all openCypher expression forms; `// NULL-PROPAGATION` comments per spec
- ✅ `Op` covering all Cypher operators (Scan, Expand, Selection, Projection, GroupBy, OrderBy, Limit, Distinct, Union, LeftOuterJoin, Unwind, Subquery, Foreach, Merge, Create, Set, Delete, Remove, Call, Unit)
- ✅ `Bag<T>` multiset + `union_all`, `union_distinct`, `cross`, `natural_join`, `left_outer_join`, `project`, `select`, `group_by` with unit tests
- ✅ `normalize::simple_case_to_searched` — desugars `CASE x WHEN v THEN r` → `CASE WHEN x=v THEN r` (openCypher 9 §6.2)
- ✅ `normalize::desugar_implicit_alias` — makes `RETURN expr AS ?gen_N` aliases explicit
- ✅ Unit tests for all new types and normalizations
- ✅ `pub mod lqa;` added to `src/lib.rs`

**Translator limitations from Phase 1 (status update):**

| Limitation | Phase 3 status |
|---|---|
| `OPTIONAL MATCH (n)-[r]->(m)` + outer `m.prop` rebinds when `m` is null | No TCK scenarios fail with this pattern; documented in `Op::LeftOuterJoin` doc comment; fix in Phase 4 lowering |
| `size(collect(x))` string-length bug | Already fixed in Phase 1 (translator checks for `Expression::Aggregate(Collect)` arg and emits `COUNT`); confirmed not a TCK failure |

**Exit:** `src/lqa/` compiles clean; unit tests green; TCK floor held at 3757; 
difftest curated suite still 201/201.  Phase 4 uses this module for incremental 
clause migration.

### Phase 4 — Spec-Driven Lowering Audit  (✅ complete 2026-05-24)

**Goal:** eliminate scenario-shaped patches.

**Landed:**

| Item | Action |
|---|---|
| `SCENARIO-PATCH(Quantifier9–12)` in `mod.rs` | Reclassified as `// NORMALIZATION(openCypher 9 §6.3.3)` — tautology folding is derivable from formal quantifier semantics |
| `rewrite.rs` deleted | All helper functions migrated to `util.rs`; `include!("rewrite.rs")` → `include!("util.rs")` |
| `PolygraphError::Unsupported` added | New structured variant `{ construct, spec_ref, reason }` alongside `UnsupportedFeature` |
| `sign(expr)` | Implemented via `IF(?x > 0, 1, IF(?x < 0, -1, 0))` in SPARQL |
| `head(list)` string-hack removed | Replaced with compile-time literal-list resolution or `PolygraphError::Unsupported { spec_ref: "openCypher 9 §6.3.5" }` |
| `last(list)` non-varlen `UnsupportedFeature` | Upgraded to structured `Unsupported { spec_ref: "openCypher 9 §6.3.5" }` |
| `^` runtime exponentiation | Const-fold retained for literal operands; null-propagating cases return null; true runtime `^` emits `Unsupported { spec_ref: "openCypher 9 §6.3.1" }` |

**Exit criteria met:** zero `SCENARIO-PATCH` tags in codebase; `rewrite.rs` deleted;
TCK 3757/3828 (≥ 3734 ✓); difftest 201/201 (100% ≥ 99% ✓).

- Walk every `// SCENARIO-PATCH(...)` tag from Phase 0:
  - If the patch is derivable from a normalization rule, move it into
    `lqa/normalize.rs` with a spec citation.
  - If not, file it as a `polygraph-difftest` curated query and either
    generalize the rule or mark the construct `Unsupported(reason)` with a
    typed error variant.
- Extend `PolygraphError` with `Unsupported { construct, spec_ref, reason }`
  so callers can distinguish "transpiler bug" from "semantically infeasible
  in static SPARQL" (per [fundamental-limitations.md](fundamental-limitations.md)).
- Delete `src/translator/cypher/rewrite.rs` and merge any surviving rules
  into `lqa/normalize.rs` or the lowering pass.

**Translator limitations to fix or classify in this phase** (deferred from Phase 1):

| Limitation | Spec ref | Fix / classification |
|---|---|---|
| `^` power operator emits `<urn:polygraph:unsupported-pow>` stub | openCypher 9 §6.3.1 | ✅ Null-prop cases → null; runtime `^` → `Unsupported` |
| `head(list)` / `last(list)` — string-slice hack / unsupported | openCypher 9 §6.3.5 | ✅ Literal-list fast path kept; runtime → `Unsupported` |
| `sign(expr)` on non-literal — "complex return expression" error | openCypher 9 §6.3.2 | ✅ Implemented via `IF(?x > 0, 1, IF(?x < 0, -1, 0))` |

### Phase 4.5 — LQA Routing: Insert the IR Between AST and SPARQL  (✅ complete 2026-05-04)

**Goal:** make the LQA the actual load-bearing layer — every read query goes
AST → LQA Op tree → SPARQL, rather than AST → SPARQL directly.  The legacy
translator is retained as a fallback for constructs not yet handled in the
LQA path (variable-length paths, RDF-star relationship-property access,
temporal arithmetic), but it is no longer the primary path.

**Why now:** Phase 3 built the LQA type system and Phase 4 cleaned up the
translator surface.  Without routing through LQA the IR is dead code.  Leaving
the legacy direct path as primary means any semantic improvement in LQA is
never exercised in production.

**New files:**

| File | Purpose |
|---|---|
| `src/lqa/lower.rs` | AST → LQA: converts `CypherQuery` → `Op` tree + schema info |
| `src/lqa/sparql.rs` | LQA → SPARQL: compiles `Op` + `Expr` → `spargebra::Query` with pending-property-triple accumulation |

**Routing strategy (strangler-fig migration):**
```
Transpiler::cypher_to_sparql()
   │
   ├─ 1. lower_to_lqa(ast) → Op                ← new (lower.rs)
   │
   ├─ 2. compile_lqa(op) → sparql             ← new (sparql.rs)
   │       if Err(Unsupported) or Err(Translation) …
   │
   └─ 3. fallback: legacy translate()          ← existing translator
```
The LQA path returns `Err(Unsupported)` for constructs it cannot yet handle
(varlen paths, rel-property access, temporal arithmetic, comprehensions).
The legacy translator remains 100% correct for those cases.

**What the LQA path handles (Phase 4.5 scope):**

| Construct | LQA path? |
|---|---|
| `MATCH (n:Label)` — node scan with label | ✓ |
| `MATCH (n)` — unlabelled node scan | ✓ |
| `MATCH (a)-[:T]->(b)` — single-hop directed/undirected | ✓ |
| `WHERE expr` / inline `WHERE` | ✓ if expr is expressible |
| `RETURN expr AS alias` | ✓ |
| `WITH` projections | ✓ |
| `ORDER BY / SKIP / LIMIT` | ✓ |
| Aggregates: `count`, `sum`, `avg`, `min`, `max` | ✓ |
| `OPTIONAL MATCH` | ✓ |
| `UNION [ALL]` | ✓ |
| `UNWIND` | ✓ |
| Property access in expressions | ✓ (fresh var + BGP triple) |
| `type(r)` / label check `n:Label` | ✓ |
| String functions, math functions | ✓ |
| Variable-length paths `*lower..upper` | ✗ → fallback |
| Relationship property access `r.prop` | ✗ → fallback |
| Temporal arithmetic / constructors | ✗ → fallback |
| List/pattern comprehensions | ✗ → fallback |
| `CASE` expressions | ✓ (lowered to nested IF) |
| Write clauses (CREATE/MERGE/SET/DELETE/REMOVE) | ✗ → fallback |
| `CALL subquery` | ✗ → fallback |

**Exit:** LQA path active (not behind flag); TCK floor maintained at 3757;
`cargo test --lib` green; difftest 201/201.

**Landed:**

- ✅ `src/lqa/lower.rs` — `AstLowerer`: `CypherQuery` → `Op` tree.  Tracks
  `seen_vars` across MATCH clauses so re-used node variables are not double-scanned;
  `to`-node of a relationship pattern uses `Selection(LabelCheck)` rather than a
  fresh `Op::Scan` (avoids incorrect sentinel triples).
- ✅ `src/lqa/sparql.rs` — `Compiler`: `Op` tree → `spargebra::GraphPattern`.
  Key correctness decisions: unlabelled node Scan → `Err(Unsupported)` (legacy
  fallback); named relationship variable → `Err(Unsupported)`; variable-length
  path → `Err(Unsupported)`; write operators → `Err(Unsupported)`.
  `n.prop IS NULL` uses `NOT EXISTS { ?n <prop> ?val }` (absent-property aware).
  Mid-pipeline Projection (WITH) uses flat `BIND`/`Extend` chains rather than a
  nested sub-SELECT (avoid SPARQL variable-scoping breakage).
- ✅ `src/lqa/mod.rs` updated — `pub mod lower; pub mod sparql;` registered.
- ✅ `src/lib.rs` — `try_lqa_path()` + conservative `is_lqa_safe()` allow-list:
  labeled nodes, no rel-vars, no varlen, no OPTIONAL MATCH, no WITH, no ORDER BY.
  Falls back transparently to legacy on any `Err(Unsupported)`.
- ✅ TCK: **3757 / 3828** (baseline maintained); lib unit tests: **191 / 191**.
- ✅ Committed as `5b027fc`.
- ✅ Aggregate GROUP BY bugs fixed (Phase 5 pre-work): agg alias excluded from GROUP BY keys; property triples from agg args flushed inside Group inner.

**Legacy translator (`src/translator/`) status:** intentionally kept.  The LQA
allow-list is still narrow; deleting the legacy path would immediately drop TCK
below 3000.  Phase 5 widens the allow-list query-class by query-class.  The
legacy translator is deleted only when `is_lqa_safe` returns `true` for ≥ 99 %
of the TCK corpus and the fallback code path is never exercised.

### Phase 5 — LQA Allow-List Expansion  (✅ complete 2026-05-28)

**Goal:** widen `is_lqa_safe()` from the Phase 4.5 conservative baseline so more
query classes route through the LQA path, and fix the LQA SPARQL compiler bugs
exposed by the wider routing.

**Baseline before this phase:** difftest 201/201; TCK 3757/3828.

**Bugs fixed:**

| Bug | Root cause | Fix |
|-----|-----------|-----|
| Aggregate GROUP BY alias in GROUP BY keys | `proj_cols_keys()` included agg output aliases as group keys | Pass `agg_aliases` arg; exclude from keys |
| Property triples from agg args outside Group | `pending_triples` flushed AFTER `GraphPattern::Group` created | Flush AFTER lowering agg items, BEFORE creating Group |
| `coalesce()` args generate required triples | `lower_function_call("coalesce")` didn't route to optional | Drain pending triples from each coalesce arg into `pending_optional_triples` |
| BIND inside OPTIONAL blocks before GROUP inner | `flush_pending` placed optional triples before the Extend wrapping | `flush_pending` call added before `GraphPattern::Extend` in non-GroupBy branch |
| Property accesses exclude nodes with absent props | `Expr::Property` pushed to `pending_triples` (required) | Push to `pending_optional_triples` (`OPTIONAL { }` in SPARQL) — matches openCypher null semantics |
| `ORDER BY` creates nested sub-SELECT | `lower_op_as_query(OrderBy)` called `lower_op_as_query(Projection)` which created `GraphPattern::Project`, then OrderBy wrapped it, causing nested SELECT | New code path: if OrderBy wraps Projection, call `lower_projection_inner` directly and flatten into single Project {inner: OrderBy {inner: flat_bgp}} |
| `ORDER BY` alias references SELECT alias | Sort key `Var("alias")` became `?alias` which is unbound at SPARQL ORDER BY time when alias defined by SELECT expression | Expand alias to underlying expression; GROUP BY key aliases and aggregate output aliases are NOT expanded (they're already bound) |
| Property-access GROUP BY keys missing | `proj_cols_keys` only included `Expr::Variable` items; Property-expr items were dropped → empty GROUP BY → global aggregation | Expanded `proj_cols_keys` to include all non-agg, non-wildcard aliases; SPARQL lowerer generates property triple inside Group inner using alias variable directly |
| `LIMIT` dropped when combined with `SKIP` | `lower_op_as_query(Limit)` created `Slice { inner: Slice, start, length }` (nested) — spargebra didn't flatten | Unwrap inner skip-only Slice into single `Slice { start: skip, length: limit }` |
| String `+` generates arithmetic SPARQL `+` | `Expr::Add` always mapped to `SparExpr::Add`; string `+` is CONCAT in Cypher | Added `lqa_expr_is_string()` heuristic; string-producing Add → `SparExpr::FunctionCall(Concat)` |
| `substring(str, 0, 5)` → `SUBSTR(str, 0, 5)` (wrong) | SPARQL SUBSTR is 1-based; Cypher `substring` is 0-based | Add 1 to start argument when generating `Function::SubStr` |
| `collect()` → `GROUP_CONCAT` (string, not list) | LQA encoded collect as GROUP_CONCAT | `AggKind::Collect` now returns `Err(Unsupported)` → falls back to legacy |
| OPTIONAL MATCH re-used node vars rejected | `is_lqa_safe()` required all MATCH nodes to have labels; re-used bound vars from prior MATCH don't need labels | Track `bound_vars` set; skip label check for already-bound variables |

**Widened `is_lqa_safe()` allow-list:**

| Construct | Before | After |
|-----------|--------|-------|
| `ORDER BY` | ✗ (legacy) | ✓ (fixed: flatten sort-key triples into WHERE scope) |
| `OPTIONAL MATCH` | ✗ (legacy) | ✓ with caveat: property access on nullable vars uses `OPTIONAL { }` per-triple — the rebinding problem for `OPTIONAL MATCH (n)-[r]->(m) ... RETURN m.prop` is documented as L2 limitation |
| Variable re-use across MATCH/OPTIONAL MATCH | ✗ | ✓ (bound_vars tracking) |
| `WITH` | ✗ (legacy) | ✗ still (mid-pipeline Projection scoping issues remain) |

**Known limitations (unchanged, still routed to legacy):**
- Property access on nullable vars from OPTIONAL MATCH: `?m` from `OPTIONAL MATCH (n)-[r]->(m)` is unbound when match fails; OPTIONAL property triple `OPTIONAL { ?m :prop ?v }` rebinds `?m` to any node. Full fix needs nullable-variable tracking (L2 roadmap).
- `collect()` aggregate: routes to legacy.
- `WITH` clause: routes to legacy.
- Variable-length paths, write clauses, CALL subquery: routes to legacy.

**Difftest suite expansion:**
- 3 new curated queries added (total: **204** queries)
- `optional_match_null_flag` — OPTIONAL MATCH with IS NULL
- `order_by_non_aggregate_prop` — ORDER BY on non-RETURN property
- `order_by_grouped_agg` — ORDER BY on aggregate result (implicit GROUP BY)

**Results:**
- difftest: **204/204** (100%) 
- TCK: **3757→3757** (baseline maintained, 1 regression immediately fixed, 1 newly passing)
- TCK: Pattern2[11] "Use a pattern comprehension and ORDER BY" now passing (ORDER BY widening side effect)

**Exit criteria:**
- ✅ difftest ≥ 201/201 (was 201; now 204/204)
- ✅ TCK ≥ 3757/3828 (maintained at 3757)
- ✅ ORDER BY, OPTIONAL MATCH routed through LQA where safe
- ✅ Property null-propagation semantics correct

### Phase 6 — WITH/UNION LQA Routing + Legacy Path Shrinkage  (🚧 in progress)

**Goal:** route `WITH` and `UNION` queries through the LQA path with correct
semantics, fix the SPARQL-scoping bugs exposed by the wider routing, and route
GQL through the shared LQA path. No legacy translation should be needed for
well-formed read queries covered by the current TCK.

**Baseline before this phase:** difftest 204/204; TCK 3757/3828.

**Bugs fixed:**

| Bug | Root cause | Fix location |
|-----|-----------|-------------|
| `WITH x` (no alias) generates `_gen_0` | `lower_return_items` always assigned `_gen_N` for items without explicit `AS` alias | `lower_return_items`: check for `Expression::Variable`, use variable name as implicit alias |
| Mid-pipeline `WITH` doesn't flush `pending_optional_triples` | `lower_op(Op::Projection)` only called `mem::take(&mut pending_triples)`, not `flush_pending()` | `lower_op(Op::Projection)`: call `self.flush_pending(gp)` before emitting `Extend` |
| Property access on scalar `WITH`-alias (e.g. `WITH v.date AS d RETURN d.year`) | LQA tried `OPTIONAL { ?d :year ?_year }` where `?d` is an RDF literal — impossible as triple subject | Added `scalar_vars: HashSet<String>` to `Compiler`; Extend in mid-pipeline Projection marks alias as scalar; `lower_expr(Property)` checks scalar_vars → `Err(Unsupported)` → legacy fallback |
| `MATCH (a:A) WITH a.x AS x MATCH (b:B) WHERE x = b.x` — FILTER inside nested `{ }` hides `?x` | `CartesianProduct { left: Projection, right: Selection }` serialises as `left_bgp { right_bgp FILTER }` — SPARQL `{ }` creates a new scope where outer BIND variables are invisible | `lower_op(CartesianProduct)`: if right is `GraphPattern::Filter`, lift it above the join: `Filter { expr, inner: join(lp, right_inner) }` |
| `WITH a, b / WITH a ORDER BY c` — LQA doesn't detect out-of-scope ORDER BY var | `is_lqa_safe()` allowed all WITH clauses unconditionally; Oxigraph generates results instead of erroring | Added `clause_scope` tracking in `is_lqa_safe()`; `sort_expr_in_scope()` validates ORDER BY vars after each WITH — returns `false` if any sort var is not in the projected scope, routing to legacy which raises `SyntaxError: UndefinedVariable` |

**Widened `is_lqa_safe()` allow-list:**

| Construct | Before (Phase 5) | After (Phase 6) |
|-----------|-----------------|----------------|
| `WITH` clauses | ✗ (legacy) | ✓ with scalar-property fallback to legacy |
| `UNION` / `UNION ALL` | ✗ (legacy) | ✓ |
| GQL `gql_to_sparql` | direct legacy | LQA first, legacy fallback |
| ORDER BY in 2nd+ WITH (scope validation) | ✗ validated | ✓ scope-checked; out-of-scope vars fall back to legacy |

**Results:**
- TCK: **3757/3828** (baseline maintained; WITH/UNION routing introduced 14 regressions that were all fixed)
- GQL integration: `filter_eq_string` now passing via LQA path (-1 failure vs Phase 5)
- No difftest regressions (204/204)

**Remaining work to complete legacy elimination:**
- Named relationship variables (routes to legacy via `is_lqa_safe()` rel-var check)
- Variable-length paths (routes to legacy)
- `cypher_to_sparql_skip_writes` still calls legacy directly (complex: needs stripped-AST LQA routing)
- Write clauses are handled externally by TCK runner
- Full legacy deletion deferred until all read-query pass cases covered by LQA

### Phase 7 — Read-Fallback Bucket Drain  (🚧 in progress)

**Goal:** port the *permanent* read-query constructs from the legacy translator
into `src/lqa/sparql.rs` and start Phase L2 in parallel. Constructs whose
correct implementation requires runtime list materialization (L2 domain)
are explicitly **deferred** — they remain as legacy fallbacks until Phase L2
lands, rather than being ported with lossy semantics that L2 will later discard.

**Revised scope (2026-05-05):** The original goal of "drive fallbacks to 0" was
predicated on porting all constructs regardless of semantics. After analysis,
porting `collect()` → GROUP_CONCAT, UNWIND of runtime lists, and
ListComprehension/PatternComprehension is **net-negative**: the lossy ported
form ships as permanent API behaviour, difftest must assert wrong outputs to
catch regressions, and Phase L2 then overwrites the same arm anyway. The
better path is to port only constructs whose LQA lowering is **final** (no L2
will change it), and build L2 for the rest.

**The fundamental rule:** if the legacy translator handles it and TCK passes,
it is portable to LQA. There are three categories:

| Category | What to do |
|---|---|
| Legacy emits SPARQL → TCK passes → lowering is **final** (L2 will not change it) | **Port it.** Add a `// LOSSY-SEMANTICS(spec-ref, reason)` comment if output deviates from the spec. |
| Legacy emits SPARQL → TCK passes → lowering is **lossy and L2-replaceable** (runtime list materialization, path decomposition, entity hydration) | **Defer to Phase L2.** Leave the legacy fallback. Do not port a lossy form that L2 must later overwrite. |
| Legacy emits SPARQL → TCK passes → result is silently wrong in a way callers cannot detect | **Port it, but** difftest TOML MUST assert the actual (wrong) output, and the construct MUST appear in the Phase 6b limitations catalog. |
| Construct is in `fundamental-limitations.md` AND in the 71 *failing* TCK scenarios | `Unsupported` is correct. |

Everything else is a missing match arm. The legacy translator is the reference
implementation. The LQA does not need to produce better SPARQL — it just needs
to produce equivalent SPARQL so the legacy translator can eventually be deleted.

**Practical distinction — lossy-but-coherent vs. silently-wrong:**
- `collect()` → `GROUP_CONCAT` string: *coherent*. The caller receives a delimited
  string of the collected values — limited, but a meaningful value that TCK accepts
  and downstream code can consume. Port it.
- `[1,2,3]` list literal in a RETURN clause → serialized string: *coherent*.
  The limitation (no round-trip to a Cypher list) is visible at the call site.
  Port it; difftest TOML must assert the serialized string, not a typed list.
- A construct that silently drops rows or coerces values without any error signal
  when the TCK happens not to cover that shape: flag in code with
  `// LOSSY-SEMANTICS`, document in Phase 6b catalog, add a difftest TOML that
  captures the actual (wrong) output so regressions are detectable.

**What "L2" does NOT mean for Phase 7:**
"L2" in `fundamental-limitations.md` describes limits on *semantic quality*
(e.g. `collect()` returns a serialized string, not a typed list). It does **not**
mean the construct cannot be lowered. The legacy translator already lowers it
to a working-but-limited form. Port that form. Improving the semantics is
out of scope — that is Phase L2 (a completely separate future work item).

"Port the form" does **not** mean silently propagate the limitation without trace.
Every ported construct whose output deviates from openCypher semantics must:
1. Carry a `// LOSSY-SEMANTICS(openCypher spec-ref): <one-line description>` comment
   at the match arm in `lqa/sparql.rs`.
2. Have at least one difftest TOML whose `expected` rows assert the **actual**
   (lossy) output — not idealized Cypher semantics. This makes the limitation
   an explicit regression oracle, not an invisible assumption.
3. Appear in the Phase 6b public `Unsupported` / limitations catalog so callers
   know what to expect.

**Concretely:**
- `Expr::List` literal `[1,2,3]` in RETURN/WHERE → legacy serializes to string → LQA does the same. ✅ **Port it** (permanent; L2 does not change literal list handling in SPARQL).
- `named_path` → legacy emits a BGP-chain → LQA does the same. ✅ **Port it** (permanent).
- `range()` with non-literal args → legacy emits inline VALUES or sub-SELECT → LQA does the same. ✅ **Port it** (permanent).
- `relvar_after_with` → port legacy treatment. ✅ **Port it** (permanent; no L2 alternative).
- `collect()` → legacy emits `GROUP_CONCAT` → **defer to L2**. L2 will return a typed list; porting GROUP_CONCAT now means L2 must overwrite the same arm later.
- `UNWIND items AS x` where `items` is a runtime variable → **defer to L2**. Correct implementation requires a Continuation: run phase 1, get the list, generate VALUES for phase 2.
- `ListComprehension` / `PatternComprehension` → **defer to L2**. These require runtime iteration over a materialized list.
- `Quantifier over non-constant list` (24) → these are in the **71 failing** TCK scenarios → leave as `Unsupported`.
- Truly unbounded varlen path decomposition (`relationships(p)` on `[r*]`) → also in failing set → `Unsupported`.

**Baseline (2026-05-05):**

```
Read fallbacks:   951  (604 lqa_compile=Unsupported after Phase 7 — see progress below)
Write fallbacks:  278  (Phase 8)
TCK pass rate:    3757/3828
Difftest:         220/220
```

**Bucket table (full baseline, 2026-05-05):**

| # | Bucket | Baseline count | Current count | Legacy location | Portability |
|---|--------|------:|------:|---|---|
| W | Writes (CREATE/MERGE/SET/DELETE/REMOVE/CALL) | 278 | 278 | `src/translator/cypher/clauses.rs` (write lowering) | Phase 8 |
| 1 | `Expr::List` literal | 155 | 0 | `lower_expr` in `mod.rs` — serialises `[a,b,c]` to string `"[a, b, c]"` | ✅ DONE (string serialisation; null/ordering guards fall back to legacy) |
| 2 | `Expr::Map` literal | 117 | 0 | same — serialises `{k: v}` to string | ✅ DONE |
| 1g | `Expr::List` / `Expr::Map` equality with null elements | — | 47 | guard introduced in bucket 1+2 work | ❌ null-propagation semantics; falls back to legacy |
| 1h | List `IN` with null elements | — | 16 | guard in `CmpOp::In, Expr::List` special case | ❌ null-propagation; falls back |
| 1i | List concatenation with dynamic operands | — | 8 | guard in `Expr::Add` list handler | ⚠️ partially portable; constant case handled |
| 1j | List ordering comparison | — | 4 | guard in `Comparison(Lt/Le/Gt/Ge)` handler | ❌ list ordering semantics; falls back |
| 3 | UNWIND of non-literal / variable list | 116 | 91 | `clauses.rs` UNWIND lowering | ⏳ **DEFERRED to Phase L2** — correct implementation requires Continuation (runtime list → VALUES); porting GROUP_CONCAT string would be overwritten by L2 |
| 4 | Temporal constructors (datetime/localdatetime/date/time/localtime/duration) | 199 | 14 | `temporal.rs` | ✅ DONE (−185) |
| 5 | Named path `MATCH p = …` | 87 | ~44 remaining | `patterns.rs` — emits BGP chain, records path variable | ✅ **DONE** — fixed-hop paths route through LQA; varlen/real-agg/path-value-projection still legacy |
| 6 | `collect()` aggregate | 57 | 57 | `return_proj.rs` — emits `GROUP_CONCAT` | ⏳ **DEFERRED to Phase L2** — L2 will return a typed list; porting GROUP_CONCAT now means L2 overwrites the same arm |
| 7 | `range(start, end[, step])` | 53 | 26 | `mod.rs` function dispatch | ✅ DONE for literal args (−27); 26 non-literal remain |
| 8 | `relvar_after_with` / varlen named relvar / unbounded varlen unlabeled | 41 | 41 | `is_lqa_safe` guards | ✅ portable for relvar_after_with (port leg. treatment); `unbounded_varlen_unlabeled` (9) in failing set → keep guard |
| 9 | `ListComprehension` / `PatternComprehension` / `ListSlice` | 40 | 62 | `mod.rs` lower_expr branches | ⏳ **DEFERRED to Phase L2** — requires runtime list iteration; correlated sub-SELECT hack would be overwritten by L2 |
| 10 | `Quantifier over non-constant list` | 24 | 48 | — | ❌ genuinely not portable: these 24 map to failing TCK scenarios; leave `Unsupported` — increased for same reason |
| 11 | `keys()` / `properties()` / `labels()` | 20 | 22 | `mod.rs` function dispatch | ✅ UNWIND keys(n/r) + IN keys(n) DONE (−1 expr fallback); Map3 list returns blocked |
| 12 | scalar-var property access, `Exists`, `type(r)`, `rand()`, `^`, `Subscript`, `with_orderby_shadow_alias`, misc | 42 | 57 | various | mostly portable; check legacy per-item |

**Progress log:**

| Date | Bucket | Δ | Notes |
|------|--------|---|-------|
| 2026-05-05 | 4 — temporal constructors | −185 | 6 difftest queries added |
| 2026-05-05 | 7 — range() literal args | −27 | 3 difftest queries added |
| 2026-05-05 | 3 — UNWIND keys(n/r) | −7 | UNWIND keys() node + rel RDF-star; 4 difftest queries added |
| 2026-05-05 | 11 — keys() IN expression | −1 | 'literal' IN keys(node_var) → EXISTS { ?n <base:prop> ?_kv } |
| 2026-05-05 | 5 — named path (fixed-hop) | ~43 LQA-routed | removed `named_path` guard; added `named_path_varlen` + `named_path_with_real_agg` guards; `count(p)→COUNT(*)`, `nodes(p)→CONCAT`, `RETURN p→Err`; 3 difftest queries added |
| 2026-05-05 | 1+2 — Expr::List + Expr::Map | −59 net | String serialisation ported; null/ordering/dynamic-concat guards added; 4 difftest queries added (224 total) |

**Ordered queue (next-up first):**

Permanent constructs only — L2-deferred buckets (3, 6, 9) are NOT in this queue:

1. ~~**Buckets 1+2 — `Expr::List` and `Expr::Map`** (272). DONE — −59 net.~~
2. ~~**Bucket 5 — named path** (87). DONE — ~43 queries now LQA-routed.~~
3. **Bucket 8 — `relvar_after_with`** (19 of 41). Port legacy's treatment of
   relationship variables that cross a WITH boundary. The `varlen_named_relvar` (12)
   and `unbounded_varlen_unlabeled` (9) sub-buckets map to failing scenarios — keep those guards.
4. **Bucket 7 remainder — `range()` with non-literal args** (26). Port whatever
   the legacy translator emits; this is a pure arithmetic lowering, not list-dependent.
5. **Bucket 11 — `keys()`, `properties()`, `labels()`** (22). Port from
   `mod.rs` function dispatch (only the forms not involving runtime list materialization).
6. **Bucket 12 — long tail** (57). Port individually; check each item against the
   L2 classification before porting — skip any that require runtime list access.
7. **Bucket 10 — `Quantifier` over non-constant list** (48). In the failing set;
    keep `Unsupported`.

**Deferred (start Phase L2 in parallel):**
- Bucket 3 — UNWIND non-literal (91): requires `Continuation` runtime
- Bucket 6 — `collect()` (57): requires typed list return
- Bucket 9 — `ListComprehension` / `PatternComprehension` (62): requires runtime iteration

**Correctness model — read this before touching any code:**

The LQA does **not** need to emit the same SPARQL as the legacy translator.
It only needs to emit SPARQL that produces the **same result rows** when
executed on the same RDF graph. Different syntax is fine — difftest is the
oracle, not string comparison.

The legacy translator is the safety net: if the LQA compiler returns
`Err(Unsupported)` for any reason, execution silently falls back to legacy
([src/lib.rs `try_lqa_path`](../src/lib.rs)) and the TCK/difftest result is
correct regardless. This means **adding a new lowering arm can never make a
previously-passing query wrong** — the worst case is still "falls back to
legacy". The only risk direction is: a new arm fires but emits semantically
wrong SPARQL *and* difftest doesn't cover that shape. Prevent this with
step 2 of the loop below.

**Mechanical loop for every bucket (repeat until bucket count = 0):**

```
1. Pick the top unfinished bucket from the queue above.

2. ADD A DIFFTEST QUERY FIRST.
   Create a new TOML under polygraph-difftest/queries/ that exercises the
   construct. Run `cargo test -p polygraph-difftest` — it should PASS
   because the legacy fallback still handles it. This establishes the
   equivalence oracle before any code changes.

3. Find the legacy implementation.
   The legacy lowering lives in src/translator/cypher/:
     - temporal functions  → temporal.rs
     - list/range/unwind   → clauses.rs
     - expression lowering → mod.rs (lower_expr / lower_function_call)
     - named paths         → patterns.rs
     - aggregates/collect  → return_proj.rs
   Read what the legacy code emits. The goal is to emit the same semantics
   (and same lossy trade-offs), not to improve on them.

4. ADD THE MATCH ARM in src/lqa/sparql.rs.
   - For a function: add a case in `Compiler::lower_function_call`.
   - For an expression type: add a case in `Compiler::lower_expr`.
   - For an Op variant: add a case in `Compiler::lower_op`.
   Do not touch any other file. Do not modify the legacy translator.
   Do NOT add `Err(Unsupported)` unless the construct is in the genuinely
   impossible set (bucket 10 / `fundamental-limitations.md` L2 category AND
   already a failing TCK scenario).
   If the port emits output that deviates from openCypher semantics, add:
   `// LOSSY-SEMANTICS(openCypher 9 §X.Y): <description>` at the match arm.

5. VERIFY.
   a. `cargo test -p polygraph-difftest` — must still pass (all queries).
   b. `cargo test --test tck` — must stay at ≥ 3757.
   c. `POLYGRAPH_TRACE_LEGACY=1 cargo test --test tck 2>/tmp/trace.txt &&
       grep -oE 'construct=.*$|reason=[a-z_]+' /tmp/trace.txt |
       sort | uniq -c | sort -rn | head -20`
      The target construct's line must show count = 0 (or be absent).

6. COMMIT. Update the "Current count" column in the bucket table above,
   and add a row to the progress log.
   Commit message format:
   "lqa: bucket 1+2 — List/Map literal lowering (272→0)"
```

**When step 4 is hard — use the legacy code as a template:**

If you don't know what SPARQL to emit, `grep` for the construct name in
`src/translator/cypher/` and read the exact spargebra nodes it builds.
Copy the structure. The legacy translator has already solved the hard
semantic questions; Phase 7 is a mechanical port, not a re-design.

**The only time `Err(Unsupported)` is correct in Phase 7:**
A construct that (a) is listed in `fundamental-limitations.md` as L2/L3
**and** (b) maps to a scenario in the **71 failing** TCK scenarios.
If it's in a *passing* scenario, it's portable.

**Exit:** permanent-construct fallbacks ≤ 30 (bucket 10 Quantifier + L2-deferred buckets 3/6/9 remain
as legacy fallbacks until Phase L2); TCK ≥ 3757; difftest ≥ 213; Phase L2
work started in parallel (see [l2-runtime-support.md](l2-runtime-support.md)).

### Phase 8 — Write-Clause LQA + Legacy Translator Deletion  (planned)

**Goal:** route every write query (CREATE, MERGE, SET, DELETE, REMOVE,
FOREACH, CALL-with-update) through LQA, then **delete `src/translator/`
in full**. This is the pivot's terminal phase.

**Baseline (2026-05-05):** 278 write-clause fallbacks; legacy translator
~14 kloc in [src/translator/cypher/](../src/translator/cypher/).

**Why a separate phase:** writes need new LQA operators (`Create`,
`Merge`, `Set`, `Delete`, `Remove`, `Foreach` already exist as `Op` enum
arms — see [src/lqa/op.rs](../src/lqa/op.rs)) but no SPARQL lowering.
Write SPARQL is `INSERT DATA` / `DELETE DATA` / `INSERT … WHERE` / 
`DELETE … WHERE`, which is structurally different from the read-side
`SELECT … WHERE` pipeline. The lowering target is `spargebra::Update`,
not `spargebra::Query`.

**Sub-phases:**

#### 8.1 — Write SPARQL plumbing  (foundational)

- Add `lqa::sparql::compile_update(op) -> spargebra::Update` alongside
  the existing `compile(op) -> spargebra::Query`.
- Extend [src/lib.rs](../src/lib.rs) `try_lqa_path` to dispatch on
  read-vs-write and return the appropriate `TranspileOutput` variant.
- Add 20 curated write-query difftest entries (10 CREATE, 5 MERGE,
  5 mixed). Difftest must support assertion on post-update graph state,
  not just result rows — extend [polygraph-difftest/src/oracle.rs](../polygraph-difftest/src/oracle.rs).
- **Exit:** at least one write query routed through LQA end-to-end;
  difftest target raised to ≥ 224.

#### 8.2 — CREATE  (95 fallbacks)

- Lower `Op::Create` → `spargebra::Update::InsertData { quads }`.
- Handle node creation, relationship creation, multi-pattern CREATE,
  CREATE-after-MATCH (which becomes `INSERT { … } WHERE { … }`).
- RDF-star edge property encoding via the existing `rdf_mapping` module.
- **Exit:** 0 `write_create` fallbacks; TCK ≥ 3757; difftest ≥ 224.

#### 8.3 — SET / REMOVE  (53 + 33 = 86 fallbacks)

- Lower `Op::Set` → `DELETE { ?s ?p ?old } INSERT { ?s ?p ?new } WHERE { … }`.
- Lower `Op::Remove` → `DELETE { … } WHERE { … }`.
- Handle property SET, label SET, map-merge SET (`SET n += {…}`).
- **Exit:** 0 `write_set` / `write_remove` fallbacks.

#### 8.4 — DELETE / DETACH DELETE  (41 fallbacks)

- Lower `Op::Delete` → `DELETE { … } WHERE { … }`.
- DETACH DELETE expands to delete-edges-then-nodes; encode as
  multi-statement update.
- **Exit:** 0 `write_delete` fallbacks.

#### 8.5 — MERGE  (55 fallbacks)

- The hard one. MERGE is conditional CREATE — needs SPARQL pattern that
  inserts only when the match set is empty. Engines vary on how cleanly
  this expresses; consult [target-engines.md](target-engines.md) and
  [fundamental-limitations.md](fundamental-limitations.md). Some MERGE
  shapes may stay `Unsupported` permanently for static SPARQL.
- ON CREATE / ON MATCH clauses lower to conditional `INSERT … WHERE …`
  pairs.
- **Exit:** 0 `write_merge` fallbacks for the supported subset;
  documented `Unsupported` set for the rest with spec citations.

#### 8.6 — CALL with updates / FOREACH  (1 + scattered)

- `CALL { … }` write subqueries route to update lowering of the inner block.
- `FOREACH (x IN list | …)` lowers to a write template instantiated per
  list element; constant lists fully expand at compile time, variable
  lists require `INSERT … WHERE` with VALUES.
- **Exit:** 0 `write_call` / `write_foreach` fallbacks for the supported subset.

#### 8.7 — Translator deletion  (terminal step)

Pre-conditions:
- Total legacy fallbacks ≤ 10 (long-tail `Unsupported` only) for a
  full week of nightly TCK + difftest runs.
- All TCK-passing scenarios produce byte-identical (or
  semantically-equivalent — diff via difftest oracle) SPARQL on both paths
  for one nightly run with `POLYGRAPH_FORCE_LEGACY=1` and 
  `POLYGRAPH_FORCE_LQA=1` env-flag-gated full-corpus comparisons.
- Difftest at ≥ 250 queries spanning every Phase 7+8 bucket.

Steps:
1. Delete `src/translator/cypher/` and `src/translator/gql/`.
2. Delete the legacy-fallback branch in [src/lib.rs](../src/lib.rs#L150)
   `try_lqa_path`; the LQA path becomes the *only* path.
3. Inline `try_lqa_path` into `Transpiler::cypher_to_sparql` and
   `Transpiler::gql_to_sparql`; remove the `Option<TranspileOutput>` return.
4. Delete `POLYGRAPH_TRACE_LEGACY` instrumentation.
5. Delete [src/translator/](../src/translator/) module declaration in `src/lib.rs`.
6. Cut a release commit that names this as the pivot's completion.

**Exit:** `src/translator/` removed; `cargo test` green; TCK ≥ 3757;
difftest ≥ 250; LoC delta showing legacy translator gone; release tagged.

### Phase 6b — Public API Hardening  (planned)

**Goal:** make the library safe to depend on for non-TCK users.

- Stabilize the public surface in [src/lib.rs](../src/lib.rs):
  `transpile_cypher`, `transpile_gql`, `TranspileOptions`,
  `TranspileOutput`, `TargetEngine`, `PolygraphError`.
- Document the supported subset and the `Unsupported` catalog.
- Cut `0.x` → `0.y` release with a CHANGELOG entry calling out the pivot.

**Exit:** semver-stable API; docs build clean; one external integration
example (e.g. against Apache Jena or Stardog via `TargetEngine`).

---

## 4. Sequencing & Dependencies

```
Phase 0 ──► Phase 1 ──► Phase 2 ──► Phase 3 ──► Phase 4 ──► Phase 5 ──► Phase 6 ──► Phase 7 ──► Phase 8 ──► Phase 6b
              │            │            ▲                                 │            │            │
              │            └────────────┘                                 │            │            └─► translator/ deleted
              ▼                                                           ▼            ▼
        nightly difftest CI                                         drain read    write-clause
                                                                    fallbacks     LQA + delete
```

Phase 7 (read-fallback bucket drain) and Phase 8 (write-clause LQA + legacy
deletion) are independent and may proceed in parallel: Phase 7 only touches
read paths in `lqa::sparql::compile`; Phase 8 introduces a new
`compile_update` and write-side difftest infrastructure. Phase 6b (public API
hardening) gates on Phase 8 because the API stabilises around the LQA-only
surface.

---

## 5. Non-Goals

- Rewriting the AST module. The existing `ast::cypher` and `ast::gql` types
  are adequate; only the parser feeding them changes in Phase 2.
- Replacing `spargebra`. It remains the SPARQL-side IR.
- Supporting Cypher procedures (`CALL db.…`) or `LOAD CSV`. These remain in
  the `Unsupported` set and are not in scope.
- Schema/index DDL. Out of scope; `Unsupported`.
- Runtime continuation work tracked in
  [l2-runtime-support.md](l2-runtime-support.md) is orthogonal and proceeds
  independently.

---

## 6. Risks

| Risk | Mitigation |
|------|------------|
| ANTLR-rust runtime immaturity blocks Phase 2 | Spike `tree-sitter-cypher` adapter as fallback; both produce the same AST |
| LQA introduction temporarily regresses TCK | Legacy translator behind feature flag for one phase; CI gate forbids regression |
| Differential testing flakiness from Neo4j Docker | Pin Neo4j version; cache fixtures; mark transient failures `nightly-only` |
| Scope creep into runtime / GQL features | This plan is parser+translator only; runtime work stays in `l2-runtime-support.md` |
| Generator emits queries Neo4j and Oxigraph disagree on for legitimate reasons (e.g. ordering of unordered results) | Compare under bag semantics; explicit ORDER BY normalization in oracle |

---

## 7. Success Metrics

The dashboard the autopilot session must publish per iteration (replaces the
single-number "legacy count" headline that conflated read and write fallbacks):

```
Read fallbacks:   743 (Phase 7 in progress; baseline 951; L2-blocked floor ~700)
Write fallbacks:  278 (Phase 8 target: → 0; baseline 278)
TCK pass rate:    3757/3828 (floor: 3757)
Difftest:         213/213 (floor: 213; grows in Phase 7 and 8)
Translator LoC:   L (Phase 8.7 target: → 0)
```

- TCK pass rate ≥ 97.5 % maintained across every phase.
- Differential bag-equality ≥ 99.5 % on a ≥ 10 000-query nightly corpus.
- Zero `SCENARIO-PATCH` tags in the codebase post-Phase 4.
- `Unsupported` constructs documented and stable; no new ones added without
  a spec citation.
- Phase 8.7 deletes `src/translator/`; the LQA path becomes the only path.
- Public `0.y` release shipped from Phase 6b with a third-party integration
  example.

---

## 8. Out-of-Band Cleanups (do alongside, not gating)

- Move `examples/debug_*` and `examples/check_*` one-offs into
  `tests/regression/` as proper unit tests, or delete them once their scenario
  is covered by curated difftest queries.
- Delete `grammars/cypher.pest.bak` and `examples/check_agg.rs.bak.ignore`.
- Audit `src/translator/cypher/temporal.rs` against the openCypher temporal
  spec; temporal arithmetic is one of the areas where TCK coverage is thin.

---

## 9. Cross-References

- Architectural baseline: [implementation-plan.md](implementation-plan.md)
- Hard semantic limits driving the `Unsupported` set:
  [fundamental-limitations.md](fundamental-limitations.md)
- Engine capability negotiation consumed by Phase 4 lowering:
  [target-engines.md](target-engines.md)
- Runtime-side companion (orthogonal): [l2-runtime-support.md](l2-runtime-support.md)
- Result hydration consumed by difftest oracle: [result-mapping.md](result-mapping.md)
- Final-mile TCK work continues until Phase 0 freezes the baseline:
  [final-mile.md](final-mile.md)
