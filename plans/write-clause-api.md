# Write-Clause Public API — End-to-End Read/Write openCypher & ISO GQL on SPARQL Triplestores

**Status**: complete
**Updated**: 2026-05-07
**Target version**: v0.7.1

---

## Goal

Enable the complete Neo4j ["Get started with Cypher"](https://neo4j.com/docs/getting-started/cypher/intro-tutorial/) tutorial — and by extension, any real-world openCypher or ISO GQL workflow that mixes graph construction with graph querying — to run end-to-end against Oxigraph or any other SPARQL 1.1 triplestore.

Today, `Transpiler::cypher_to_sparql()` and `Transpiler::gql_to_sparql()` handle all read queries. The missing piece is a stable public API for the write side: `CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`.

---

## What already exists

The TCK runner (`tests/tck/main.rs`) already contains working write-clause generation code that powers the 98.1% TCK pass rate. This code operates on the `Clause` AST which is shared between openCypher and ISO GQL (GQL clauses are lowered to Cypher-equivalent during parsing):

| Function | Lines | Handles |
|---|---|---|
| `write_clauses_to_updates()` | ~500 | `CREATE`, `MERGE` (node + relationship, `ON CREATE/MATCH SET`), `SET`, `REMOVE`, `DELETE`, `DETACH DELETE`, `UNWIND` iteration, `WITH` alias propagation |
| `emit_create_pattern_with_bindings()` | ~80 | Pattern → SPARQL INSERT triples, RDF-star annotated relationship properties |
| `check_nondetach_delete_connected()` | ~60 | Guards `DELETE` (non-detach) by running an ASK query to detect connected nodes |
| `expr_to_sparql_lit()` / temporal helpers | ~250 | Literal value encoding including temporal constructors |

This code is currently private test infrastructure. v0.7.1 promotes it to a proper public API surface, shared across both language variants.

---

## What needs to be built

### 1. Extract write generator into `src/translator/cypher/write_update.rs`

Move the core generation logic from `tests/tck/main.rs` into the library. Key changes:

- **Parameterise over `TargetEngine`** — replace the hardcoded `BASE` constant and the `RDF-star` boolean with calls to `engine.base_iri()` and `engine.supports_rdf_star()`, matching the pattern used by `translate()` and `translator::gql::translate()`.
- **Language-agnostic implementation** — accept a `&[Clause]` slice (the Clause AST is shared between openCypher and ISO GQL after parsing).
- **Proper error returns** — return `Result<Vec<String>, PolygraphError>` rather than silently returning an empty Vec on parse failure.
- **Remove test-only temporal helpers** — the temporal constructor evaluation (date/time/datetime/duration map constructors) can stay in the TCK runner for now; the write generator only needs `expr_to_sparql_lit()` which already handles the common cases.
- **Keep the coalescing pre-pass** — consecutive `CREATE` clauses must share a bnode scope; this is a correctness requirement, not a test hack.

### 2. Expose on `Transpiler` (parallel methods for Cypher and GQL)

For **openCypher**:
```rust
/// Transpile the write clauses (`CREATE`, `MERGE`, `SET`, `REMOVE`, `DELETE`,
/// `DETACH DELETE`) of an openCypher query into a sequence of SPARQL 1.1 Update strings.
///
/// Execute the returned updates **in order** against your SPARQL endpoint before
/// running the SELECT produced by `cypher_to_sparql()` for the same query.
///
/// For pure-read queries this returns `Ok(vec![])`.
///
/// # Errors
///
/// Returns `PolygraphError::UnsupportedFeature` for constructs that have no
/// SPARQL Update equivalent (e.g. `CREATE CONSTRAINT`, variable-length path MERGE).
pub fn cypher_to_sparql_update(
    cypher: &str,
    engine: &dyn TargetEngine,
) -> Result<Vec<String>, PolygraphError>
```

For **ISO GQL** (identical signature, mirrors `gql_to_sparql()`):
```rust
pub fn gql_to_sparql_update(
    gql: &str,
    engine: &dyn TargetEngine,
) -> Result<Vec<String>, PolygraphError>
```

Both methods share the same underlying write-generation logic, since GQL clauses are Cypher-equivalent after parsing.

### 3. Document the two-phase call pattern

Mixed read-write queries (e.g. `MERGE (n:Movie {title: $t}) RETURN n`) require two steps from the caller.

For **openCypher**:
```rust
// Phase 1 — write
let updates = Transpiler::cypher_to_sparql_update(cypher, &engine)?;
for update in &updates {
    store.update(update)?;
}

// Phase 2 — read (write clauses are automatically skipped internally)
let output = Transpiler::cypher_to_sparql(cypher, &engine)?;
let results = store.query(&output.sparql)?;
```

For **ISO GQL**:
```rust
// Phase 1 — write
let updates = Transpiler::gql_to_sparql_update(gql, &engine)?;
for update in &updates {
    store.update(update)?;
}

// Phase 2 — read
let output = Transpiler::gql_to_sparql(gql, &engine)?;
let results = store.query(&output.sparql)?;
```

This replaces today's private `translate_skip_writes()` with a coherent public workflow.

### 4. Integration test: Neo4j movie graph tutorial

Add `tests/integration/movie_graph.rs` (or a new example `examples/movie_graph.rs`):

1. **Populate** — execute all `MERGE` statements from the tutorial's "Create the Movie Graph" block using `cypher_to_sparql_update()`.
2. **Query** — run each read query from the tutorial through `cypher_to_sparql()` and assert expected result sets (actor names, directors, co-actor lists, shortest-path length).
3. **Teardown** — run `MATCH (n) DETACH DELETE n` through `cypher_to_sparql_update()` and verify the store is empty.

This test acts as a living compatibility check: if the tutorial breaks, the test fails.

---

## CONSTRAINT DDL

The tutorial begins with two `CREATE CONSTRAINT` statements:

```cypher
CREATE CONSTRAINT movie_title IF NOT EXISTS FOR (m:Movie) REQUIRE m.title IS UNIQUE;
CREATE CONSTRAINT person_name IF NOT EXISTS FOR (p:Person) REQUIRE p.name IS UNIQUE;
```

SPARQL triplestores have no constraint system. These must return:

```
PolygraphError::UnsupportedFeature {
    feature: "CREATE CONSTRAINT",
    spec_ref: "openCypher 9.0 §8 — schema constraints have no SPARQL 1.1 equivalent",
}
```

The integration test skips these two statements. The README should note that callers targeting a triplestore can safely drop constraint DDL before transpilation.

---

## Known limitations

| Construct | Status | Notes |
|---|---|---|
| `CREATE (n:Label {prop: val})` | ✅ Supported | Emits `INSERT DATA { _:n a <base:Label>; <base:prop> val }` |
| `MERGE (n:Label {prop: val})` | ✅ Supported | Emits `INSERT WHERE NOT EXISTS { ... }` pattern |
| `MERGE ...-[:REL]->(m)` | ✅ Supported | Single-hop relationship MERGE |
| `ON CREATE SET` / `ON MATCH SET` | ✅ Supported | Conditional INSERT/UPDATE |
| `SET n.prop = val` | ✅ Supported | `DELETE ... INSERT ... WHERE` |
| `REMOVE n.prop` / `REMOVE n:Label` | ✅ Supported | `DELETE WHERE { ?n <base:prop> ?v }` |
| `DELETE n` | ✅ Supported | Guarded by connectivity ASK check |
| `DETACH DELETE n` | ✅ Supported | `DELETE { ?n ?p ?o . ?s ?p ?n } WHERE { ... }` |
| `CREATE CONSTRAINT` / `DROP CONSTRAINT` | ❌ UnsupportedFeature | No SPARQL equivalent |
| `CREATE INDEX` | ❌ UnsupportedFeature | No SPARQL equivalent |
| Variable-length path MERGE | ❌ UnsupportedFeature | Requires L2 runtime (v0.8.1) |
| `FOREACH` write | ❌ UnsupportedFeature | Planned for v0.8.1 |
| MERGE uniqueness guarantee | ⚠️ Best-effort | Without constraints, concurrent writes or inconsistently loaded data can produce duplicate nodes with identical properties |

---

## Size

**Medium** — the implementation already exists in the test runner; this is primarily an extraction, parameterisation, and API stability exercise. The integration test for the movie graph is the largest net-new piece of work.
