# TCK Failure Reference

**Updated**: 2026-05-07  
**Baseline**: 3787 / 3828 passing (98.9 %), **41 failing**.  
**Legacy fallbacks**: ~647 scenario executions still route through the legacy translator — goal is zero (see Phase 8.7).  
**Target**: ≥ 3790 (≥ 99 %) — see [plans/l2-runtime-support.md](plans/l2-runtime-support.md).

This file is the authoritative, searchable reference for every currently-failing
TCK scenario. Organised by failure bucket; each entry records the feature file
link, failing scenario numbers, root cause, mitigation level, effort estimate,
and cross-references to the design documents.

L-levels are defined in [plans/fundamental-limitations.md](plans/fundamental-limitations.md):

| Level | Meaning |
|-------|---------|
| **L1** | Fixable within the static, single-round-trip transpiler model |
| **L2** | Requires multi-phase (Continuation) runtime execution |
| **L3** | Permanently infeasible with a static SPARQL transpiler |

---

## Bucket Q — Quantifiers on runtime lists  *(10 failures)*

**L-level**: L2  
**Design doc**: [plans/l2-runtime-support.md §3.1](plans/l2-runtime-support.md)  
**Effort**: medium (1–2 weeks)

The `none`/`single`/`any`/`all` quantifiers are applied to a list whose
contents are not known at transpile time — it comes from a graph node/relationship
property, a `collect()`, or a randomly-shuffled `rand()`/`reverse()` chain.
The LQA path correctly returns `Err(Unsupported)` for these; legacy also fails
because these scenarios exercise runtime list iteration.

**Sub-bucket Q-a: quantifier on node/rel list (8 failures)**

Scenario pattern: `MATCH p = (:S)-[*]->(e) RETURN none(x IN nodes(p) WHERE ...)`.
The list `nodes(p)` / `relationships(p)` is not materialised at translate time.

| Feature | Failing scenarios | Path | Status |
|---------|------------------|------|--------|
| [Quantifier1.feature](tests/tck/features/expressions/quantifier/Quantifier1.feature) | [8] None quantifier on list containing nodes | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier1.feature](tests/tck/features/expressions/quantifier/Quantifier1.feature) | [9] None quantifier on list containing relationships | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier2.feature](tests/tck/features/expressions/quantifier/Quantifier2.feature) | [8] Single quantifier on list containing nodes | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier2.feature](tests/tck/features/expressions/quantifier/Quantifier2.feature) | [9] Single quantifier on list containing relationships | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier3.feature](tests/tck/features/expressions/quantifier/Quantifier3.feature) | [8] Any quantifier on list containing nodes | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier3.feature](tests/tck/features/expressions/quantifier/Quantifier3.feature) | [9] Any quantifier on list containing relationships | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier4.feature](tests/tck/features/expressions/quantifier/Quantifier4.feature) | [8] All quantifier on list containing nodes | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier4.feature](tests/tck/features/expressions/quantifier/Quantifier4.feature) | [9] All quantifier on list containing relationships | LQA+L2 | Failing in legacy; LQA Unsupported |

**Root cause**: The quantifier list is `relationships(p)` or `nodes(p)` on a
varlen path — these return a runtime sequence of graph entities, not scalar
values that can be embedded in `VALUES`. Fixing these requires L2 (path
decomposition) or a property-path SPARQL rewrite.

**Sub-bucket Q-b: statically-folded quantifier not folded (2 failures)**

The predicate is a compile-time constant (`true` or `false`) but the list is
runtime. The tautology fold in LQA handles literal-list cases but misses cases
where the list comes from a chain of `rand()` / `reverse()` / list comprehension.

| Feature | Failing scenarios | Path | Status |
|---------|------------------|------|--------|
| [Quantifier9.feature](tests/tck/features/expressions/quantifier/Quantifier9.feature) | [2] None quantifier is always false if predicate is statically true | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier10.feature](tests/tck/features/expressions/quantifier/Quantifier10.feature) | [2] Single quantifier is always false if predicate is statically true | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier12.feature](tests/tck/features/expressions/quantifier/Quantifier12.feature) | [1] All quantifier is always false if predicate is statically false | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier11.feature](tests/tck/features/expressions/quantifier/Quantifier11.feature) | [2] Any quantifier is always true if predicate is statically true | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Quantifier11.feature](tests/tck/features/expressions/quantifier/Quantifier11.feature) | [3] Any quantifier is always true if single or all is true (×5 examples) | LQA+L2 | Failing in legacy; LQA Unsupported |

**Note**: Quantifier9[2], 12[1], 11[2] now fold via the non-empty-list tautology
shortcut in `try_fold_quantifier_invariants` (added 2026-05-07). Quantifier10[2]
requires `single(true, L) = false-if-size-≠-1` which depends on runtime size;
Quantifier11[3] requires evaluating whether `single(x) OR all(x)` implies
`any(x)` — a semantic entailment the static transpiler cannot evaluate over a
runtime list.

---

## Bucket T1 — Duration arithmetic  *(7 scenarios — Done)*

**L-level**: L1  
**Feature**: [Temporal8.feature](tests/tck/features/expressions/temporal/Temporal8.feature)  
**Design doc**: [plans/l2-runtime-support.md §1.1](plans/l2-runtime-support.md)

All seven scenarios compute arithmetic on `duration`, `date`, `time`,
`localtime`, `localdatetime`, and `datetime` values.
They route through the LQA path which handles duration arithmetic natively.

| Feature | Failing scenarios | Path | Status |
|---------|------------------|------|--------|
| [Temporal8.feature](tests/tck/features/expressions/temporal/Temporal8.feature) | [1]–[7] All arithmetic-on-temporal scenarios (×7 outlines) | LQA+L1 | Done in LQA |

---

## Bucket LC — List comprehension / Pattern comprehension  *(6 failures)*

**L-level**: L2  
**Design doc**: [plans/l2-runtime-support.md §3.3](plans/l2-runtime-support.md)  
**Effort**: medium

List comprehensions `[x IN list | expr]` where `list` is a runtime variable
(a `collect()` result or graph-traversal output), and pattern comprehensions
inside larger expressions.

| Feature | Failing scenarios | Path | Status |
|---------|------------------|------|--------|
| [List12.feature](tests/tck/features/expressions/list/List12.feature) | [1] Collect and extract using a list comprehension | LQA+L2 | Failing in legacy; LQA Unsupported |
| [List12.feature](tests/tck/features/expressions/list/List12.feature) | [2] Collect and filter using a list comprehension | LQA+L2 | Failing in legacy; LQA Unsupported |
| [List12.feature](tests/tck/features/expressions/list/List12.feature) | [4] Returning a list comprehension | LQA+L2 | Failing in legacy; LQA Unsupported |
| [List12.feature](tests/tck/features/expressions/list/List12.feature) | [5] Using a list comprehension in a WITH | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Pattern2.feature](tests/tck/features/expressions/pattern/Pattern2.feature) | [7] Use a pattern comprehension inside a list comprehension | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Pattern2.feature](tests/tck/features/expressions/pattern/Pattern2.feature) | [8] Use a pattern comprehension in WITH | LQA+L2 | Failing in legacy; LQA Unsupported |

**Root cause**: `[x IN collect(n) | x.name]` collects graph nodes into a
runtime list, then maps a property projection over them. The legacy translator
emits `"complex return expression"` because `collect()` serialises to a
`GROUP_CONCAT` string and element access on that string is unsupported.
List12[4,5] use `MATCH p = (n)-->()` with `nodes(p)` / `relationships(p)` in
the comprehension list — path decomposition is required (L2 §3.3).

---

## Bucket O — Heterogeneous ordering  *(3 failures)*

**L-level**: L1  
**Design doc**: [plans/l2-runtime-support.md §3.4](plans/l2-runtime-support.md)  
**Effort**: small

SPARQL `ORDER BY` over our `"[…]"` list-encoding strings is lexicographic;
Cypher sorts by type-rank then element-wise. A sort-key column trick
(computed at translate time for literal-list UNWIND targets) would fix these.

| Feature | Failing scenarios | Path | Status |
|---------|------------------|------|--------|
| [ReturnOrderBy1.feature](tests/tck/features/clauses/return-orderby/ReturnOrderBy1.feature) | [11] ORDER BY should order distinct types in expected order | Legacy only | Failing in legacy; LQA not reached |
| [ReturnOrderBy1.feature](tests/tck/features/clauses/return-orderby/ReturnOrderBy1.feature) | [12] ORDER BY DESC should order distinct types in expected order | Legacy only | Failing in legacy; LQA not reached |
| [WithOrderBy1.feature](tests/tck/features/clauses/with-orderBy/WithOrderBy1.feature) | [21] Sort distinct types in ascending order | Legacy only | Failing in legacy; LQA not reached |
| [WithOrderBy1.feature](tests/tck/features/clauses/with-orderBy/WithOrderBy1.feature) | [22] Sort distinct types in descending order | Legacy only | Failing in legacy; LQA not reached |
| [WithOrderBy1.feature](tests/tck/features/clauses/with-orderBy/WithOrderBy1.feature) | [45] Sort order consistent with comparisons — lists example | Legacy only | Failing in legacy; LQA not reached |

**Note**: [21]/[22] `UNWIND [n, r, p, 1.5, …]` mix graph entities with scalars;
ordering graph entities requires stable IRI-based comparison, not available
statically. [45] uses `[x IN values WHERE x < value]` where `values` is a
WITH-bound literal list — the comparator `x < value` over list elements requires
runtime evaluation.

---

## Bucket Mrg — MERGE structural failures  *(4 failures)*

**L-level**: L2 (Merge1[14], Merge5[3,4,21]) / L1 (Merge5[14])  
**Design doc**: [plans/l2-runtime-support.md §3.6](plans/l2-runtime-support.md)  
**Effort**: medium

| Feature | Failing scenarios | Error | Path | Status |
|---------|------------------|-------|------|--------|
| [Merge1.feature](tests/tck/features/clauses/merge/Merge1.feature) | [14] Merges should not match on deleted nodes | Row count mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Merge5.feature](tests/tck/features/clauses/merge/Merge5.feature) | [3] Matching two relationships | Result set mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Merge5.feature](tests/tck/features/clauses/merge/Merge5.feature) | [4] Using bound variables from other updating clause | Result set mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Merge5.feature](tests/tck/features/clauses/merge/Merge5.feature) | [14] Using list properties via variable | Unsupported: complex return expression | LQA+L2 | Failing in legacy; LQA Unsupported |
| [Merge5.feature](tests/tck/features/clauses/merge/Merge5.feature) | [21] Do not match on deleted relationships | Row count mismatch | Legacy only | Failing in legacy; LQA not reached |

**Root causes**:
- **Merge1[14]** / **Merge5[21]**: `MATCH … DELETE … MERGE` — the MERGE must
  not match the just-deleted entity. Our INSERT-WHERE-NOT-EXISTS pattern sees
  the entity as deleted within the same SPARQL update, but read-after-write
  visibility depends on engine transaction semantics. Requires two-phase
  execution (L2).
- **Merge5[3,4]**: Multi-MERGE with shared node variables — `MERGE (a)-[r1:T]->(b)
  MERGE (a)-[r2:T]->(b)` creates duplicate edges. The LQA write path emits
  two independent INSERT-WHERE-NOT-EXISTS updates without tracking inter-MERGE
  state.
- **Merge5[14]**: `UNWIND ['a,b'] AS str WITH split(str, ',') AS roles MERGE
  (a)-[r:FB {foobar: roles}]->(b)` — `roles` is a runtime list; the
  `"complex return expression"` error comes from the legacy translator's
  return projection failing on a collected list value.

---

## Bucket VL — Variable-length path edge cases  *(5 failures)*

**L-level**: L1 (cardinality bug) / L3 (Match6[14] multigraph)  
**Design doc**: [plans/fundamental-limitations.md](plans/fundamental-limitations.md)  
**Effort**: small to medium per sub-case

| Feature | Failing scenarios | Error | Path | Status |
|---------|------------------|-------|------|--------|
| [Match4.feature](tests/tck/features/clauses/match/Match4.feature) | [4] Matching longer variable length paths | Actual rows: empty | Legacy only | Failing in legacy; LQA not reached |
| [Match4.feature](tests/tck/features/clauses/match/Match4.feature) | [8] Matching relationships into a list using VL | Row count mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Match5.feature](tests/tck/features/clauses/match/Match5.feature) | [26] Handling mixed relationship patterns and directions 1 | Row count mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Match5.feature](tests/tck/features/clauses/match/Match5.feature) | [27] Handling mixed relationship patterns and directions 2 | Row count mismatch | Legacy only | Failing in legacy; LQA not reached |
| [Match6.feature](tests/tck/features/clauses/match/Match6.feature) | [14] Named path with undirected fixed variable length pattern | Actual rows: empty | Irreducible | Permanent |

**Root causes**:
- **Match4[4]**: `MATCH (a)-[*2..3]->(b)` on a larger graph — cardinality
  mismatch suggests the SPARQL property path `/:REL{2,3}` doesn't enumerate
  all intermediate paths that the TCK expects.
- **Match4[8]**: `MATCH (a)-[rs*]->(b)` with `rs` used as a list in WHERE —
  collecting the relationship variables along a varlen path requires L2
  path decomposition.
- **Match5[26,27]**: Mixed directed/undirected patterns in the same chain —
  UNION of forward+backward branches combined with a fixed-direction hop
  produces incorrect cardinality.
- **Match6[14]**: Undirected fixed-length VL path with multigraph edges —
  RDF cannot represent parallel edges between the same two nodes with the
  same predicate (L3 permanent limit).

---

## Bucket Misc — Isolated singletons  *(8 failures)*

Mixed L1/L2 cases, each requiring individual work.

### ReturnOrderBy2[12] — Aggregation of named paths

**L-level**: L1  
**Feature**: [ReturnOrderBy2.feature](tests/tck/features/clauses/return-orderby/ReturnOrderBy2.feature#L243)  
**Error**: row count mismatch — `RETURN count(p), length(p) ORDER BY length(p)`  
**Root cause**: `count(p)` on a named path variable counts serialised strings
not distinct paths; interaction with GROUP BY produces incorrect grouping.

### Set1[5] — Adding a list property

**L-level**: L1/L2  
**Feature**: [Set1.feature](tests/tck/features/clauses/set/Set1.feature#L109)  
**Error**: `"complex return expression"` — `SET n.roles = split(str, ',')` assigns
a runtime list to a property; the RETURN expression that follows cannot be
lowered because `split()` returns a non-literal list.

### With6[4] — Implicit grouping with single path variable

**L-level**: L1  
**Feature**: [With6.feature](tests/tck/features/clauses/with/With6.feature#L95)  
**Error**: row count mismatch — `WITH p, count(*) AS cnt` uses a named path `p`
as the grouping key; our GROUP BY emits `GROUP BY ?p_serialised` which deduplicates
incorrectly when paths share the same string encoding.

### Comparison1[14] — Path equality ignoring direction

**L-level**: L2  
**Feature**: [Comparison1.feature](tests/tck/features/expressions/comparison/Comparison1.feature#L276)  
**Error**: result set mismatch — `MATCH p1 = (a)-[r]->(b) MATCH p2 = (b)-[r]->(a)
RETURN p1 = p2 AS result` — Cypher path equality is direction-agnostic;
our encoding encodes direction, so `p1 ≠ p2`.

### List11[3] — range() with runtime step

**L-level**: L1 (const_int propagation)  
**Feature**: [List11.feature](tests/tck/features/expressions/list/List11.feature#L101)  
**Error**: `"complex return expression"` for `range(start, stop, step)` where `start`
and `stop`/`step` come from a `WITH … AS step` with a non-constant expression
(`step` is an element from a UNWINDed list, not a WITH-constant).  
**Status**: Partially improved — `const_int_vars` now handles `WITH n AS x` for
literal integers. The remaining case requires `step` from `UNWIND stepList AS step`
which is a runtime binding.

### Temporal2[6] — Named timezone parsing

**L-level**: L1 (DST-unaware) / L2 (full DST correctness)  
**Feature**: [Temporal2.feature](tests/tck/features/expressions/temporal/Temporal2.feature#L144)  
**Error**: result set mismatch — `datetime('2015-06-24T12:50:35.556+0100[Europe/London]')`
should round-trip with the named timezone preserved in the output string.  
**Design doc**: [plans/iana-timezone.md](plans/iana-timezone.md)  
**Status**: The `chrono-tz` integration plan is written; blocked on dependency
approval. See `iana-timezone.md` for the full fix.

---

## Bucket SKIP — Policy-skipped scenarios  *(not counted in 44)*

These scenarios are not in the 44 failures above — they are **skipped** by
the TCK runner and thus do not count against the 3828 total. Lifting them
requires new infrastructure.

| Skip reason | Count (approx) | Mitigation | Design ref |
|-------------|---------------:|------------|------------|
| `And parameters are:` — Cypher query parameters | ~80 | Runtime parameter binding | [plans/l2-runtime-support.md §4.1](plans/l2-runtime-support.md) |
| `And there exists a procedure …` — CALL stubs | ~40 | `ProcedureRegistry` trait + built-in procedures | [plans/l2-runtime-support.md §4.2](plans/l2-runtime-support.md) |
| `And having executed:` — setup CREATE parse failures | ~28 | Improved CREATE coverage in TCK setup helper | [plans/l2-runtime-support.md §4.3](plans/l2-runtime-support.md) |
| **Total** | **~148** | | |

---

## Prioritised fix queue

Ordered by (passes unlocked) / (effort days):

| Priority | Bucket | Failures unlocked | Effort | Blocker |
|----------|--------|:-----------------:|--------|---------|
| — | **T1 / Temporal8** — duration arithmetic | ~~+7~~ Done | — | Closed |
| 1 | **Q-b** — tautology fold (non-empty guard) | +3 done; +2 remain | ½ day | None |
| 2 | **O** — heterogeneous sort key column (L1 static trick) | +5 | 2 days | None |
| 3 | **Misc: Temporal2[6]** — chrono-tz integration | +1 | 1 day | Dependency approval |
| 4 | **Q-a** — quantifier on `nodes(p)` / `relationships(p)` | +8 | 1–2 weeks | L2 path decomposition |
| 5 | **LC** — list comprehension on `collect()` result | +6 | 1–2 weeks | L2 Continuation API |
| 6 | **Mrg** — MERGE after DELETE; multi-MERGE cardinality | +3 to +5 | 3–5 days | L2 two-phase write |
| 7 | **VL** — Match4[4]/[8], Match5[26,27] cardinality | +4 | 3–5 days | Path algebra audit |
| 8 | **SKIP: parameters** | +60 to +80 | 1–2 weeks | Public API change |
| 9 | **SKIP: procedure stubs** | +30 to +40 | 1–2 weeks | `ProcedureRegistry` trait |

---

## Legacy Fallback Inventory  *(goal: zero)*

Every scenario that routes through the legacy translator is a blocker for
**Phase 8.7** (delete `src/translator/`). The LQA path returns
`Err(Unsupported)` for 647 scenario executions across three fallback
classes. Instrument with `POLYGRAPH_TRACE_LEGACY=1 cargo test --test tck`.

### Class A — LQA compile-time `Unsupported`  *(491 events)*

The LQA Op tree is lowered but `sparql.rs` cannot compile it to SPARQL.
Fix track: extend `crates/polygraph/src/lqa/sparql.rs`.

| Construct / reason | Events | L-level | Fix notes |
|--------------------|-------:|---------|----------|
| `Quantifier over non-constant list` | 97 | L2 | Q-a / Q-b buckets above |
| `list comprehension [x IN list WHERE pred \| expr]` | 95 | L2 | LC bucket above |
| `list/map equality with null elements` | 47 | L1 | Extend null-equality path in `sparql.rs` |
| `UNWIND with variable/expression list` | 35 | L2 | UNWIND-var Continuation |
| `non-literal value (List) in UNWIND/VALUES context` | 31 | L2 | UNWIND-var Continuation |
| `range()` | 27 | L1 | Const-int propagation (partially done); step from UNWIND |
| `path value in projection` | 17 | L2 | Path decomposition |
| `list IN with null elements` | 16 | L1 | Extend null-IN path in `sparql.rs` |
| `PatternComprehension` expression type | 15 | L2 | LC bucket |
| `ListSlice` expression type | 15 | L1 | Implement `[start..end]` slice in `sparql.rs` |
| `duration()` constructor | 11 | L1 | Port duration literal constructor |
| `Exists` expression type | 9 | L1 | Port EXISTS subquery in `sparql.rs` |
| `list concatenation with dynamic operands` | 8 | L2 | Runtime list concat |
| `Subscript` expression type | 8 | L1 | Implement `list[idx]` subscript in `sparql.rs` |
| `property access on scalar variable (var=nonMap)` | 6 | L1 | Guard against non-map property access |
| `property access on scalar variable (var=nonGraphElement)` | 6 | L1 | Guard against non-graph property access |
| `collect()` aggregate | 6 | L2 | Runtime aggregate materialisation |
| `property access on scalar variable (var=list)` | 4 | L1 | List element property access |
| `non-literal value (Variable) in UNWIND/VALUES context` | 4 | L2 | UNWIND-var Continuation |
| `list ordering comparison` | 4 | L1 | Extend cypher_compare in LQA |
| `properties()` | 3 | L2 | Runtime map extraction |
| `labels()` | 3 | L2 | Runtime label extraction |
| `Aggregate` expression type (non-standard position) | 3 | L1 | Aggregates outside RETURN |
| `type(r)` with unknown/multiple edge types | 2 | L1 | Union-type branching |
| `property access .offsetMinutes` (var=duration) | 2 | L1 | Duration component accessors |
| `head()` | 2 | L1 | Implement head() in `sparql.rs` |
| `time()` constructor | 1 | L1 | Port time() literal constructor |
| `rand()` | 1 | L1/L3 | rand() is non-deterministic; may stay L3 |
| Other rare constructs | ~8 | L1/L2 | See trace output |

### Class B — LQA write-path `Unsupported`  *(92 events)*

The mutation (CREATE/MERGE/SET/DELETE) pipeline rejects. These go through
`lqa/write.rs`.
Fix track: extend `crates/polygraph/src/lqa/write.rs`.

| Construct / reason | Events | L-level | Fix notes |
|--------------------|-------:|---------|----------|
| `write_where_complex_op` | 32 | L1 | WHERE clause uses expression unsupported in write mode |
| `write_delete_with_return` | 28 | L1 | DELETE + RETURN in same query |
| `write_set_replace_or_merge_map` | 12 | L1 | `SET n = map` / `SET n += map` |
| `write_merge_with_outer_match` | 10 | L2 | MERGE where outer MATCH provides binding |
| `write_set_complex_expr` | 5 | L1 | SET property value is a complex expression |
| `write_merge_rel_unbound_nodes` | 5 | L1 | MERGE relationship whose nodes are unbound |
| `write_select_complex` | 1 | L1 | SELECT after write complex |

### Class C — `is_lqa_safe` pre-flight rejected  *(64 events)*

The safety pre-pass decides the AST shape cannot be lowered by LQA at all.
Fix track: `try_lqa_path()` in `crates/polygraph/src/lib.rs` + add LQA
support for each gating reason.

| Reason | Events | L-level | Fix notes |
|--------|-------:|---------|----------|
| `named_path_varlen` | 21 | L2 | Named path over varlen patterns → path decomposition |
| `varlen_named_relvar` | 13 | L1 | Varlen pattern with named relationship variable |
| `relvar_after_with` | 11 | L1 | Relationship variable referenced after a WITH boundary |
| `unbounded_varlen_unlabeled` | 10 | L1 | `[*]` without label/type bound — safety limit |
| `with_orderby_shadow_alias` | 3 | L1 | WITH item alias shadows an ORDER BY key |
| `named_path_with_real_agg` | 3 | L1 | Named path combined with real aggregate in same WITH |
| `varlen_rel_props` | 1 | L1 | Varlen relationship with inline property filter |
| `clause_shape` | 1 | L1 | Unsupported clause ordering |

---

## How to use this file

- **Search by feature name**: `grep -n "Match4" FAILURES.md`
- **Find all L2 items**: `grep "L-level.*L2" FAILURES.md`
- **Find items with no blockers**: look for `None` in the Blocker column above
- **Track progress**: update the failure count in the header and mark rows
  `~~strikethrough~~` when fixed; commit with `tck: fix <FeatureName>[N]`

After each fix, re-run `cargo test -p polygraph --test tck` and update the
header counts. The diff tool `tools/tck_diff.sh` can be used to confirm no
regressions against the baseline at `tests/tck/baseline/scenarios.jsonl`.
