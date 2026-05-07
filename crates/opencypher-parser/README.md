# opencypher-parser

Standalone openCypher and ISO GQL parser producing a typed AST. No SPARQL dependency, no execution engine required.

Part of the [rs-polygraph](https://github.com/trickle-labs/rs-polygraph) project.

## Usage

```rust
use opencypher_parser::parse_cypher;

let query = parse_cypher("MATCH (n:Person) WHERE n.age > 30 RETURN n.name").unwrap();
```

## Features

- Full openCypher 9 grammar (PEG parser via `pest`)
- ISO GQL subset (IS labels, FILTER clauses, NEXT scope boundary)
- Typed AST (`CypherQuery`, `Clause`, `Expression`, ...)
- No SPARQL, no execution engine, no runtime dependencies beyond `pest` and `thiserror`

## License

Apache-2.0
