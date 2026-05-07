# rs-polygraph Roadmap

**Audience**: Product managers, stakeholders, and technically curious readers who want to understand what each release delivers and why — without needing to read Rust code or SPARQL specifications.

**Purpose**: `rs-polygraph` transpiles openCypher and ISO GQL property graph queries into SPARQL 1.1 (and SPARQL-star) algebra. The output targets any SPARQL-compliant engine without modifying those engines. This roadmap tracks the phased delivery as a series of 0.x releases.

See [plans/](plans/) for design documents and [AGENTS.md](AGENTS.md) for project governance.

---

## Versions

### Foundation & Core Algebra (v0.1.x – v0.2.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|-----------------|------|------|
| **v0.1.0** | ✅ Released | **Foundation**: Initialize Cargo workspace with module structure (`ast`, `parser`, `translator`, `target`, `rdf_mapping`, `error`). Write `grammars/cypher.pest` covering core clauses (`MATCH`, `WHERE`, `RETURN`, `WITH` in openCypher syntax). Implement `pest`-based parser producing typed `CypherQuery` AST. Unit tests for all core AST node types. Parser round-trip tests. First milestone: `polygraph::parser::cypher::parse(query)` returns a typed AST for basic `MATCH … RETURN` queries. | Large | [implementation-plan.md](plans/implementation-plan.md) |
| **v0.2.0** | ✅ Released | **Core Translator**: Define `AstVisitor` trait in `translator/visitor.rs`. Implement node/label/property → RDF triple pattern mappings. Map relationships (directed and undirected) → triple patterns. Translate `WHERE` predicates to `FILTER` expressions, `RETURN` projections to `SELECT` variables, `OPTIONAL MATCH` to `OPTIONAL { }` graph patterns, and `WITH` to sub-select or `BIND`. Full integration tests asserting SPARQL output. Milestone: `Transpiler::cypher_to_sparql(q, engine)` works for single-hop queries. | Large | [implementation-plan.md](plans/implementation-plan.md) |


### Graph Features & Multi-Source Queries (v0.3.x – v0.4.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|-----------------|------|------|
| **v0.3.0** | ✅ Released | **RDF Mapping**: Implement `rdf_mapping::rdf_star` encoder for edge property triples in RDF-star syntax. Implement `rdf_mapping::reification` fallback for standard RDF. Define `TargetEngine` trait with `supports_rdf_star()` capability flag, allowing adapters to declare engine constraints. Implement `target::rdf_star::RdfStar` and `target::GenericSparql11` adapters. Full test coverage for both encoding modes. Milestone: Relationship properties transpile correctly for both RDF-star and legacy SPARQL 1.1 engines. | Medium | [implementation-plan.md](plans/implementation-plan.md) |
| **v0.4.0** | ✅ Released | **Feature Completeness**: Add variable-length path patterns (`-[:REL*]->`, `-[:REL*1..]->`, `-[:REL*0..1]->`) mapped to SPARQL property path cardinality. Multi-type relationship union (`-[:A\|B]->`) via Alternative property paths. `UNWIND [literal list] AS var` → SPARQL `VALUES`. Aggregation functions (`count`, `sum`, `avg`, `min`, `max`, `collect`) → SPARQL aggregate + `GROUP BY`. `ORDER BY` (multi-field, ASC/DESC), `SKIP`/`LIMIT` → SPARQL `OrderBy` and `Slice`. `IN [a, b, c]` expressions. `CALL` procedure stubs (parsed, UnsupportedFeature returned). Write clauses (`MERGE`, `CREATE`, `SET`, `DELETE`, `REMOVE`) parsed and validated. Expand grammar to 150+ constructs. 45 regression tests. Milestone: Handles the majority of real-world read Cypher queries; publicly announce alpha. | Large | [implementation-plan.md](plans/implementation-plan.md) |



### TCK Compliance Suite (v0.5.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|----------------|------|------|
| **v0.5.0** | ✅ Released | **TCK Foundation**: Integrate the `cucumber` Gherkin test runner against an in-process Oxigraph SPARQL engine. Vendorize 463 openCypher TCK feature files across 4 categories. Implement step definitions for all «Given»/«When»/«Then» patterns. Ship baseline TCK report: **461/463 passing (99.6%)** on the 4-category subset. The 2 failures are documented fundamental static-transpiler limits (runtime path constraints, RDF multigraph representation). | Large | [implementation-plan.md](plans/implementation-plan.md) |
| **v0.5.1** | ✅ Released | **Full Suite Compliance**: Vendorize all 37 TCK categories (3828 scenarios). Phases B–D: expand grammar to cover graph/pattern/quantifier, write-clause, and full temporal constructs. Fix temporal type construction and arithmetic (ISO date/time/datetime/duration, week/ordinal/quarter components, xsd typed literals). Add write-clause semantic validation for CREATE/MERGE/SET/REMOVE/DELETE. Implement EXISTS subquery support. Implement quantifier tautology folding (Quantifier9–12 +54 passes), compile-time min/max fold over literal lists, mixed-type ORDER BY sort-key encoding with Cypher type-rank. Split the monolithic translator into 8 focused files (−1,038 dead lines). Expand TCK runner with write-clause + temporal + graph/path/quantifier shards. Fix duration semantic comparison in the harness (ISO 8601 global-negative and per-component signs). **End state: 3756/3828 passing (98.1%).** | Large | [plans/remaining-work.md](plans/remaining-work.md) |


### Spec-First Pivot (v0.6.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|----------------|------|------|
| **v0.6.0** | 🚧 In progress | **Logical Query Algebra**: Replace the TCK-driven patch methodology with a spec-anchored architecture. Freeze a regression baseline and introduce the `polygraph-difftest` differential testing harness (204 curated queries covering the full expression surface, all passing). Harden the grammar: `CALL { }` subquery, GQL label expressions (`\|`/`&`/`!`), inline `WHERE` in node patterns. Introduce the Logical Query Algebra (`src/lqa/`) — `Expr` IR with `Type` lattice, `Op` operator enum covering all Cypher operators, `Bag<T>` multiset, and a `normalize` pass (CASE desugaring, alias lifting). Run a spec-driven audit of the translator: delete `rewrite.rs`, reclassify `SCENARIO-PATCH` tags as spec-derivable normalizations or structured `Unsupported { spec_ref }` errors. Route queries through the LQA as the primary path; the legacy translator is retained as a fallback for variable-length paths, temporal arithmetic, and write clauses. Fix bugs exposed by wider LQA routing (aggregate GROUP BY scoping, ORDER BY alias expansion, OPTIONAL property null semantics, WITH scoping, UNION). Phase L2-β: duration arithmetic via custom Oxigraph SPARQL functions (8 new Temporal8 passes). **Temporal DST precision**: precise last-Sunday-of-March/October rule; hour-aware boundary for DST transition day (Oct 29 midnight = summer, 04:00 = winter); UTC conversion for mixed TZ/local `duration.inSeconds`. **collect() aggregate in LQA** (GROUP_CONCAT with collect post-wraps). **list_const_map**: WITH-bound literal lists substituted into UNWIND without legacy fallback. LQA list-map-lower custom fn `[x IN list \| toLower(x)]`; size-null-fold `size([x IN collect(r) WHERE x <> null]) → 0`; UNWIND subscript expansion `UNWIND list[idx] AS v`; relationships(p) for exact-hop bounded paths `*n..n`. **End state: 3784/3828 passing (98.9%); 232/232 difftest queries passing.** | Large | [plans/spec-first-pivot.md](plans/spec-first-pivot.md) |


### Public API & Legacy Elimination (v0.7.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|----------------|------|------|
| **v0.7.0** | 🔜 Planned | **Stable Public API + First crates.io Release**: Stabilize the public surface — `transpile_cypher`, `transpile_gql`, `TranspileOptions`, `TranspileOutput`, `TargetEngine`, `PolygraphError` — with semver guarantees. Publish the `Unsupported` construct catalog so callers can distinguish transpiler bugs from semantically infeasible SPARQL patterns. Delete the legacy translator once `is_lqa_safe()` returns `true` for ≥ 99 % of the TCK corpus. Wire up GitHub Actions CI (test / TCK / difftest / clippy / fmt / doc jobs on stable + beta) and an automated `release.yml` that publishes to crates.io on `v*` tag push. Add `[package.metadata.docs.rs]` for full-feature docs builds. Add `keywords`, `categories`, `readme`, and `exclude` metadata so the crate is discoverable and the download is small. Ship an integration example against a second SPARQL engine (Apache Jena or Stardog via `TargetEngine`). | Large | [plans/release.md](plans/release.md), [plans/spec-first-pivot.md](plans/spec-first-pivot.md) |
| **v0.7.1** | 🔜 Planned | **Write-Clause Public API**: Promote the write-clause transpilation machinery (currently private test infrastructure in `tests/tck/main.rs`) to a stable, engine-parameterised public API. Ship `Transpiler::cypher_to_sparql_update()` and `gql_to_sparql_update()` returning SPARQL 1.1 Update strings for `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, and `DETACH DELETE`. Document the two-phase call pattern (run updates first, then run the SELECT from `cypher_to_sparql()` / `gql_to_sparql()`). Return `UnsupportedFeature` for DDL constructs without a SPARQL equivalent (`CREATE CONSTRAINT`, `CREATE INDEX`). Add an integration test covering the full Neo4j ["Get started with Cypher"](https://neo4j.com/docs/getting-started/cypher/intro-tutorial/) movie graph: populate via `MERGE`, query via `MATCH`, tear down via `DETACH DELETE` (works for both openCypher and equivalent ISO GQL). **Goal: the Neo4j getting-started tutorial runs end-to-end on Oxigraph and other SPARQL triplestores.** | Medium | [plans/write-clause-api.md](plans/write-clause-api.md) |


### Result Mapping & L2 Runtime (v0.8.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|----------------|------|------|
| **v0.8.0** | 🔜 Planned | **Result Hydration**: Ship `src/result_mapping/` — the SPARQL-results-to-openCypher bridge. The transpiler returns a `TranspileOutput` carrying both the SPARQL string and a `ProjectionSchema` that maps SPARQL binding names back to Cypher columns (nodes, relationships, scalars, lists). Callers pass their raw SPARQL result rows to `output.map_results()` and receive `Vec<CypherRow>`. Enables full openCypher-on-triplestore end-to-end without any SPARQL knowledge in calling code. | Medium | [plans/result-mapping.md](plans/result-mapping.md) |
| **v0.8.1** | 🔜 Planned | **L2 Runtime Support**: Introduce `TranspileOutput::Continuation` for multi-phase queries that cannot be expressed in a single SPARQL round-trip. Phase 1 SPARQL runs first; the engine's results are fed back to the transpiler to generate Phase 2. Closes the remaining TCK failures that are L2-bounded: runtime-list quantifiers (Quantifier9–12), duration arithmetic (Temporal8), list comprehension / `properties()` / `relationships(p)` projection (List12, Graph9, Path2), and query-parameter binding (~80 skipped scenarios). Target: ≥ 99 % TCK pass rate. | Large | [plans/l2-runtime-support.md](plans/l2-runtime-support.md) |


### Standalone Parser Crate & Postgres Extensions (v0.9.x)

| Version | Release | Accomplishment | Size | Plan |
|---------|---------|----------------|------|------|
| **v0.9.0** | 🔜 Planned | **Standalone Parser**: Extract the parser and AST layers (`grammars/`, `src/ast/`, `src/parser/`, the parse-error subset of `src/error.rs`) into a standalone `opencypher-parser` crate. Zero SPARQL coupling — depends only on `pest`, `pest_derive`, and `thiserror`. Lets graph analytics tools, linters, migration utilities, and alternative backends parse openCypher/GQL without pulling in the SPARQL machinery. The `polygraph` crate becomes a thin translation layer on top. | Medium | [plans/parser-extraction.md](plans/parser-extraction.md) |
| **v0.9.1** | 🔜 Planned | **Postgres Path Extensions**: For the Postgres-backed triplestore target, implement the two custom SPARQL functions — `pg:followEdges` (walk a runtime edge list) and `pg:pathEdges` (bind intermediate edges in property-path traversal) — and a `pg:edgeAggregate` custom aggregate. Gated behind `TargetEngine::supports_path_decomposition()`. Unlocks `nodes(p)`, `relationships(p)`, per-hop property filters on unbounded paths, and the last structural TCK ceiling (Match4[8] runtime edge list). | Medium | [plans/pg-extension-protocol.md](plans/pg-extension-protocol.md) |
