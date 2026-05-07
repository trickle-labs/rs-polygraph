// Re-export parse functions from opencypher_parser so that all existing
// `polygraph::parser::*` paths continue to work without change.
//
// The parse functions return `opencypher_parser::ParseError` on failure;
// `PolygraphError` wraps it via `From<ParseError>`.
pub use opencypher_parser::parser::parse_cypher as _parse_cypher_inner;
pub use opencypher_parser::parser::parse_gql as _parse_gql_inner;

use crate::error::PolygraphError;
use opencypher_parser::ast::{CypherQuery, GqlQuery};

/// Parse an openCypher query string into a typed [`CypherQuery`] AST.
pub fn parse_cypher(input: &str) -> Result<CypherQuery, PolygraphError> {
    _parse_cypher_inner(input).map_err(Into::into)
}

/// Parse an ISO GQL query string into a typed [`GqlQuery`] AST.
pub fn parse_gql(input: &str) -> Result<GqlQuery, PolygraphError> {
    _parse_gql_inner(input).map_err(Into::into)
}
