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

This must be resolved before v0.9.0.

---

## v0.9.0 — Complete spec-first pivot + standalone parser

**Plans**: [spec-first-pivot.md](plans/spec-first-pivot.md), [parser-extraction.md](plans/parser-extraction.md) | **Status**: 🚧 in progress | **Release**: TBD

### What ships

**Legacy translator deletion**: Drain all remaining LQA fallback buckets so `src/translator/` can be deleted entirely.

| Remaining bucket | Fallback count |
|---|---|
| Bucket 7 — `range()` with non-literal args | ~26 |
| Bucket 12 — long-tail (scalar-var properties, `type(r)`, EXISTS, misc) | ~57 |
| Bucket 8 remainder — relvar cross-product / rename shapes | ~11 |

**Standalone parser crate**: Extract `grammars/`, `src/ast/`, `src/parser/`, and parse-error subset of `src/error.rs` into `opencypher-parser` crate with zero SPARQL coupling. The main crate becomes a thin translation layer on top.

### Exit criterion

- Package name resolved and crate renamed
- `src/translator/` deleted entirely
- `opencypher-parser` published to crates.io
- TCK ≥ 3765 / 3828 (baseline maintained)
- Difftest ≥ 232 / 232

---

## v0.10.0 — L2 runtime support + IANA timezone

**Plans**: [l2-runtime-support.md](plans/l2-runtime-support.md), [iana-timezone.md](plans/iana-timezone.md) | **Status**: 🚧 in progress | **Release**: TBD

### What ships

**L2 runtime wiring**: Wire remaining Cypher constructs to emit `TranspileOutput::Continuation` outputs, closing the 63 open TCK failures.

| Bucket | Scenarios | Mitigation |
|---|---|---|
| Temporal8 — duration arithmetic | ~17 | `Continuation` + native SPARQL function helpers |
| List comprehension / `properties()` / `relationships(p)` | ~18 | `Continuation` (materialize list, re-query per element) |
| UNWIND of runtime lists | ~91 + ~62 | `Continuation` (phase 1 = list query, phase 2 = VALUES) |
| `collect()` aggregate returning typed list | ~57 | `Continuation` |
| DST timezone — Temporal2/3/10 | ~10 | with `chrono-tz` support below |
| Variable-length path decomposition | ~5 | `Continuation` or v0.11.0 extension |

**IANA timezone support**: Add `chrono` + `chrono-tz` as production dependencies to handle historical LMT transitions and sub-day DST fold precision. Fixes Temporal2, Temporal3, Temporal10 failures.

### Exit criterion

- TCK ≥ 3800 / 3828 (≥ 99 % pass rate target)
- `chrono-tz` dependency added; `temporal.rs` DST table deleted
- All L2-addressable buckets fully wired

---

## v0.11.0 — Postgres path decomposition extensions

**Plan**: [pg-extension-protocol.md](plans/pg-extension-protocol.md) | **Status**: 🔜 planned | **Release**: TBD

### What ships

For Postgres-backed triplestore: implement `pg:followEdges` (walk a runtime edge list) and `pg:pathEdges` (bind intermediate edges during property-path traversal), gated behind `TargetEngine::supports_path_decomposition()`.

Unlocks:
- `nodes(p)`, `relationships(p)`, `length(p)` on variable-length paths
- Per-hop property filters on unbounded paths
- Match4[8] runtime edge list pattern (last structural TCK ceiling)

### Exit criterion

- `TargetEngine::supports_path_decomposition()` implemented
- Custom functions defined in `target/postgres/` module
- Match4[8] passing (Postgres engine only)
- Documentation updated with engine-specific limitations
