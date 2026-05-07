//! openCypher AST node types (Phase 1 + Phase 4).
//!
//! Covers: `MATCH`, `OPTIONAL MATCH`, `WHERE`, `RETURN`, `WITH`,
//! `ORDER BY`, `SKIP`, `LIMIT`, `UNWIND`, `CREATE`, `MERGE`, `SET`,
//! `DELETE`, `DETACH DELETE`, `REMOVE`, and `CALL` procedure stubs.
// ── Primitive aliases ────────────────────────────────────────────────────────

/// A bare identifier (variable name, label, property key, relationship type).
pub type Ident = String;
/// A node label (the part after `:` in a node pattern).
pub type Label = String;
/// A relationship type (the part after `:` inside `[...]`).
pub type RelType = String;

// ── Top-level query ──────────────────────────────────────────────────────────

/// The root of a parsed openCypher query.
#[derive(Debug, Clone, PartialEq)]
pub struct CypherQuery {
    pub clauses: Vec<Clause>,
}

// ── Clauses ──────────────────────────────────────────────────────────────────

/// A single clause within a Cypher query.
#[derive(Debug, Clone, PartialEq)]
pub enum Clause {
    Match(MatchClause),
    With(WithClause),
    Return(ReturnClause),
    Unwind(UnwindClause),
    Create(CreateClause),
    Merge(MergeClause),
    Set(SetClause),
    Delete(DeleteClause),
    Remove(RemoveClause),
    Call(CallClause),
    /// UNION [ALL] separator between two query arms.
    Union {
        all: bool,
    },
}

/// A `MATCH` or `OPTIONAL MATCH` clause, with an optional inline `WHERE`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchClause {
    pub optional: bool,
    pub pattern: PatternList,
    pub where_: Option<WhereClause>,
}

/// A `WHERE` predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub expression: Expression,
}

/// A `RETURN` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnClause {
    pub distinct: bool,
    pub items: ReturnItems,
    pub order_by: Option<OrderByClause>,
    pub skip: Option<Expression>,
    pub limit: Option<Expression>,
}

/// A `WITH` clause (projection + optional `WHERE`, ORDER BY, SKIP, LIMIT).
#[derive(Debug, Clone, PartialEq)]
pub struct WithClause {
    pub distinct: bool,
    pub items: ReturnItems,
    pub where_: Option<WhereClause>,
    pub order_by: Option<OrderByClause>,
    pub skip: Option<Expression>,
    pub limit: Option<Expression>,
}

// ── Phase 4 clauses ──────────────────────────────────────────────────────────

/// An `UNWIND` clause: `UNWIND expr AS var`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnwindClause {
    pub expression: Expression,
    pub variable: Ident,
}

/// A `CREATE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateClause {
    pub pattern: PatternList,
}

/// A `MERGE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeClause {
    pub pattern: Pattern,
    /// ON MATCH SET / ON CREATE SET actions attached to this MERGE clause.
    pub actions: Vec<MergeAction>,
}

/// An `ON MATCH SET` or `ON CREATE SET` action within a MERGE clause.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeAction {
    pub on_create: bool, // true = ON CREATE SET, false = ON MATCH SET
    pub items: Vec<SetItem>,
}

/// A `SET` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SetClause {
    pub items: Vec<SetItem>,
}

/// A single assignment in a `SET` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum SetItem {
    /// `n.prop = expr`
    Property {
        variable: Ident,
        key: Ident,
        value: Expression,
    },
    /// `n += { map }`
    MergeMap { variable: Ident, map: MapLiteral },
    /// `n = expr`
    NodeReplace { variable: Ident, value: Expression },
    /// `n:Label` — add one or more labels to a node
    SetLabel { variable: Ident, labels: Vec<Label> },
}

/// A `DELETE` or `DETACH DELETE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteClause {
    pub detach: bool,
    pub expressions: Vec<Expression>,
}

/// A `REMOVE` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoveClause {
    pub items: Vec<RemoveItem>,
}

/// A single item in a `REMOVE` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoveItem {
    /// `n.prop`
    Property { variable: Ident, key: Ident },
    /// `n:Label`
    Label { variable: Ident, labels: Vec<Label> },
}

/// A `CALL` procedure invocation stub.
#[derive(Debug, Clone, PartialEq)]
pub struct CallClause {
    /// Qualified procedure name, e.g. `apoc.path.expand`.
    pub procedure: String,
    pub args: Vec<Expression>,
    pub yields: Vec<Ident>,
}

// ── ORDER BY ─────────────────────────────────────────────────────────────────

/// An `ORDER BY` clause attached to `RETURN` or `WITH`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    pub items: Vec<SortItem>,
}

/// A single sort expression.
#[derive(Debug, Clone, PartialEq)]
pub struct SortItem {
    pub expression: Expression,
    pub descending: bool,
}

// ── Return / projection items ────────────────────────────────────────────────

/// Projection list for `RETURN` or `WITH`.
#[derive(Debug, Clone, PartialEq)]
pub enum ReturnItems {
    /// `RETURN *`
    All,
    /// `RETURN expr [AS alias], …`
    Explicit(Vec<ReturnItem>),
}

/// A single projected expression with an optional alias.
#[derive(Debug, Clone, PartialEq)]
pub struct ReturnItem {
    pub expression: Expression,
    pub alias: Option<Ident>,
}

// ── Pattern ──────────────────────────────────────────────────────────────────

/// A comma-separated list of patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct PatternList(pub Vec<Pattern>);

/// A single path pattern, optionally bound to a variable.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub variable: Option<Ident>,
    /// Alternating nodes and relationships, always starting and ending with a node.
    /// `[Node, Rel, Node, Rel, Node, …]`
    pub elements: Vec<PatternElement>,
}

/// An element within a path pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    Node(NodePattern),
    Relationship(RelationshipPattern),
}

/// A node pattern: `(variable:Label {prop: val})`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodePattern {
    pub variable: Option<Ident>,
    pub labels: Vec<Label>,
    pub properties: Option<MapLiteral>,
}

/// A relationship pattern: `-[:TYPE*range {prop: val}]->`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationshipPattern {
    pub variable: Option<Ident>,
    pub direction: Direction,
    pub rel_types: Vec<RelType>,
    pub properties: Option<MapLiteral>,
    pub range: Option<RangeQuantifier>,
}

/// The direction of a relationship arrow.
#[derive(Debug, Clone, PartialEq)]
pub enum Direction {
    /// `-->` or `-[…]->`
    Right,
    /// `<--` or `<-[…]-`
    Left,
    /// `--` or `-[…]-` (undirected)
    Both,
}

/// Variable-length range on a relationship pattern (`*`, `*2`, `*1..3`).
#[derive(Debug, Clone, PartialEq)]
pub struct RangeQuantifier {
    pub lower: Option<u64>,
    pub upper: Option<u64>,
}

// ── Expressions ──────────────────────────────────────────────────────────────

/// A Cypher expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Or(Box<Expression>, Box<Expression>),
    Xor(Box<Expression>, Box<Expression>),
    And(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    Comparison(Box<Expression>, CompOp, Box<Expression>),
    IsNull(Box<Expression>),
    IsNotNull(Box<Expression>),
    Add(Box<Expression>, Box<Expression>),
    Subtract(Box<Expression>, Box<Expression>),
    Multiply(Box<Expression>, Box<Expression>),
    Divide(Box<Expression>, Box<Expression>),
    Modulo(Box<Expression>, Box<Expression>),
    Negate(Box<Expression>),
    Power(Box<Expression>, Box<Expression>),
    /// Property access: `expr.key`
    Property(Box<Expression>, Ident),
    Variable(Ident),
    Literal(Literal),
    List(Vec<Expression>),
    Map(MapLiteral),
    /// Aggregate function call: `count(n)`, `sum(n.score)`, etc.
    Aggregate(AggregateExpr),
    /// General (non-aggregate) function call: `type(r)`, `abs(x)`, `nodes(p)`, etc.
    FunctionCall {
        name: String,
        distinct: bool,
        args: Vec<Expression>,
    },
    /// Label predicate in expression context: `n:Label` or `n:A:B`.
    LabelCheck {
        variable: Ident,
        labels: Vec<Label>,
    },
    /// Pattern predicate in expression context: `(a)-[:T]->(b:Label)`.
    /// Tests for path existence (translates to SPARQL EXISTS).
    PatternPredicate(Pattern),
    /// EXISTS subquery: `EXISTS { (a)-->(b) WHERE pred }`.
    /// Translates to SPARQL `EXISTS { bgp . FILTER pred }`.
    ExistsSubquery {
        patterns: PatternList,
        where_: Option<Box<Expression>>,
    },
    /// EXISTS subquery with full clause body (MATCH + WITH + WHERE + RETURN).
    /// `EXISTS { MATCH (n)-->(m) WITH n, count(*) AS c WHERE c > 2 RETURN true }`.
    ExistsFullSubquery {
        clauses: Vec<Clause>,
    },
    /// CASE expression: `CASE [operand] WHEN val THEN result ... [ELSE default] END`.
    CaseExpression {
        operand: Option<Box<Expression>>,
        whens: Vec<(Expression, Expression)>,
        else_expr: Option<Box<Expression>>,
    },
    /// Quantifier: `all(x IN list WHERE pred)`, `any(...)`, `none(...)`, `single(...)`.
    QuantifierExpr {
        kind: QuantifierKind,
        variable: Ident,
        list: Box<Expression>,
        predicate: Option<Box<Expression>>,
    },
    /// Subscript access: `expr[index]`.
    Subscript(Box<Expression>, Box<Expression>),
    /// List slice: `expr[start..end]` (either bound may be absent).
    ListSlice {
        list: Box<Expression>,
        start: Option<Box<Expression>>,
        end: Option<Box<Expression>>,
    },
    /// List comprehension: `[x IN list WHERE pred | projection]`.
    ListComprehension {
        variable: Ident,
        list: Box<Expression>,
        predicate: Option<Box<Expression>>,
        projection: Option<Box<Expression>>,
    },
    /// Pattern comprehension: `[(n)-[r]->(m) WHERE pred | projection]`.
    PatternComprehension {
        alias: Option<Ident>,
        pattern: Pattern,
        predicate: Option<Box<Expression>>,
        projection: Box<Expression>,
    },
}

/// Quantifier kind for `all / any / none / single` expressions.
#[derive(Debug, Clone, PartialEq)]
pub enum QuantifierKind {
    All,
    Any,
    None,
    Single,
}

/// Binary comparison operators.
#[derive(Debug, Clone, PartialEq)]
pub enum CompOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    StartsWith,
    EndsWith,
    Contains,
    RegexMatch,
}

// ── Literals ─────────────────────────────────────────────────────────────────

/// A literal value.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
}

/// A map literal: `{key: expr, …}`.
pub type MapLiteral = Vec<(Ident, Expression)>;

// ── Aggregate expressions ─────────────────────────────────────────────────────

/// An aggregate function call expression.
#[derive(Debug, Clone, PartialEq)]
pub enum AggregateExpr {
    /// `count(*)` or `count([DISTINCT] expr)`
    Count {
        distinct: bool,
        expr: Option<Box<Expression>>,
    },
    /// `sum([DISTINCT] expr)`
    Sum {
        distinct: bool,
        expr: Box<Expression>,
    },
    /// `avg([DISTINCT] expr)`
    Avg {
        distinct: bool,
        expr: Box<Expression>,
    },
    /// `min([DISTINCT] expr)`
    Min {
        distinct: bool,
        expr: Box<Expression>,
    },
    /// `max([DISTINCT] expr)`
    Max {
        distinct: bool,
        expr: Box<Expression>,
    },
    /// `collect([DISTINCT] expr)` → maps to SPARQL GROUP_CONCAT
    Collect {
        distinct: bool,
        expr: Box<Expression>,
    },
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cypher_query_holds_clauses() {
        let q = CypherQuery { clauses: vec![] };
        assert!(q.clauses.is_empty());
    }

    #[test]
    fn match_clause_optional_flag() {
        let m = MatchClause {
            optional: true,
            pattern: PatternList(vec![]),
            where_: None,
        };
        assert!(m.optional);
        assert!(m.where_.is_none());
    }

    #[test]
    fn match_clause_non_optional() {
        let m = MatchClause {
            optional: false,
            pattern: PatternList(vec![]),
            where_: None,
        };
        assert!(!m.optional);
    }

    #[test]
    fn node_pattern_fields() {
        let n = NodePattern {
            variable: Some("n".to_string()),
            labels: vec!["Person".to_string()],
            properties: None,
        };
        assert_eq!(n.variable.as_deref(), Some("n"));
        assert_eq!(n.labels, vec!["Person"]);
    }

    #[test]
    fn node_pattern_with_properties() {
        let props = vec![("age".to_string(), Expression::Literal(Literal::Integer(30)))];
        let n = NodePattern {
            variable: Some("n".to_string()),
            labels: vec![],
            properties: Some(props),
        };
        assert!(n.properties.is_some());
    }

    #[test]
    fn relationship_pattern_directions() {
        for dir in [Direction::Right, Direction::Left, Direction::Both] {
            let r = RelationshipPattern {
                variable: None,
                direction: dir.clone(),
                rel_types: vec![],
                properties: None,
                range: None,
            };
            assert_eq!(r.direction, dir);
        }
    }

    #[test]
    fn relationship_pattern_with_type_and_range() {
        let r = RelationshipPattern {
            variable: Some("r".to_string()),
            direction: Direction::Right,
            rel_types: vec!["KNOWS".to_string()],
            properties: None,
            range: Some(RangeQuantifier {
                lower: Some(1),
                upper: Some(3),
            }),
        };
        assert_eq!(r.rel_types, vec!["KNOWS"]);
        assert_eq!(r.range.as_ref().unwrap().lower, Some(1));
        assert_eq!(r.range.as_ref().unwrap().upper, Some(3));
    }

    #[test]
    fn return_items_all_variant() {
        let ri = ReturnItems::All;
        assert!(matches!(ri, ReturnItems::All));
    }

    #[test]
    fn return_item_with_alias() {
        let item = ReturnItem {
            expression: Expression::Variable("n".to_string()),
            alias: Some("node".to_string()),
        };
        assert_eq!(item.alias.as_deref(), Some("node"));
    }

    #[test]
    fn where_clause_holds_expression() {
        let wc = WhereClause {
            expression: Expression::Literal(Literal::Boolean(true)),
        };
        assert_eq!(wc.expression, Expression::Literal(Literal::Boolean(true)));
    }

    #[test]
    fn with_clause_fields() {
        let wc = WithClause {
            distinct: false,
            items: ReturnItems::All,
            where_: None,
            order_by: None,
            skip: None,
            limit: None,
        };
        assert!(!wc.distinct);
        assert!(wc.where_.is_none());
    }

    #[test]
    fn expression_literal_variants() {
        let _ = Expression::Literal(Literal::Integer(42));
        let _ = Expression::Literal(Literal::Float(std::f64::consts::PI));
        let _ = Expression::Literal(Literal::String("hello".into()));
        let _ = Expression::Literal(Literal::Boolean(false));
        let _ = Expression::Literal(Literal::Null);
    }

    #[test]
    fn expression_comparison() {
        let lhs = Box::new(Expression::Variable("a".to_string()));
        let rhs = Box::new(Expression::Literal(Literal::Integer(5)));
        let expr = Expression::Comparison(lhs, CompOp::Gt, rhs);
        assert!(matches!(expr, Expression::Comparison(_, CompOp::Gt, _)));
    }

    #[test]
    fn expression_property_access() {
        let base = Box::new(Expression::Variable("n".to_string()));
        let expr = Expression::Property(base, "name".to_string());
        assert!(matches!(expr, Expression::Property(_, _)));
    }

    #[test]
    fn range_quantifier_unbounded_upper() {
        let rq = RangeQuantifier {
            lower: Some(1),
            upper: None,
        };
        assert_eq!(rq.lower, Some(1));
        assert!(rq.upper.is_none());
    }

    // ── Phase 4 AST tests ──────────────────────────────────────────────────

    #[test]
    fn return_clause_with_order_limit() {
        let r = ReturnClause {
            distinct: false,
            items: ReturnItems::All,
            order_by: Some(OrderByClause {
                items: vec![SortItem {
                    expression: Expression::Variable("n".to_string()),
                    descending: true,
                }],
            }),
            skip: Some(Expression::Literal(Literal::Integer(10))),
            limit: Some(Expression::Literal(Literal::Integer(5))),
        };
        assert!(r.order_by.is_some());
        assert!(r.skip.is_some());
        assert!(r.limit.is_some());
    }

    #[test]
    fn unwind_clause_fields() {
        let u = UnwindClause {
            expression: Expression::Variable("list".to_string()),
            variable: "item".to_string(),
        };
        assert_eq!(u.variable, "item");
    }

    #[test]
    fn create_clause_holds_pattern_list() {
        let c = CreateClause {
            pattern: PatternList(vec![]),
        };
        assert!(c.pattern.0.is_empty());
    }

    #[test]
    fn set_item_property_variant() {
        let s = SetItem::Property {
            variable: "n".to_string(),
            key: "name".to_string(),
            value: Expression::Literal(Literal::String("Alice".to_string())),
        };
        assert!(matches!(s, SetItem::Property { .. }));
    }

    #[test]
    fn delete_clause_detach_flag() {
        let d = DeleteClause {
            detach: true,
            expressions: vec![Expression::Variable("n".to_string())],
        };
        assert!(d.detach);
        assert_eq!(d.expressions.len(), 1);
    }

    #[test]
    fn remove_clause_property_item() {
        let r = RemoveClause {
            items: vec![RemoveItem::Property {
                variable: "n".to_string(),
                key: "age".to_string(),
            }],
        };
        assert_eq!(r.items.len(), 1);
    }

    #[test]
    fn call_clause_fields() {
        let c = CallClause {
            procedure: "apoc.path.expand".to_string(),
            args: vec![],
            yields: vec!["node".to_string()],
        };
        assert_eq!(c.procedure, "apoc.path.expand");
        assert_eq!(c.yields.len(), 1);
    }

    #[test]
    fn aggregate_count_star() {
        let a = AggregateExpr::Count {
            distinct: false,
            expr: None,
        };
        assert!(matches!(a, AggregateExpr::Count { expr: None, .. }));
    }

    #[test]
    fn aggregate_sum_distinct() {
        let a = AggregateExpr::Sum {
            distinct: true,
            expr: Box::new(Expression::Variable("n".to_string())),
        };
        assert!(matches!(a, AggregateExpr::Sum { distinct: true, .. }));
    }

    #[test]
    fn aggregate_collect() {
        let a = AggregateExpr::Collect {
            distinct: false,
            expr: Box::new(Expression::Variable("n".to_string())),
        };
        assert!(matches!(a, AggregateExpr::Collect { .. }));
    }

    #[test]
    fn pattern_elements_roundtrip() {
        let pattern = Pattern {
            variable: None,
            elements: vec![
                PatternElement::Node(NodePattern {
                    variable: Some("a".to_string()),
                    labels: vec!["Person".to_string()],
                    properties: None,
                }),
                PatternElement::Relationship(RelationshipPattern {
                    variable: None,
                    direction: Direction::Right,
                    rel_types: vec!["KNOWS".to_string()],
                    properties: None,
                    range: None,
                }),
                PatternElement::Node(NodePattern {
                    variable: Some("b".to_string()),
                    labels: vec!["Person".to_string()],
                    properties: None,
                }),
            ],
        };
        assert_eq!(pattern.elements.len(), 3);
    }
}
