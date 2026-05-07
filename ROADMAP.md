# rs-polygraph Roadmap

**Audience**: Product managers, stakeholders, and technically curious readers who want to understand what each release delivers and why — without needing to read Rust code or SPARQL specifications.

**Purpose**: `rs-polygraph` transpiles openCypher and ISO GQL property graph queries into SPARQL 1.1 (and SPARQL-star) algebra. The output targets any SPARQL-compliant engine without modifying those engines.

See [plans/](plans/) for design documents, [AGENTS.md](AGENTS.md) for project governance, and [CHANGELOG.md](CHANGELOG.md) for the history of what has shipped.

---

## Current baseline — v0.8.1

- **TCK compliance**: 3765 / 3828 scenarios passing (98.3 %)
- **Differential tests**: 232 / 232 curated queries passing against Oxigraph
- **Public API**: `Transpiler::cypher_to_sparql`, `gql_to_sparql`, `cypher_to_sparql_update`, `gql_to_sparql_update`; `TranspileOutput` (Complete / Continuation / Write); `runtime::drive()`
- **Architecture**: LQA (Logical Query Algebra) is the primary translation path; the legacy translator in `src/translator/` is retained as a fallback for variable-length paths, UNWIND of runtime lists, list comprehensions, and `collect()`

---

## Blocking issue: package name on crates.io

The crate name **`polygraph`** is already taken on crates.io. A new name must be chosen and `Cargo.toml` / `src/lib.rs` / GitHub repo must be updated before any release. Candidates:
- `sparql-cypher` or `cypher-sparql` — descriptive but long
- `opensparql` — openCypher → SPARQL
- `cypher-transpiler` — functional but generic
- `graphstorm` — shorter, memorable

This should be resolved before §1 completes.

---

## Upcoming work

### 1. Complete the spec-first pivot — legacy translator deletion (🚧 in progress)

**Plan**: [spec-first-pivot.md](plans/spec-first-pivot.md) | **Target**: v0.9.0

Drain the remaining LQA fallback buckets so `src/translator/` can be deleted entirely. Three buckets are explicitly deferred to §2 (L2 runtime); all other permanent-construct fallbacks are ported query-class by query-class following the mechanical loop in the plan.

| Remaining bucket | Fallback count |
|---|---|
| Bucket 7 — `range()` with non-literal args | ~26 |
| Bucket 12 — long-tail (scalar-var properties, `type(r)`, EXISTS, misc) | ~57 |
| Bucket 8 remainder — relvar cross-product / rename shapes | ~11 |
| L2-deferred (UNWIND runtime lists, `collect()`, list comprehensions) | stay as `Continuation` until §2 |

**Exit criterion**: `src/translator/` deleted; TCK ≥ 3765; difftest ≥ 232.

---

### 2. L2 runtime support — close remaining TCK failures (🚧 in progress)

**Plan**: [l2-runtime-support.md](plans/l2-runtime-support.md) | **Target**: v0.9.0 patch releases

`TranspileOutput::Continuation` and `runtime::drive()` are shipped. The remaining work is wiring individual Cypher constructs to emit `Continuation` outputs so the runtime driver closes the 63 open TCK failures.

| Bucket | Scenarios | Mitigation |
|---|---|---|
| Temporal8 — duration arithmetic | ~17 | `Continuation` + native SPARQL function helpers |
| List comprehension / `properties()` / `relationships(p)` | ~18 | `Continuation` (materialize list, re-query per element) |
| UNWIND of runtime lists (Buckets 3, 9) | ~91 + ~62 | `Continuation` (phase 1 = list query, phase 2 = VALUES) |
| `collect()` aggregate returning typed list | ~57 | `Continuation` |
| DST timezone — Temporal2/3/10 | ~10 | blocked on §4 (`chrono-tz`) |
| Variable-length path decomposition | ~5 | `Continuation` or §5 extension |

**Exit criterion**: ≥ 99 % TCK pass rate (≥ 3790 / 3828).

---

### 3. Standalone parser crate (🔜 planned)

**Plan**: [parser-extraction.md](plans/parser-extraction.md) | **Target**: v0.9.1

Extract `grammars/`, `src/ast/`, `src/parser/`, and the parse-error subset of `src/error.rs` into a standalone `opencypher-parser` crate with zero SPARQL coupling. Depends only on `pest`, `pest_derive`, and `thiserror`. Lets graph analytics tools, linters, migration utilities, and alternative backends parse openCypher/GQL without pulling in `spargebra`. The `polygraph` crate becomes a thin translation layer on top.

---

### 4. IANA timezone support (🔜 planned)

**Plan**: [iana-timezone.md](plans/iana-timezone.md) | **Target**: v0.9.2

Add `chrono` + `chrono-tz` as production dependencies to replace the hand-written static DST table in `src/translator/cypher/temporal.rs`. Fixes the ~10 DST-bucket TCK failures that require the full IANA tzdata database (historical LMT transitions, sub-day DST fold precision). Unblocks the DST row in §2.

---

### 5. Postgres path extensions (🔜 planned)

**Plan**: [pg-extension-protocol.md](plans/pg-extension-protocol.md) | **Target**: v0.10.0

For the Postgres-backed triplestore target: implement `pg:followEdges` (walk a runtime edge list) and `pg:pathEdges` (bind intermediate edges during property-path traversal), gated behind `TargetEngine::supports_path_decomposition()`. Unlocks `nodes(p)`, `relationships(p)`, per-hop property filters on unbounded paths, and the last structural TCK ceiling (Match4[8]).
