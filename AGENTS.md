# rs-polygraph — Agent Guidelines

## Project Context

`rs-polygraph` is a Rust library that transpiles **openCypher** and **ISO GQL** property graph queries into **SPARQL 1.1** (and SPARQL-star) algebra. The output targets any SPARQL-compliant engine without modifying those engines.

See [ROADMAP.md](ROADMAP.md) for phased delivery. Design documents live in `plans/`; each carries a **Status** and **Updated** date.

## Plans Index

| File | Status | Purpose |
|------|--------|---------|
| [implementation-plan.md](plans/implementation-plan.md) | complete | Module layout, crate structure, initial design decisions |
| [fundamental-limitations.md](plans/fundamental-limitations.md) | reference | Hard limits of the static transpiler; L1/L2/L3 mitigation levels |
| [target-engines.md](plans/target-engines.md) | reference | SPARQL engine capability analysis (`TargetEngine` trait) |
| [spec-first-pivot.md](plans/spec-first-pivot.md) | in progress | Active: LQA bucket-drain + legacy translator deletion; Phases 6–8 in progress (v0.8.1 baseline) |
| [scenario-debt.md](plans/scenario-debt.md) | complete | Inventory of ad-hoc probe files in `examples/`; all deleted (Phase 0 deliverable) |
| [iana-timezone.md](plans/iana-timezone.md) | planned | Replace static DST table with `chrono-tz`; fix 10 timezone TCK failures |
| [release.md](plans/release.md) | complete | CI workflows, crates.io publishing, docs.rs, GitHub Pages (v0.7.0) |
| [write-clause-api.md](plans/write-clause-api.md) | complete | Write-clause public API (`cypher_to_sparql_update` / `gql_to_sparql_update`); movie-graph integration test (v0.7.1) |
| [result-mapping.md](plans/result-mapping.md) | complete | SPARQL results → openCypher values hydration API (`map_results`, v0.8.0) |
| [l2-runtime-support.md](plans/l2-runtime-support.md) | in progress | Multi-phase (L2) runtime API; `TranspileOutput::Continuation` + `runtime::drive()` (v0.8.1 baseline, ongoing) |
| [parser-extraction.md](plans/parser-extraction.md) | planned | Extract parser/AST into standalone `opencypher-parser` crate (v0.9.0) |
| [pg-extension-protocol.md](plans/pg-extension-protocol.md) | planned | Postgres triplestore custom SPARQL functions for path decomposition (v0.9.1) |

## Architecture

```
Input query (Cypher / GQL)
       │
   [parser]        pest PEG grammars → typed AST
       │
   [translator]    visitor pattern → spargebra GraphPattern
       │
   [rdf_mapping]   RDF-star or reification encoding for edge properties
       │
   [target]        TargetEngine trait — engine-specific finalization
       │
  SPARQL string / algebra
```

Key modules: `ast`, `parser`, `translator`, `rdf_mapping`, `target`. See `src/` layout in the implementation plan.

## Build and Test

```sh
cargo build
cargo test
cargo test --test tck        # TCK compliance suite
cargo bench                  # criterion benchmarks
```

The TCK test suite (`tests/tck/`) uses the `cucumber` crate and requires the openCypher TCK Gherkin files at `tests/tck/features/`.

## Code Conventions

- **Errors**: All public APIs return `Result<T, PolygraphError>` via `thiserror`. Never panic in library code.
- **No unsafe**: This crate must be `#![forbid(unsafe_code)]`.
- **Visitor pattern**: Translators implement the `AstVisitor` trait. Do not add ad-hoc match arms outside visitor impls.
- **Engine capabilities**: Always consult `TargetEngine::supports_rdf_star()` before emitting SPARQL-star syntax. Fall back to reification when false.
- **Span preservation**: Parser errors must include source spans from `pest`. Do not discard span info during AST construction.
- **Grammar files**: openCypher grammar lives in `grammars/cypher.pest`, GQL in `grammars/gql.pest`. Edits to grammars require regenerating parser tests.

## Dependencies

Prefer existing dependencies over adding new ones. Core crates: `pest`, `spargebra`, `oxigraph` (dev/integration only), `thiserror`, `cucumber` (dev), `criterion` (dev). Any new dependency requires justification in the PR.

## Testing Requirements

- Every new AST node type needs a unit test in its module.
- Every new translator mapping needs an integration test asserting the SPARQL output.
- TCK pass rate must not regress. Track compliance percentage in `ROADMAP.md`.
