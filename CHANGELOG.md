# Changelog

All notable changes to `polygraph` are documented here.  
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.8.0] — 2025-08-01 — Result hydration API

### Added
- **`output.map_results(&solutions)`** — callers convert raw SPARQL result rows
  (`Vec<SparqlSolution>`) back into openCypher-shaped `Vec<CypherRow>` without
  writing any SPARQL-awareness in calling code.
- **`SparqlSolution` / `RdfTerm`** — engine-agnostic input types.  Callers
  convert from their native SPARQL engine type before calling `map_results`.
- **`CypherValue`** — full openCypher type hierarchy (`Null`, `Boolean`,
  `Integer`, `Float`, `String`, `List`, `Map`, `Node`, `Relationship`).
- **`ProjectionSchema` / `ColumnKind`** — automatically built by the translator
  (LQA path) alongside the SPARQL string; classifies each RETURN column as
  `Scalar`, `Node`, or `Relationship`, preserving aliases.
- **XSD datatype conversion** (`src/result_mapping/xsd.rs`) — maps
  `xsd:integer`, `xsd:double`, `xsd:float`, `xsd:boolean`, `xsd:string`, plain
  literals, IRIs, and blank nodes to the correct `CypherValue` variant.
- **10 integration tests** (`tests/integration/result_mapping.rs`) covering
  schema generation (scalar, node, aliased, distinct, multiple columns,
  aggregate) and end-to-end scalar result mapping (integer, null, empty).

### Fixed
- LQA compiler now correctly classifies projected node variables as
  `ColumnKind::Node { iri_var }` instead of `ColumnKind::Scalar`.  Previously
  `RETURN n` and `RETURN a, b` with node variables produced Scalar entries in
  the schema, causing callers to receive IRIs as strings instead of
  `CypherNode` values.  Relationship variables are likewise classified as
  `ColumnKind::Relationship`.

## [0.7.1] — 2025-08-01 — Write-clause public API

### Added
- **`Transpiler::cypher_to_sparql_update(cypher, engine)`** — new public API
  that transpiles CREATE / MERGE / SET / REMOVE / DETACH DELETE Cypher
  statements into SPARQL 1.1 Update strings ready for execution against any
  compliant engine.  DELETE/DETACH DELETE are routed through the LQA write
  path; CREATE/MERGE/SET/REMOVE through the static write generator in
  `translator::cypher::write_update`.
- **`Transpiler::gql_to_sparql_update(gql, engine)`** — same, for ISO GQL
  input (GQL is lowered to the same IR before write generation).
- **`translator::cypher::write_update`** module (public within crate) — static
  write-clause generator extracted from the TCK test harness, parameterised
  over `base: &str` instead of a hard-coded constant.  Handles:
  - `CREATE` — blank-node INSERT DATA;
  - `MERGE (node)` — idempotent INSERT...WHERE FILTER NOT EXISTS;
  - `MERGE (n)-[:R]->(m)` — edge INSERT matching existing src/dst nodes;
  - `MATCH...SET` — property update via DELETE/INSERT...WHERE;
  - `MATCH...REMOVE` — property removal via DELETE...WHERE.
- **`tests/integration/movie_graph.rs`** — end-to-end integration test that
  populates a mini Neo4j movie graph (3 movies + 6 people), validates MERGE
  idempotency, adds relationships, runs read queries, exercises SET/REMOVE, and
  tears down with DETACH DELETE.

### Changed
- `cypher_to_sparql_update` routes only DELETE/DETACH DELETE through the LQA
  write path; CREATE/MERGE/SET/REMOVE now use the static write generator, which
  correctly implements idempotent MERGE semantics that the LQA write path did
  not preserve for relationship patterns.

## [0.7.0] — 2025-08-01 — Spec-anchored LQA + differential testing milestone

This release completes the spec-first pivot (Phase 8): the primary translation
path is now the LQA (Logical Query Algebra) pipeline, spec-anchored against the
openCypher TCK. The legacy direct-to-SPARQL translator is retained as a
fallback for constructs not yet covered by LQA.

### Added
- **250-query differential test suite** (`polygraph-difftest`): curated TOML
  fixtures covering arithmetic, aggregation, CASE, COLLECT, EXISTS, GQL label
  filters, named paths, XOR predicates, and write clauses.  All 250 pass
  end-to-end against Oxigraph.
- `rust-toolchain.toml` pinning to `stable` for reproducible CI builds.
- `keywords`, `categories`, `rust-version`, `readme`, and
  `[package.metadata.docs.rs]` fields in `Cargo.toml` for crates.io quality.

### Changed
- Bumped version from `0.1.0` to `0.7.0` to reflect the significant work
  accumulated since initial publication.
- `cargo clippy -- -D warnings` now passes with zero warnings across all
  source files.  Dead constants, methods, and functions were removed; intentional
  suppressions are annotated with `#[allow]`.
- `cargo fmt` is clean throughout.

### Fixed
- Inner-attribute placement in `translator/cypher/mod.rs` (inner attributes
  must precede outer doc comments).
- Orphaned doc-comment blocks in `lqa/sparql.rs` that triggered
  `clippy::empty_line_after_doc_comments`.
- Manual prefix-strip patterns in `parser/cypher.rs` replaced with
  `str::strip_prefix` chains.

### Known limitations
- 647 TCK scenarios still route through the legacy translator fallback; removing
  it requires the L2 runtime continuation API (planned for v0.8.1).
- `filter_is_null` GQL integration test is a known pre-existing failure
  (inline IS NULL filter not yet lowered by the GQL path).
