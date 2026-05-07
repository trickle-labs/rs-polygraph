/// ISO GQL AST node types (Phase 5).
///
/// The core GQL read-query constructs (`MATCH`, `FILTER`/`WHERE`, `RETURN`,
/// `NEXT`) map 1-to-1 onto equivalent openCypher clauses. Rather than
/// duplicating all clause/pattern/expression types, `GqlQuery` stores a
/// `Vec<crate::ast::cypher::Clause>` so the GQL translator can delegate
/// directly to the Cypher translator.
///
/// GQL-specific constructs handled during parsing:
/// - `(n IS Person)` node type predicate → treated as `(n:Person)`
/// - `FILTER expr` → treated as a standalone WHERE
/// - `NEXT` → treated as a scope boundary (mapped to `WITH *`)
/// - `-[r IS KNOWS]->` edge type predicate → treated as `-[r:KNOWS]->`
/// - `IS Label1 & Label2` multiple labels → two `:Label` entries
use crate::ast::cypher::Clause;

/// The root of a parsed ISO GQL query.
#[derive(Debug, Clone, PartialEq)]
pub struct GqlQuery {
    /// GQL clauses lowered to their equivalent openCypher representations.
    pub clauses: Vec<Clause>,
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::cypher::{MatchClause, PatternList};

    #[test]
    fn gql_query_holds_clauses() {
        let q = GqlQuery { clauses: vec![] };
        assert!(q.clauses.is_empty());
    }

    #[test]
    fn gql_query_with_match_clause() {
        let m = MatchClause {
            optional: false,
            pattern: PatternList(vec![]),
            where_: None,
        };
        let q = GqlQuery {
            clauses: vec![Clause::Match(m)],
        };
        assert_eq!(q.clauses.len(), 1);
        assert!(matches!(q.clauses[0], Clause::Match(_)));
    }
}
