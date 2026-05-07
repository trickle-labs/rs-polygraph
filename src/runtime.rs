//! Synchronous L2 runtime driver for multi-phase transpilation.
//!
//! When a Cypher query cannot be expressed as a single SPARQL 1.1 query (e.g.,
//! UNWIND of a runtime-bound list, list comprehensions over graph objects, or
//! quantifiers over collected lists), the transpiler emits a
//! [`TranspileOutput::Continuation`] instead of `Complete`.
//!
//! This module provides the infrastructure to drive those continuation chains
//! to completion using any SPARQL executor the caller supplies.
//!
//! # Example
//!
//! ```rust,ignore
//! use polygraph::{Transpiler, runtime::{drive, SparqlExecutor}};
//! use polygraph::result_mapping::BindingRow;
//!
//! struct MyEngine { /* ... */ }
//!
//! impl SparqlExecutor for MyEngine {
//!     fn execute(&self, sparql: &str) -> Result<Vec<BindingRow>, polygraph::PolygraphError> {
//!         // Execute sparql against your engine, convert results to BindingRow
//!         todo!()
//!     }
//! }
//!
//! let output = Transpiler::cypher_to_sparql("MATCH (n) RETURN n.name", &engine)?;
//! let rows = drive(output, &MyEngine { /* ... */ })?;
//! ```

use crate::result_mapping::BindingRow;
use crate::{PolygraphError, TranspileOutput};

/// A synchronous SPARQL executor contract for use with [`drive`].
///
/// Implementors convert a SPARQL SELECT query string to a sequence of binding
/// rows. Each row is a `Vec<(variable_name, Option<value_string>)>` where
/// `value_string` is the serialised RDF term (IRI in angle brackets, literal
/// in quotes, or blank node label).
///
/// This trait is intentionally minimal: it operates on raw SPARQL strings and
/// plain binding vectors, keeping `polygraph` free of dependencies on any
/// specific SPARQL engine.
pub trait SparqlExecutor {
    /// Execute a SPARQL SELECT query and return the result rows.
    ///
    /// Returns an empty vec if the query matches no results. Returns `Err`
    /// on parse or execution failure.
    fn execute(&self, sparql: &str) -> Result<Vec<BindingRow>, PolygraphError>;
}

/// Drive a [`TranspileOutput`] to completion, executing all phases.
///
/// For `Complete` outputs, executes the single SPARQL query and returns its
/// rows.  For `Continuation` outputs, executes phase 1, passes the result
/// rows to the continuation closure to obtain phase 2, and drives that
/// phase recursively (supporting N-phase pipelines).
///
/// `Write` outputs cannot be driven — use
/// [`Transpiler::cypher_to_sparql_update`] to obtain the update strings and
/// execute them separately against your engine.
///
/// # Errors
///
/// Returns `Err` if any phase fails to execute or if the output is a `Write`.
pub fn drive<E: SparqlExecutor>(
    output: TranspileOutput,
    executor: &E,
) -> Result<Vec<BindingRow>, PolygraphError> {
    match output {
        TranspileOutput::Complete { sparql, .. } => executor.execute(&sparql),
        TranspileOutput::Continuation {
            phase1,
            continue_fn,
        } => {
            let phase1_rows = drive(*phase1, executor)?;
            let next = continue_fn(phase1_rows)?;
            drive(next, executor)
        }
        TranspileOutput::Write { .. } => Err(PolygraphError::UnsupportedFeature {
            feature: "drive() cannot execute Write outputs; \
                      use cypher_to_sparql_update() to obtain the update strings"
                .to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_mapping::{ProjectionSchema, TranspileOutput};

    struct FakeExecutor {
        rows: Vec<BindingRow>,
    }

    impl SparqlExecutor for FakeExecutor {
        fn execute(&self, _sparql: &str) -> Result<Vec<BindingRow>, PolygraphError> {
            Ok(self.rows.clone())
        }
    }

    fn empty_schema() -> ProjectionSchema {
        ProjectionSchema {
            columns: vec![],
            distinct: false,
            base_iri: String::new(),
            rdf_star: false,
        }
    }

    #[test]
    fn drive_complete_returns_rows() {
        let output = TranspileOutput::Complete {
            sparql: "SELECT * WHERE {}".to_string(),
            schema: empty_schema(),
        };
        let executor = FakeExecutor {
            rows: vec![vec![("x".to_string(), Some("1".to_string()))]],
        };
        let rows = drive(output, &executor).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0].0, "x");
    }

    #[test]
    fn drive_continuation_chains_phases() {
        // Phase 1 returns one row; continuation doubles it into two rows.
        let phase1 = TranspileOutput::Complete {
            sparql: "SELECT ?x WHERE {}".to_string(),
            schema: empty_schema(),
        };
        let output = TranspileOutput::Continuation {
            phase1: Box::new(phase1),
            continue_fn: Box::new(|rows| {
                // Use phase 1 results to construct phase 2.
                assert_eq!(rows.len(), 1);
                let doubled: Vec<BindingRow> = vec![rows[0].clone(), rows[0].clone()];
                Ok(TranspileOutput::Complete {
                    sparql: "SELECT ?y WHERE {}".to_string(),
                    schema: ProjectionSchema {
                        columns: vec![],
                        distinct: false,
                        base_iri: String::new(),
                        rdf_star: false,
                    },
                })
            }),
        };
        // FakeExecutor returns 1 row for any query (phase 1 and phase 2).
        let executor = FakeExecutor {
            rows: vec![vec![("x".to_string(), Some("42".to_string()))]],
        };
        let rows = drive(output, &executor).unwrap();
        // Phase 2 (also returns 1 row from FakeExecutor) succeeds.
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn drive_write_returns_error() {
        let output = TranspileOutput::Write {
            updates: vec!["INSERT DATA {}".to_string()],
            select: None,
        };
        let executor = FakeExecutor { rows: vec![] };
        let err = drive(output, &executor).unwrap_err();
        assert!(err.to_string().contains("Write"));
    }
}
