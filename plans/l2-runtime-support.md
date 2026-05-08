# L2 Runtime Support — Roadmap to Full TCK Compliance

**Status**: in progress
**Updated**: 2026-06-10
**Baseline**: 3757 / 3828 scenarios pass (98.1 %), 71 failed.
**Current**: 3790 / 3828 scenarios pass (99.0 %), 38 failing — Comparison1[14] path equality + WithOrderBy1[45] list comparison sort key fixed.
**Target**: ≥ 99.3 % pass + skipped categories collapsed.

This plan describes how to close the remaining gap between the static
transpiler and the openCypher TCK by introducing **L2 (multi-phase) runtime
support**, plus targeted infrastructure work to lift the policy-skipped
scenarios.

L1/L2/L3 levels are defined in
[plans/fundamental-limitations.md](fundamental-limitations.md).
This plan focuses on L2 — runtime round-trips between transpiler and SPARQL
engine, with minor L3 extensions where they unlock disproportionate value.

---

## 1. Scope

### 1.1 Target failure population

| Bucket | Scenarios | Mitigation level |
|-------:|-----------|------------------|
| Q1 — quantifiers on runtime lists (Quantifier9-12) | 63 | L2 |
| T1 — duration arithmetic (Temporal8) | 27 | L2 + native helpers |
| LC1 — list comprehension/`properties()`/`relationships(p)` projection (List12, Graph9, Path2) | 10 | L2 |
| O1 — list/null/heterogeneous ordering (ReturnOrderBy1, WithOrderBy1) | 8 | L1 (sort key trick) — included for completeness |
| DST — Temporal10 daylight-saving (Europe/Stockholm) | 6 | L2 + tz database |
| Mrg — Merge1, Merge5, Match4[8] structural | 8 | L2 |
| A1 — heterogeneous min/max (Aggregation2) | 3 | L1 (encoded sort key) |
| Misc — Pattern1/2, Comparison1, List11, Set1[5], Match4/6, ReturnOrderBy4, With6, Graph3/4, Precedence1, Temporal2/3 | ~28 | mixed L1/L2 |
| **Total addressable** | **~153** | |

### 1.2 Skipped scenario population

| Skip reason | Count (approx) | Mitigation |
|-------------|---------------:|------------|
| `And parameters are:` (query parameters) | ~80 | runtime parameter binding |
| `And there exists a procedure …` (CALL stubs) | ~40 | procedure registry |
| `And having executed:` setup parse failures | ~28 | improved CREATE coverage |
| **Total skipped** | **148** | |

Closing both populations puts the project at **≥ 3,640 / 3,789 (96 %)** with
the irreducible Match6[14] (RDF multigraph) being the only known permanent
ceiling.

---

## 2. Architecture: `TranspileOutput::Continuation`

**Status: ✅ Infrastructure implemented.** The enum variants and runtime driver described
below are already in the codebase. The remaining work is (a) wiring the TCK and
difftest test runners to call `drive()`, and (b) implementing the first Continuation
emitters in `lqa/sparql.rs`.

Original design (for reference):

```rust
pub enum TranspileOutput {
    /// Single-phase: one SPARQL string ready to execute.
    Complete {
        sparql: String,
        schema: ProjectionSchema,
    },

    /// Multi-phase: execute `phase1`; pass result rows to `continue_fn` to
    /// obtain the next `TranspileOutput` (which itself may be a continuation,
    /// supporting N-phase pipelines).
    Continuation {
        phase1: Box<TranspileOutput>,
        continue_fn: Box<dyn FnOnce(Vec<BindingRow>) -> Result<TranspileOutput, PolygraphError> + Send>,
    },
}
```

A thin runtime driver (in a new `polygraph-runtime` crate, kept separate from
the pure transpiler so that engine integrators can omit it) executes the
continuation chain:

```rust
pub fn run<S: SparqlEngine>(engine: &S, output: TranspileOutput)
    -> Result<QueryResult, PolygraphError>
{
    match output {
        TranspileOutput::Complete { sparql, schema } => engine.query(&sparql, schema),
        TranspileOutput::Continuation { phase1, continue_fn } => {
            let phase1_rows = run(engine, *phase1)?;
            run(engine, continue_fn(phase1_rows.rows)?)
        }
    }
}
```

### 2.1 Why a runtime driver, not engine-side execution

* **Engine portability**: every SPARQL engine sees only standard SPARQL 1.1
  strings. No engine-specific extensions required.
* **Composability**: a continuation can produce another continuation,
  supporting arbitrary-depth pipelines (e.g., UNWIND of UNWIND of variable).
* **Caching**: phase1 results can be cached when the same Cypher input is
  re-run with different parameters.
* **Testability**: each phase is a pure SPARQL string; existing TCK
  infrastructure (Oxigraph in-process) drives them sequentially.

### 2.2 Phase boundary detection

A phase boundary is required wherever the transpiler currently rejects with
`UnsupportedFeature`. A new pre-pass over the Cypher AST classifies each
clause into `StaticPhase` or `RuntimePhase`:

```rust
enum PhaseKind { Static, Runtime { reason: RuntimeReason } }

enum RuntimeReason {
    UnwindOfVariable,        // UNWIND var (var is non-literal)
    QuantifierOverRuntimeList,
    ListComprehensionInProjection,
    PropertiesOfNode,
    RelationshipsOfPath,
    DurationArithmetic,
    DstAwareTemporal,
}
```

The transpiler walks clauses left-to-right. As soon as a `RuntimePhase`
clause is reached, it emits a phase-1 query that materialises the bindings
needed by the runtime clause, then the continuation closure builds phase 2
with those bindings inlined as `VALUES`.

---

## 4. Implementation Progress

### Phase L2-α: Infrastructure  ✅ DONE (2026-05-07)

`TranspileOutput::Continuation`, `runtime::SparqlExecutor`, and `runtime::drive()`
are all implemented and unit-tested. Nothing emits `Continuation` yet; both
test runners return an error when they encounter one. The next step is:

1. **TCK runner wiring** (`tests/tck/main.rs`): create an `OxigraphExecutor`
   implementing `SparqlExecutor` via the existing `OxStore` (with all custom
   functions registered). Replace the `"L2 continuation not yet supported"`
   error arm with a call to `runtime::drive(&output, &executor)` for the result rows.

2. **Difftest runner wiring** (`polygraph-difftest/src/runner.rs`): same pattern —
   wrap the Oxigraph query executor in `SparqlExecutor` and call `drive()`.

Both runners are now wired. Any Continuation emitted by the transpiler is automatically
exercised by both the TCK and difftest suites without further runner changes.

### Phase L2-γ: First Continuation emitter — list comprehension arithmetic  ✅ DONE (2026-06-09)

Pattern: `SET n.prop = [literal_list] RETURN [i IN n.prop | arithmetic_expr] AS alias`

**LQA change** (`src/lqa/sparql.rs`):
- Added `compile_output(op, base_iri) -> Result<TranspileOutput, PolygraphError>` — new L2-capable entry point.
- Added `try_list_comp_projection_continuation(op, base_iri)` — detects `Projection` where all items are `ListComprehension { list: Property(Var, key), predicate: None, projection: Some(arithmetic) }`.
- Phase 1: compiles a `SELECT ?__lc_src_{alias}` that fetches the source property values.
- Continuation closure: parses the stored list string, evaluates the arithmetic map expression in Rust (`eval_lc_map_expr`), serializes result list, emits a `VALUES` block.
- Added `parse_cypher_list` — parses `"[1, 2, 3]"` format into `CypherRtVal` list.
- Added `eval_lc_map_expr` — evaluates Cypher arithmetic on scalar values.

**`lib.rs` write path change**: When `translate_skip_writes` returns `Unsupported`, now tries `compile_output(strip_writes(&op), base_iri)` as a fallback. If that also fails, falls back to full legacy.

**TCK runner change** (`tests/tck/main.rs`): `Write + Continuation select` arm added — calls `runtime::drive()` on the Continuation and binds the result rows.

**Result**: Set1[5] (Adding a list property) now passes. 3787→3788 TCK. No regressions. Difftest 232/232.

### Phase L2-β: Duration arithmetic via custom SPARQL functions  ✅ DONE (2026-05-14)

Instead of using `TranspileOutput::Continuation` (which would require a 2-phase
round-trip), duration arithmetic was solved by registering **custom Oxigraph
SPARQL functions** — a simpler static approach that avoids L2 infrastructure:

| Function URI | Operation |
|---|---|
| `urn:polygraph:duration-add` | `dur_a + dur_b` |
| `urn:polygraph:duration-sub` | `dur_a - dur_b` |
| `urn:polygraph:duration-mul-num` | `dur * number` |
| `urn:polygraph:duration-div-num` | `dur / number` |

**LQA change** (`src/lqa/sparql.rs`): In `Expr::Add/Sub/Mul/Div`, when the
LHS is an `Expr::Property(..)`, emit an `IF(STRSTARTS(STR(?x), "P"), custom_fn(...), arithmetic)` dispatch. For `Add` with two Property operands, the existing list-concat heuristic is preserved as the outer branch, and duration-add as the middle branch.

**Arithmetic functions** (`src/translator/cypher/temporal.rs`): Four new
`pub` functions (`duration_add_str`, `duration_sub_str`, `duration_mul_num_str`,
`duration_div_num_str`) + two internal carry helpers (`ns_carry`, `min_carry`).
Key rules:
- Add/sub: carry ns→s→min→h only (no h→d or mo→y carry)
- Mul/div: full cascade via same logic as `temporal_duration_from_map`
- Sub-nanosecond fractions truncated (not rounded) to match Cypher semantics

**TCK/difftest runners**: custom functions registered via `.with_custom_function(...)` chain on `SparqlEvaluator`. Return `Literal::new_simple_literal` (not `xsd:duration`) to avoid Oxigraph normalizing 32H → 1D8H.

**Result**: 8 new TCK passes (all of Temporal8). No regressions. Difftest 232/232.

---

### Remaining 40 failures (post Phase L2-γ)

| Bucket | Count | Mitigation |
|--------|------:|-----------|
| L2-deferred (quantifiers, collect, list comprehension) | ~29 | L2-Q1/LC1 |
| L1-temporal (Temporal10 DST, Temporal2/3 constructors) | ~9 | chrono-tz + parser |
| L1-varlen (variable-length path, named paths) | ~5 | structural |
| L1-write (Merge5, Merge1) | ~4 | write clause work |
| L1-structural (properties(), UNWIND variable) | ~6 | structural |
| L1-other (Match4/5 cardinality, Comparison1, With6) | ~3 | structural |

---

## 5. Backlog

### 3.1 Q1 — Quantifiers on runtime lists  *(63 failures)*

**Pattern**:

```cypher
MATCH p = (start:S)-[*0..3]->(end)
WITH tail(nodes(p)) AS nodes
RETURN nodes, none(x IN nodes WHERE x.name = 'a') AS result
```

The `nodes` variable is bound to a runtime list (`tail(nodes(p))`); we cannot
fold the quantifier at translate time.

**L2 design**:

* **Phase 1**: `SELECT ?nodes WHERE { … materialise the list … }` returns
  one row per `nodes` value. The list is encoded as our existing
  `"[item1, item2, …]"` string.
* **Continuation**: parse each `?nodes` string back into a list of items,
  evaluate the quantifier predicate per row in pure Rust (no SPARQL needed
  for this trivial boolean), and emit:

  ```sparql
  VALUES (?nodes ?result) {
    ("[(:A {name:'a'})]" false)
    ("[]" true)
    …
  }
  ```

  as a single-binding-set Phase 2 query.

The continuation closure carries the predicate AST (`x.name = 'a'`) and a
small evaluator for our value encoding. Because the runtime list values are
already in our `"[…]"` string format, no additional parsing of node IRIs is
needed — the predicate operates on the encoded string-leaf values.

**Estimate**: +63 passes. Implementation effort: medium (1–2 weeks); shares
the value-encoding parser with §3.3 and §3.4.

---

### 3.2 T1 — Duration arithmetic on temporal values  *(27 failures)*

**Pattern**:

```cypher
RETURN date({year: 1984, month: 10, day: 11}) + duration({months: 14}) AS d
```

SPARQL 1.1 does not specify date+duration arithmetic; Oxigraph does not
implement it generically; even where it works, semantic equality on
`xsd:duration` conflicts with Cypher's structural equality (see Temporal7[6]
in current notes).

**L2 design**:

* **Phase 1**: detect the temporal arithmetic node at translate time;
  serialise both operands as untyped strings, materialise into a single-row
  result.
* **Continuation**: evaluate the arithmetic in Rust using the existing
  `temporal.rs` calendar machinery (already supports nanosecond precision
  for date/time/datetime); inject the resulting ISO-8601 string as a
  `VALUES` literal in Phase 2.

This sidesteps both the missing SPARQL feature and the semantic-equality
conflict.

**Estimate**: +27 passes. Implementation effort: small (most of the calendar
math already exists); main work is wiring the continuation pattern.

---

### 3.3 LC1 — List comprehension and complex projections  *(10 failures)*

**Pattern**:

```cypher
MATCH p = (a:Start)-[:REL*2..2]->(b)
RETURN relationships(p)
```

```cypher
MATCH (n)
RETURN [x IN nodes(p) | x.name] AS oldNames
```

```cypher
MATCH (p:Person)
RETURN properties(p) AS m
```

All three need to reify a **set of triples** into a single column value
(map literal or list of map literals).

**L2 design**:

* **Phase 1**: emit a SPARQL query that selects the underlying triples
  individually, grouped by the projection key:

  ```sparql
  SELECT ?p ?key ?val WHERE {
    ?p <base:__node> <base:__node> .
    ?p ?key ?val .
    FILTER(STRSTARTS(STR(?key), "http://tck.example.org/"))
    FILTER(?key != <base:__node>)
  }
  ```

* **Continuation**: group rows by `?p`, build the `{key: val, …}` string in
  Rust using the existing `list_elem_to_str` helper, and return as a single-
  column `VALUES` Phase 2 result.

The same machinery handles `relationships(p)` by selecting reified edges
along the path and emitting `[[:REL {prop: val}], …]` strings.

**Estimate**: +10 passes (List12 ×6, Graph9 ×2, Path2 ×2). Implementation
effort: medium; main complexity is the reverse RDF→Cypher value mapper,
which is largely already implemented in `result_mapping/`.

---

### 3.4 O1 — Ordering of lists / nulls / heterogeneous types  *(8 failures)*

**Pattern**:

```cypher
UNWIND [[], ['a'], ['a', 1], [1], [1, 'a'], [1, null]] AS lists
RETURN lists ORDER BY lists
```

SPARQL `ORDER BY` over our list-encoding strings is lexicographic; Cypher
sorts by length-then-elementwise with a type-rank ladder.

**L1 design (no L2 needed)**:

* In `translate_unwind_clause`, when the UNWIND items are themselves lists
  and there is an outer ORDER BY referencing the bound variable, emit a
  parallel `__sort_key` column in the `VALUES` block. Compute the sort key
  in Rust at translate time:

  ```text
  ""              → "0"
  ["a"]           → "1|s|a"
  ["a", 1]        → "1|s|a|i|1"
  [1, null]       → "1|i|1|n"
  ```

  Then `ORDER BY ?__sort_key, ?lists`.

* In `translate_order_by`, recognise that the sort target is a list-encoded
  string column and silently substitute `?__sort_key` for `?lists`.

**Estimate**: +8 passes. Implementation effort: small.

---

### 3.5 DST — Daylight-saving temporal arithmetic  *(6 failures)*

**Pattern** (Temporal10[8]):

```cypher
RETURN duration.inSeconds(
  datetime({year:2017, month:10, day:29, hour:0, timezone:'Europe/Stockholm'}),
  localdatetime({year:2017, month:10, day:29, hour:4})
) AS duration
```

Expected: `PT5H` (5 wall-clock hours including DST fall-back).
Actual: `PT4H` (no DST awareness).

**L2 design**:

* Add `chrono-tz` as a regular dependency.
* In the duration-between functions in `temporal.rs`, when both endpoints
  have a named timezone, look up the DST offset for each instant and apply
  it before computing the duration.
* No SPARQL-level change needed — this is pure compile-time evaluation of
  literal-argument duration functions (T1 territory). It only becomes L2
  when the endpoints come from a runtime variable, in which case use the
  same Phase 1 / continuation pattern as §3.2.

**Estimate**: +6 passes. Implementation effort: small once `chrono-tz`
is approved as a new dependency.

---

### 3.6 Mrg — MERGE structural failures  *(8 failures)*

**Symptoms** vary:
- Merge1[9]: row count mismatch — `MERGE` updates not visible to subsequent
  reads in same query (read-before-write semantics).
- Merge5[3]: `MATCH (a:A), (b:B) MERGE (a)-[r:TYPE]->(b) RETURN count(r)` —
  cartesian-product MERGE creates one edge per (a,b) pair, but query returns
  count over a single binding.
- Match4[8]: `[rs*]` with runtime list (see §1.1a in fundamental-limitations).

**L2 design** for Match4[8]: directly applies §3.1 (multi-phase execution).
Phase 1 returns the materialised `rs` list; continuation generates a fixed-
length chain UNION of length `len(rs)`.

**L2 design** for MERGE same-statement read-after-write (Merge1[9]):
* Phase 1: the existing INSERT operation.
* Continuation: re-translate the post-MERGE SELECT using the just-mutated
  store state.

This is essentially what the test runner already does (split write/read);
formalising it inside `TranspileOutput::Continuation` lets every engine
integrator benefit.

**Estimate**: +8 passes. Implementation effort: medium; reuses the
write/read split already in `tests/tck/main.rs`.

---

### 3.7 A1 — `min()`/`max()` over heterogeneous values  *(3 failures)*

**L1 design**: at translate time, wrap each input value `?x` with a
type-rank prefix:

```sparql
BIND(CONCAT(
  IF(isBlank(?x), "0_",
   IF(?x = "" || isLiteral(?x), CONCAT(type_rank(?x), "_"), "9_")),
  STR(?x)
) AS ?__x_sortable)
```

then `MIN(?__x_sortable)` / `MAX(?__x_sortable)`, then strip the prefix in
the projection. Type rank ladder matches Cypher: `null < bool < num < str <
list < map`.

**Estimate**: +3 passes. Implementation effort: small.

---

### 3.8 Miscellaneous singletons  *(~28 failures)*

Each requires individual triage. After implementing §3.1–3.7, the remaining
failures fall into these patterns:

* **Pattern1/2 ×5**: pattern comprehension in WHERE — needs a sub-SELECT
  with EXISTS on a per-row basis. L1 doable via dedicated rewrite.
* **List11[3]**: `range(start, stop, step)` with runtime variables — L2
  evaluation in continuation.
* **Set1[5]**: list comprehension on SET-tracked list — combination of
  S1+LC1 rewrites.
* **Comparison1[14]**: path equality `p1 = p2` regardless of direction —
  requires path canonicalisation (sort endpoints, normalise direction).
* **Match4[4], Match6[14]**: variable-length path on dynamically-built
  graphs — Match4[4] L2 (re-execute after CREATE), Match6[14] permanent
  multigraph limit.
* **Precedence1[26,28]**: 3VL precedence with `IN` over runtime lists —
  L2 evaluation.
* **ReturnOrderBy4[1], With6[4], Graph3[6], Graph4[5]**: each one a
  small targeted bug — fix individually.
* **Temporal2[6], Temporal3[10]**: named-timezone offset preservation
  (`+01:00[Europe/London]`) — L2 with `chrono-tz`.

**Estimate**: +25–28 passes after the major buckets land.

---

## 4. Lifting the Policy Skips  *(148 scenarios)*

### 4.1 Cypher query parameters  *(~80 scenarios)*

Currently the test harness sees `And parameters are:` and sets
`world.skip = true`.

**Design**:

* Add `params: HashMap<String, CypherValue>` to the public transpilation
  API: `Transpiler::cypher_to_sparql_with_params(&str, &Engine, &Params)`.
* Translate Cypher `$param` references either:
  * Inline as literals at translate time (preferred for static params), or
  * As SPARQL `?_param_name` variables bound via a `VALUES` clause prefix
    (preserves caching of the SPARQL string across parameter values).
* Update the TCK runner to parse the `parameters are:` table into a
  `Params` map and remove the skip.

**Estimate**: +60 to +80 passes (most parameter scenarios are otherwise
in-spec). Implementation effort: medium; touches the public API surface.

### 4.2 CALL procedure stubs  *(~40 scenarios)*

The TCK uses `CALL` to invoke standard procedures (e.g., `db.labels()`,
`apoc.create.node`, custom test fixtures).

**Design**:

* Add a `ProcedureRegistry` trait to the transpiler:

  ```rust
  pub trait ProcedureRegistry {
      fn lookup(&self, name: &str) -> Option<&dyn ProcedureSpec>;
  }

  pub trait ProcedureSpec {
      fn arg_types(&self) -> &[CypherType];
      fn yield_columns(&self) -> &[(String, CypherType)];
      fn translate(&self, args: &[Expression]) -> Result<GraphPattern, PolygraphError>;
  }
  ```

* Provide built-in implementations for `db.labels()`, `db.relationshipTypes()`,
  `db.propertyKeys()` — each becomes a small SPARQL `SELECT DISTINCT` over
  the appropriate metadata.

* TCK fixture procedures (e.g., `test.assertEqual`) become no-op stubs
  registered by the test runner.

**Estimate**: +30 to +40 passes. Implementation effort: medium; the
registry is small but each builtin needs its own SPARQL mapping.

### 4.3 Setup parse failures in `having executed:`  *(~28 scenarios)*

Some setup CREATE blocks use constructs the simple `create_to_insert_data`
helper in [tests/tck/main.rs](tests/tck/main.rs) doesn't handle yet:

* `WITH * UNWIND range(…) AS i CREATE (n {var: i})`
* `MATCH … CREATE (n)-[:T]->(m)` over the result of a range
* `FOREACH` (dropped from openCypher but still in some TCK fixtures)

**Design**:

* Replace the bespoke `create_to_insert_data` function with a routing
  through the main translator's CREATE-skip-writes path: parse the setup
  query as Cypher, translate write clauses to SPARQL Updates exactly the
  same way the executing-query handler does.
* Once the translator supports UNWIND-bound CREATE generation (covered by
  §3.6 L2 plan), these fixtures load correctly.

**Estimate**: +20 to +28 passes. Implementation effort: small (mostly
consolidation).

---

## 5. Phasing & Dependencies

The work has natural dependency chains. Order chosen to minimise rework
(value-encoding parser shared across §3.1, §3.3, §3.7).

| Phase | Buckets | Adds | Cumulative pass | Effort |
|-------|---------|------|-----------------|--------|
| L2-α | infra: `TranspileOutput::Continuation`, runtime driver, value-encoding parser | 0 | 3488 (92.1 %) | 1 week |
| L2-β | §3.1 Q1 + §3.4 O1 + §3.7 A1 (small wins on top of α) | +74 | 3562 (94.0 %) | 1–2 weeks |
| L2-γ | §3.3 LC1 + §3.5 DST + §3.6 Mrg + §3.8 (singletons batch 1) | +30 | 3592 (94.8 %) | 2 weeks |
| L2-δ | §3.2 T1 (duration runtime) + §3.8 (singletons batch 2) | +30 | 3622 (95.6 %) | 1–2 weeks |
| Skip-1 | §4.1 query parameters | +60–80 | ≈ 3700 (97.6 %) | 1–2 weeks |
| Skip-2 | §4.2 CALL procedures + §4.3 setup parser unification | +50 | ≈ 3750 (99.0 %) | 1–2 weeks |

After L2-δ + Skip-1 + Skip-2 the remaining **~39 scenarios** are split
between Match6[14] (irreducible RDF multigraph limit) and the long tail of
genuine static-transpiler bugs that fall out as side-effects of the
diagnostic improvements.

---

## 6. Engine-Integrator Impact

The new continuation API is opt-in:

* Engines that consume the existing single-string SPARQL output continue
  to work; they receive `TranspileOutput::Complete` for any query whose
  feature set they support, and a clear `RequiresRuntimeSupport { reason }`
  error for queries that need L2.
* Engines that want full TCK compliance link `polygraph-runtime` and
  invoke `runtime::run(&engine, output)` instead of `engine.query(&sparql)`.
* The Postgres extension protocol planned in
  [plans/pg-extension-protocol.md](pg-extension-protocol.md) becomes one
  specific way to elide the runtime round-trip — its custom SPARQL
  functions can replace continuations §3.1, §3.3, §3.6 with a single SPARQL
  string by pushing decomposition into the engine.

---

## 7. Out-of-Scope / Permanent Limits

* **Match6[14]** — RDF multigraph parallel edges. No L2/L3 mitigation
  exists within a triple-based store. See
  [plans/fundamental-limitations.md §2](fundamental-limitations.md).
* **Performance** — multi-phase queries inherently incur N round-trips.
  Engines wanting single-round-trip execution must adopt the L3 extensions
  in [plans/pg-extension-protocol.md](pg-extension-protocol.md).
* **Streaming** — the runtime driver materialises Phase 1 results in
  memory. Streaming continuations are a future refinement.

---

## 8. Acceptance Criteria

* `cargo test --test tck` passes ≥ 99 % of scenarios.
* `polygraph-runtime` crate published with examples for Oxigraph and an
  HTTP SPARQL backend.
* `TranspileOutput::Complete` is the return for every Cypher query that
  does not require runtime data; only L2-only constructs return
  `Continuation`.
* Skipped scenario count drops from 148 to ≤ 10 (with each remaining
  skip individually justified).
* No regression in the static-transpiler-only test target
  (`cargo test --test tck -- --tag '@static-only'`).
