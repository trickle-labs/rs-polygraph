# Result Mapping: SPARQL Results → openCypher Results

**Status**: complete  
**Updated**: 2025-08-01

**Goal**: Make `rs-polygraph` a complete openCypher-on-triplestore bridge. The
library already transpiles Cypher→SPARQL. This plan adds the inverse: mapping
SPARQL query results back into Cypher-shaped values (nodes, relationships,
scalars). Together they enable **1G (OneGraph)** — full openCypher semantics on
any SPARQL-compliant triplestore.

**Design constraint**: The library remains execution-agnostic. The caller
executes SPARQL against their own store. `rs-polygraph` provides the query
_and_ a recipe for interpreting the results.

---

## Architecture

```
                           rs-polygraph
                    ┌─────────────────────────┐
Cypher query ──────►│  parser → translator    │──── SPARQL string ──────► triplestore
                    │                         │                               │
                    │  + ProjectionSchema     │                               │
                    │    (column kinds, aliases│                               │
                    │     var→entity mappings) │                               ▼
                    │                         │                        SPARQL bindings
                    │  result_mapping module  │◄──── SparqlSolution ◄────────┘
                    │    hydrate + reshape    │
                    └────────┬────────────────┘
                             │
                             ▼
                      Vec<CypherRow>
                    (Nodes, Rels, scalars)
```

The caller's contract:

```rust
// Step 1: Transpile (breaking change — returns TranspileOutput, not String)
let output = Transpiler::cypher_to_sparql(query, &engine)?;

// Step 2: Execute (caller's concern — HTTP, embedded, whatever)
let sparql_results: Vec<SparqlSolution> = my_store.query(&output.sparql)?;

// Step 3: Map results back to Cypher values
let cypher_rows: Vec<CypherRow> = output.map_results(&sparql_results)?;
```

---

## Part 1 — Core Types (`src/result_mapping/types.rs`)

### CypherValue

The universal Cypher result value. Mirrors the openCypher type system.

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum CypherValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<CypherValue>),
    Map(BTreeMap<String, CypherValue>),
    Node(CypherNode),
    Relationship(CypherRelationship),
    // Path deferred — see "Hard parts" below
}

#[derive(Debug, Clone, PartialEq)]
pub struct CypherNode {
    /// The IRI identity of this node in the triplestore
    pub id: String,
    /// Labels (from rdf:type triples, with base IRI stripped)
    pub labels: Vec<String>,
    /// Properties (all datatype-valued predicates, base IRI stripped)
    pub properties: BTreeMap<String, CypherValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CypherRelationship {
    /// Opaque identity (IRI for reification, or synthesized for RDF-star)
    pub id: String,
    /// Relationship type name (IRI local name)
    pub rel_type: String,
    /// Start node IRI
    pub start_node: String,
    /// End node IRI
    pub end_node: String,
    /// Properties (from annotated triples / reification properties)
    pub properties: BTreeMap<String, CypherValue>,
}
```

### CypherRow

```rust
/// One result row, with columns named by RETURN aliases.
pub type CypherRow = BTreeMap<String, CypherValue>;
```

### SparqlSolution (input type)

Rather than depending on a specific SPARQL engine crate at compile time, define
a minimal trait or accept a generic binding representation:

```rust
/// A single SPARQL result binding. Maps variable names to RDF term strings.
/// The caller converts from their engine's native type.
pub struct SparqlSolution {
    pub bindings: BTreeMap<String, Option<RdfTerm>>,
}

#[derive(Debug, Clone)]
pub enum RdfTerm {
    Iri(String),
    Literal { value: String, datatype: Option<String>, language: Option<String> },
    BlankNode(String),
}
```

This keeps `rs-polygraph` free of runtime dependencies on oxigraph, rdf4j, etc.
A `From<oxigraph::QuerySolution>` impl can live in a separate `polygraph-oxigraph`
crate or behind a feature flag.

**~80 lines.**

---

## Part 2 — Projection Schema (`src/result_mapping/schema.rs`)

The translator already knows the shape of each RETURN column — it just discards
that information after building the SPARQL. This part captures it.

### ColumnKind

```rust
pub enum ColumnKind {
    /// A scalar value: property access, literal, aggregate, expression.
    /// The SPARQL variable directly holds the value.
    Scalar,

    /// A node variable. SPARQL holds the IRI; labels and properties
    /// need hydration from additional SPARQL variables.
    Node {
        /// SPARQL variable holding the node IRI (e.g., "n")
        iri_var: String,
        /// SPARQL variables for labels (e.g., "n__label")
        label_var: String,
        /// SPARQL variables for property keys/values
        prop_key_var: String,
        prop_val_var: String,
    },

    /// A relationship variable. Encoding depends on RDF-star vs reification.
    Relationship {
        /// How the relationship was encoded
        encoding: RelEncoding,
        /// Variable for the relationship type predicate
        type_var: String,
        /// Source and target node variables
        src_var: String,
        dst_var: String,
        /// Property key/value variables (if relationship has properties)
        prop_key_var: Option<String>,
        prop_val_var: Option<String>,
    },
}

pub enum RelEncoding {
    RdfStar,
    Reification { reif_var: String },
}
```

### ProjectionSchema

```rust
pub struct ProjectionSchema {
    /// Ordered list of output columns matching the Cypher RETURN clause.
    pub columns: Vec<ProjectedColumn>,
    /// Whether the original query used RETURN DISTINCT.
    pub distinct: bool,
    /// The base IRI used during translation (needed to strip prefixes).
    pub base_iri: String,
    /// Whether RDF-star encoding was used.
    pub rdf_star: bool,
}

pub struct ProjectedColumn {
    /// The output column name (alias if provided, otherwise expression text).
    pub name: String,
    /// What kind of value this column holds.
    pub kind: ColumnKind,
}
```

**~60 lines.**

---

## Part 3 — SPARQL Augmentation (translator changes)

When a RETURN clause projects a node or relationship variable (not just a
property), the translator must emit **additional** SPARQL patterns to fetch
the entity's labels, properties, and relationship metadata.

### Node hydration patterns

For `RETURN n` (node variable), augment the SPARQL with:

```sparql
SELECT ?n ?n__label ?n__pk ?n__pv WHERE {
  # ... existing patterns ...
  OPTIONAL { ?n a ?n__label }
  OPTIONAL { ?n ?n__pk ?n__pv . FILTER(!isIRI(?n__pv)) }
}
```

This fetches all labels and all datatype-valued (non-IRI) properties for `n`
in a single query. The result mapper then groups rows by `?n` and collapses
them into `CypherNode` structs.

The `FILTER(!isIRI(?n__pv))` heuristic excludes object properties (other
nodes) from the properties map. This matches Cypher semantics where
`n.prop` returns scalar values, not other nodes.

### Relationship hydration patterns

**RDF-star**:

For `RETURN r` where `r` is a relationship variable with known endpoints
`?a`, `?b`, and type predicate IRI:

```sparql
SELECT ?a ?b ?r__type ?r__pk ?r__pv WHERE {
  ?a ?r__type ?b .
  OPTIONAL { <<?a ?r__type ?b>> ?r__pk ?r__pv }
}
```

**Reification**:

```sparql
SELECT ?r ?r__type ?r__src ?r__dst ?r__pk ?r__pv WHERE {
  ?r a rdf:Statement ; rdf:subject ?r__src ; rdf:predicate ?r__type ; rdf:object ?r__dst .
  OPTIONAL { ?r ?r__pk ?r__pv . FILTER(?r__pk NOT IN (rdf:type, rdf:subject, rdf:predicate, rdf:object)) }
}
```

### Translator changes

In `translate_return_clause()` (~line 1415 of `translator/cypher.rs`):

1. Walk each `ReturnItem`. If `Expression::Variable(name)` and `name` was
   bound to a node pattern → classify as `ColumnKind::Node`, emit hydration
   triples, record in schema.
2. If `Expression::Variable(name)` and `name` was bound to a relationship
   pattern → classify as `ColumnKind::Relationship`, emit hydration triples.
3. Otherwise → `ColumnKind::Scalar`.

The translator must track **what each variable was bound to** during MATCH
pattern translation. This is a new `HashMap<String, BindingKind>` built during
`translate_match_clause()`, passed to `translate_return_clause()`.

```rust
enum BindingKind {
    Node,
    Relationship { src_var: String, dst_var: String, type_iri: Option<String> },
    Scalar,
}
```

### Change to translate() signature

```rust
// Before
pub fn translate(query: &CypherQuery, base_iri: Option<&str>, rdf_star: bool) -> Result<String, PolygraphError>

// After
pub fn translate(query: &CypherQuery, base_iri: Option<&str>, rdf_star: bool) -> Result<TranslationResult, PolygraphError>

pub struct TranslationResult {
    pub sparql: String,
    pub schema: ProjectionSchema,
}
```

**~120 lines of translator changes.**

---

## Part 4 — Result Mapper (`src/result_mapping/mapper.rs`)

The core algorithm:

```rust
pub fn map_results(
    solutions: &[SparqlSolution],
    schema: &ProjectionSchema,
) -> Result<Vec<CypherRow>, PolygraphError>
```

### Algorithm

1. **Group phase**: If any column is `Node` or `Relationship`, multiple SPARQL
   rows may describe the same logical Cypher row (one row per label × property
   combination). Group by the "anchor" variables — the variables that represent
   the primary identity of each column (node IRI, relationship reif var or
   src+type+dst triple).

2. **Hydrate phase**: For each group:
   - **Node columns**: Collect distinct `?n__label` values → `labels`. Collect
     distinct `(?n__pk, ?n__pv)` pairs → `properties`. Strip `base_iri` from
     IRIs to get clean names.
   - **Relationship columns**: Extract `?r__type` → `rel_type`. Collect
     `(?r__pk, ?r__pv)` → `properties`. Wire up `start_node` / `end_node`.
   - **Scalar columns**: Convert `RdfTerm` → `CypherValue` using XSD datatype
     mapping:
     - `xsd:integer`, `xsd:long`, `xsd:int` → `CypherValue::Integer`
     - `xsd:double`, `xsd:float`, `xsd:decimal` → `CypherValue::Float`
     - `xsd:boolean` → `CypherValue::Boolean`
     - `xsd:string`, plain literal → `CypherValue::String`
     - Unbound → `CypherValue::Null`

3. **Assemble phase**: Build one `CypherRow` per group, with column names from
   `schema.columns[i].name`.

### Row grouping detail

For a query like `RETURN n, n.name, m`:

```
SPARQL row 1: ?n=:Alice ?n_name="Alice" ?m=:Bob  ?n__label=:Person ?n__pk=:age ?n__pv=30  ?m__label=:Person ?m__pk=:name ?m__pv="Bob"
SPARQL row 2: ?n=:Alice ?n_name="Alice" ?m=:Bob  ?n__label=:Person ?n__pk=:name ?n__pv="Alice" ?m__label=:Employee ?m__pk=:age ?m__pv=25
SPARQL row 3: ?n=:Alice ?n_name="Alice" ?m=:Bob  ?n__label=:Employee ?n__pk=... (more combinations)
```

Group key: `(?n, ?m)` = `(:Alice, :Bob)`. From this group, build:
- `n` → `CypherNode { id: "Alice", labels: ["Person", "Employee"], properties: {age: 30, name: "Alice"} }`
- `n.name` → `CypherValue::String("Alice")`
- `m` → `CypherNode { id: "Bob", labels: ["Person", "Employee"], properties: {name: "Bob", age: 25} }`

**~150 lines.**

---

## Part 5 — API Changes (`src/lib.rs`)

### TranspileOutput (breaking change)

```rust
/// The output of a Cypher/GQL → SPARQL transpilation.
pub struct TranspileOutput {
    /// The SPARQL query string to execute against the triplestore.
    pub sparql: String,

    /// Schema describing column types and SPARQL variable mappings.
    /// Used by `map_results()` to reshape SPARQL bindings into Cypher rows.
    pub schema: ProjectionSchema,
}

impl TranspileOutput {
    /// Map SPARQL query results back into Cypher-shaped rows.
    ///
    /// The caller executes `self.sparql` against their triplestore and
    /// passes the raw bindings here. This method uses the projection
    /// schema to hydrate nodes/relationships and convert datatypes.
    pub fn map_results(
        &self,
        solutions: &[SparqlSolution],
    ) -> Result<Vec<CypherRow>, PolygraphError> {
        result_mapping::map_results(solutions, &self.schema)
    }
}
```

### Updated Transpiler methods

```rust
impl Transpiler {
    pub fn cypher_to_sparql(
        cypher: &str,
        engine: &dyn target::TargetEngine,
    ) -> Result<TranspileOutput, PolygraphError> {
        let ast = parser::parse_cypher(cypher)?;
        let result = translator::cypher::translate(
            &ast, engine.base_iri(), engine.supports_rdf_star()
        )?;
        let sparql = engine.finalize(result.sparql)?;
        Ok(TranspileOutput { sparql, schema: result.schema })
    }

    pub fn gql_to_sparql(
        gql: &str,
        engine: &dyn target::TargetEngine,
    ) -> Result<TranspileOutput, PolygraphError> {
        let ast = parser::parse_gql(gql)?;
        let result = translator::gql::translate(
            &ast, engine.base_iri(), engine.supports_rdf_star()
        )?;
        let sparql = engine.finalize(result.sparql)?;
        Ok(TranspileOutput { sparql, schema: result.schema })
    }
}
```

Callers that only want the SPARQL string: `output.sparql`.

**~40 lines.**

---

## Part 6 — Module Layout

```
src/
├── result_mapping/
│   ├── mod.rs           # Re-exports
│   ├── types.rs         # CypherValue, CypherNode, CypherRelationship, CypherRow
│   ├── schema.rs        # ProjectionSchema, ColumnKind, ProjectedColumn
│   ├── mapper.rs        # map_results() core algorithm
│   └── xsd.rs           # XSD datatype → CypherValue conversion
└── ...
```

Add `pub mod result_mapping;` to `lib.rs`.

---

## Part 7 — Integration with Existing Subsystems

### rdf_mapping

The `result_mapping` module is the **inverse** of `rdf_mapping`. The existing
`rdf_mapping::rdf_star` and `rdf_mapping::reification` modules define how
edges are encoded as triples. The result mapper must reverse that encoding.
Share the IRI construction logic rather than duplicating it.

### target::TargetEngine

Add to the trait:

```rust
pub trait TargetEngine {
    // ... existing methods ...

    /// Whether entity hydration patterns should be added for RETURN variables.
    /// Default: true. An engine might disable this if it handles hydration
    /// client-side.
    fn hydrate_entities(&self) -> bool { true }
}
```

### Error variants

Add to `PolygraphError`:

```rust
#[error("Result mapping error: {message}")]
ResultMapping { message: String },
```

---

## Hard Parts & Deferred Items

### Path reconstruction — deferred

`RETURN path` / `RETURN shortestPath(...)` cannot be fully reconstructed from
SPARQL property paths, which return only endpoints. Options:

- **Iterative deepening**: Emit a series of SPARQL queries at increasing path
  lengths and stitch together. Expensive.
- **CONSTRUCT + BFS**: Use CONSTRUCT to fetch the local neighborhood, then
  BFS in Rust. Feasible for short paths.
- **Declare limitation**: `RETURN path` returns start and end nodes only,
  with a warning. Most practical queries use `RETURN n, m` not `RETURN path`.

Recommend deferring path reconstruction to a later phase.

### RETURN * with entity hydration

`RETURN *` projects all in-scope variables. The translator must iterate the
binding map to determine which are nodes vs relationships vs scalars, and
emit hydration for all entity variables. This is mechanically identical to
explicit RETURN — just requires enumerating the binding map.

### Row explosion from hydration

A node with 5 labels and 10 properties would generate up to 50 rows per
logical result in the naive cross-join approach. Mitigations:

1. **Separate OPTIONAL blocks per axis** — prevents label×property cross-join:
   ```sparql
   { SELECT ?n (GROUP_CONCAT(?lbl; separator=",") AS ?n__labels) WHERE { ?n a ?lbl } GROUP BY ?n }
   OPTIONAL { ?n ?n__pk ?n__pv . FILTER(!isIRI(?n__pv)) }
   ```
   This gives one row per property, not per label×property. ~3× better.

2. **Subquery pre-aggregation** — aggregate labels and properties in subqueries
   using `GROUP_CONCAT`. One row per entity. Cleanest, but relies on SPARQL
   engine supporting GROUP_CONCAT (all modern engines do).

   ```sparql
   {
     SELECT ?n
       (GROUP_CONCAT(DISTINCT ?lbl; separator="\x1F") AS ?n__labels)
     WHERE { OPTIONAL { ?n a ?lbl } }
     GROUP BY ?n
   }
   OPTIONAL { ?n ?n__pk ?n__pv . FILTER(!isIRI(?n__pv)) }
   ```

   Properties still produce multiple rows, but labels are collapsed.
   Final grouping in rust collapses the property rows.

Recommend **option 2** (subquery pre-aggregation for labels) as the default
strategy, with a fallback to option 1 for engines with limited subquery
support.

### collect() aggregation

`RETURN collect(n.name)` → `CypherValue::List`. The SPARQL `GROUP_CONCAT`
result is a string that needs parsing back into a list. Alternative: use
the raw SPARQL rows before aggregation and aggregate in Rust. Needs further
design.

---

## Phased Delivery

| Phase | Scope | Estimate |
|-------|-------|----------|
| R1 | `CypherValue` types, `SparqlSolution` input type, XSD conversion, scalar-only `map_results()` | ~200 lines |
| R2 | `ProjectionSchema` + `ColumnKind`, translator emits schema for scalar columns, `TranspileOutput` API change | ~250 lines |
| R3 | Node hydration: translator emits label/property patterns, mapper groups and hydrates | ~300 lines |
| R4 | Relationship hydration: RDF-star and reification variants | ~200 lines |
| R5 | `RETURN *` support, edge cases (NULL, DISTINCT, ORDER BY + hydration) | ~150 lines |

**Total: ~1,100 lines.** Test suite adds roughly another 400 lines.

R1–R2 are independently useful — scalar-only result mapping covers the
majority of analytical queries (the TCK suite is ~90% scalar projections).

---

## Relationship to Existing Plans

### tck-final-four.md

That plan introduces `TranspileOutput` with `Single` / `MultiPhase` for write
queries (DELETE+RETURN, MERGE). This plan's `TranspileOutput` subsumes it —
the `schema` field carries result shape metadata, and multi-phase execution
can be added as a `Vec<QueryPhase>` alongside `sparql` if needed.

### target-engines.md

Engine-specific adapters (Jena, RDF4J, GraphDB, etc.) will benefit from result
mapping out of the box. The `TargetEngine` trait change
(`hydrate_entities()`) is backward-compatible with a default impl.

### OneGraph (1G) end state

With result mapping complete, the stack for "Cypher on any triplestore" is:

```
Application (Cypher queries)
       │
   rs-polygraph (transpile + result map)
       │
   HTTP SPARQL client (not in this crate)
       │
   Any SPARQL endpoint (Jena, RDF4J, GraphDB, Oxigraph, Stardog, ...)
```

No modifications to the triplestore. No stored procedures. Just SPARQL.
