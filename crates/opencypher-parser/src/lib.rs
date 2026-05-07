#![forbid(unsafe_code)]
//! Standalone openCypher and ISO GQL parser.
//!
//! Parses openCypher and ISO GQL query strings into a typed AST. Has no
//! dependency on SPARQL or any execution engine — suitable for linters,
//! graph analytics tools, migration utilities, and alternative backends.
//!
//! # Quick start
//!
//! ```rust
//! use opencypher_parser::parse_cypher;
//!
//! let query = parse_cypher("MATCH (n:Person) RETURN n.name").unwrap();
//! assert!(!query.clauses.is_empty());
//! ```
//!
//! # GQL
//!
//! ```rust
//! use opencypher_parser::parse_gql;
//!
//! let query = parse_gql("MATCH (n IS Person) RETURN n.name").unwrap();
//! assert!(!query.clauses.is_empty());
//! ```

pub mod ast;
pub mod error;
pub mod parser;

pub use ast::{
    AggregateExpr, CallClause, Clause, CompOp, CreateClause, CypherQuery, DeleteClause, Direction,
    Expression, GqlQuery, Ident, Label, Literal, MapLiteral, MatchClause, MergeClause,
    NodePattern, OrderByClause, Pattern, PatternElement, PatternList, RangeQuantifier, RelType,
    RelationshipPattern, RemoveClause, RemoveItem, ReturnClause, ReturnItem, ReturnItems,
    SetClause, SetItem, SortItem, UnwindClause, WhereClause, WithClause,
};
pub use error::ParseError;
pub use parser::{parse_cypher, parse_gql};
