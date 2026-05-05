#![forbid(unsafe_code)]

//! `polygraph` — transpile openCypher and ISO GQL queries to SPARQL 1.1.
//!
//! Phases 1–4 are complete:
//! - Phase 1: openCypher parser + AST
//! - Phase 2: SPARQL algebra translator (MATCH/WHERE/RETURN/WITH/OPTIONAL)
//! - Phase 3: RDF-star and reification edge property encoding
//! - Phase 4: ORDER BY/SKIP/LIMIT, aggregation, UNWIND, variable-length paths,
//!   multi-type relationships, IN list literals, write clause stubs
//!
//! Use [`sparql_engine::RdfStar`] for engines that support SPARQL-star natively, or
//! [`sparql_engine::GenericSparql11`] for standard SPARQL 1.1.
//!
//! # Example
//!
//! ```rust
//! use polygraph::parser::parse_cypher;
//!
//! let ast = parse_cypher("MATCH (n:Person) WHERE n.age > 30 RETURN n.name").unwrap();
//! println!("{ast:#?}");
//! ```

pub mod ast;
pub mod error;
pub mod lqa;
pub mod parser;
pub mod rdf_mapping;
pub mod result_mapping;
pub mod sparql_engine;
pub mod translator;

pub use error::PolygraphError;
pub use result_mapping::{
    BindingRow, CypherRow, CypherValue, ProjectionSchema, RdfTerm, SparqlSolution, TranspileOutput,
};

/// The main entry point for transpilation operations.
///
/// Transpilation methods beyond parsing are planned for Phase 2 and later.
pub struct Transpiler;

impl Transpiler {
    /// Parse an openCypher query string and return a typed AST.
    ///
    /// This is the stable Phase 1 API. Transpilation to SPARQL is
    /// implemented in Phase 2 via [`Self::cypher_to_sparql`].
    pub fn parse_cypher(cypher: &str) -> Result<ast::CypherQuery, PolygraphError> {
        parser::parse_cypher(cypher)
    }

    /// Transpile an openCypher query to SPARQL.
    ///
    /// Returns a [`TranspileOutput`] containing the SPARQL string and a
    /// projection schema for result mapping.
    ///
    /// The `engine` is consulted for engine-specific capabilities (RDF-star,
    /// federation). The optional `base_iri` on the engine is used as the
    /// namespace for labels, relationship types and property names.
    ///
    /// # Example
    ///
    /// ```rust
    /// use polygraph::{Transpiler, sparql_engine::GenericSparql11};
    ///
    /// let engine = GenericSparql11;
    /// let output = Transpiler::cypher_to_sparql(
    ///     "MATCH (n:Person) WHERE n.age > 30 RETURN n.name",
    ///     &engine,
    /// ).unwrap();
    /// assert!(output.sparql.contains("SELECT"));
    /// ```
    pub fn cypher_to_sparql(
        cypher: &str,
        engine: &dyn sparql_engine::TargetEngine,
    ) -> Result<TranspileOutput, PolygraphError> {
        let ast = parser::parse_cypher(cypher)?;
        // Phase 4.5: try the LQA path first; fall back to legacy on Unsupported.
        if let Some(output) = try_lqa_path(&ast, engine)? {
            return Ok(output);
        }
        let result =
            translator::cypher::translate(&ast, engine.base_iri(), engine.supports_rdf_star())?;
        let sparql = engine.finalize(result.sparql)?;
        Ok(TranspileOutput::complete(sparql, result.schema))
    }

    /// Like `cypher_to_sparql` but silently skips write clauses (SET/REMOVE/MERGE/CREATE/DELETE).
    /// The caller is responsible for executing write operations separately.
    pub fn cypher_to_sparql_skip_writes(
        cypher: &str,
        engine: &dyn sparql_engine::TargetEngine,
    ) -> Result<TranspileOutput, PolygraphError> {
        let ast = parser::parse_cypher(cypher)?;
        let result = translator::cypher::translate_skip_writes(
            &ast,
            engine.base_iri(),
            engine.supports_rdf_star(),
        )?;
        let sparql = engine.finalize(result.sparql)?;
        Ok(TranspileOutput::complete(sparql, result.schema))
    }

    /// Transpile an ISO GQL query to SPARQL.
    ///
    /// Returns a [`TranspileOutput`] containing the SPARQL string and a
    /// projection schema for result mapping.
    ///
    /// GQL-specific syntax (`IS Label`, `FILTER`, `NEXT`) is lowered to
    /// Cypher-equivalent constructs during parsing, so translation reuses
    /// the Cypher algebra translator.
    ///
    /// # Example
    ///
    /// ```rust
    /// use polygraph::{Transpiler, sparql_engine::GenericSparql11};
    ///
    /// let engine = GenericSparql11;
    /// let output = Transpiler::gql_to_sparql(
    ///     "MATCH (n:Person) WHERE n.age > 30 RETURN n.name",
    ///     &engine,
    /// ).unwrap();
    /// assert!(output.sparql.contains("SELECT"));
    /// ```
    pub fn gql_to_sparql(
        gql: &str,
        engine: &dyn sparql_engine::TargetEngine,
    ) -> Result<TranspileOutput, PolygraphError> {
        let ast = parser::parse_gql(gql)?;
        // GQL parsing produces a GqlQuery whose clauses are Cypher-equivalent.
        // Wrap them in a CypherQuery so the shared LQA path can handle them.
        let cypher_ast = ast::CypherQuery {
            clauses: ast.clauses.clone(),
        };
        if let Some(output) = try_lqa_path(&cypher_ast, engine)? {
            return Ok(output);
        }
        let result =
            translator::gql::translate(&ast, engine.base_iri(), engine.supports_rdf_star())?;
        let sparql = engine.finalize(result.sparql)?;
        Ok(TranspileOutput::complete(sparql, result.schema))
    }
}

/// Attempt to transpile `ast` via the LQA IR path.
///
/// Returns `Ok(Some(output))` on success, `Ok(None)` when the query contains
/// constructs not yet handled by the LQA path (triggering legacy fallback),
/// and `Err(e)` for unexpected errors (parse failures, etc.).
fn try_lqa_path(
    ast: &ast::CypherQuery,
    engine: &dyn sparql_engine::TargetEngine,
) -> Result<Option<TranspileOutput>, PolygraphError> {
    translator::cypher::check_semantics(ast)?;

    if !is_lqa_safe(ast) {
        if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
            eprintln!("[LEGACY] is_lqa_safe=false");
        }
        return Ok(None);
    }

    let mut lowerer = lqa::lower::AstLowerer::new();
    let op = match lowerer.lower_query(ast) {
        Ok(op) => op,
        Err(PolygraphError::Unsupported { ref construct, .. })
        | Err(PolygraphError::UnsupportedFeature { feature: ref construct }) => {
            if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                eprintln!("[LEGACY] lqa_lower=Unsupported construct={construct}");
            }
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    let compiled = match lqa::sparql::compile(&op, engine.base_iri().as_deref()) {
        Ok(c) => c,
        Err(PolygraphError::Unsupported { ref construct, .. })
        | Err(PolygraphError::UnsupportedFeature {
            feature: ref construct,
        }) => {
            if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                eprintln!("[LEGACY] lqa_compile=Unsupported construct={construct}");
            }
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    let sparql = match engine.finalize(compiled.sparql) {
        Ok(s) => s,
        Err(PolygraphError::Unsupported { .. })
        | Err(PolygraphError::UnsupportedFeature { .. }) => {
            if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                eprintln!("[LEGACY] finalize=Unsupported");
            }
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    Ok(Some(TranspileOutput::complete(sparql, compiled.schema)))
}

/// Check that every variable reference in `expr` is present in `scope`.
/// Used to validate ORDER BY expressions in WITH clauses after scope has been
/// restricted by a previous WITH projection.
fn sort_expr_in_scope(
    expr: &ast::cypher::Expression,
    scope: &std::collections::HashSet<String>,
) -> bool {
    use ast::cypher::Expression;
    match expr {
        Expression::Variable(v) => scope.contains(v.as_str()),
        Expression::Property(base, _) => sort_expr_in_scope(base, scope),
        Expression::FunctionCall { args, .. } => args.iter().all(|a| sort_expr_in_scope(a, scope)),
        Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b)
        | Expression::Modulo(a, b)
        | Expression::Power(a, b)
        | Expression::And(a, b)
        | Expression::Or(a, b)
        | Expression::Xor(a, b) => sort_expr_in_scope(a, scope) && sort_expr_in_scope(b, scope),
        Expression::Not(e)
        | Expression::Negate(e)
        | Expression::IsNull(e)
        | Expression::IsNotNull(e) => sort_expr_in_scope(e, scope),
        Expression::Comparison(a, _, b) => {
            sort_expr_in_scope(a, scope) && sort_expr_in_scope(b, scope)
        }
        // Literals, maps, and other non-variable expressions are always in scope.
        _ => true,
    }
}

/// Return `true` only if the query can be safely transpiled through the LQA
/// path without risk of producing semantically wrong SPARQL.
fn is_lqa_safe(ast: &ast::CypherQuery) -> bool {
    match lqa_safe_reason(ast) {
        None => true,
        Some(reason) => {
            if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                eprintln!("[LEGACY] is_lqa_safe=false reason={reason}");
            }
            false
        }
    }
}

/// Returns `None` if safe for LQA, or `Some(reason)` describing the blocking construct.
fn lqa_safe_reason(ast: &ast::CypherQuery) -> Option<&'static str> {
    use ast::cypher::{Clause, Expression, PatternElement, ReturnItems};
    use std::collections::HashSet;

    let mut clause_kinds: Vec<&str> = Vec::new();
    let mut clause_scope: Option<HashSet<String>> = None;

    for c in &ast.clauses {
        match c {
            Clause::Match(m) => {
                clause_kinds.push("match");
                if m.optional {
                    clause_kinds.push("optional_match");
                }
            }
            Clause::Return(r) => {
                clause_kinds.push("return");
                // Check for duplicate column aliases (ColumnNameConflict → legacy raises SyntaxError).
                if let ReturnItems::Explicit(items) = &r.items {
                    let mut seen_aliases: HashSet<&str> = HashSet::new();
                    for item in items {
                        let alias = item.alias.as_deref().or_else(|| {
                            if let Expression::Variable(v) = &item.expression {
                                Some(v.as_str())
                            } else {
                                None
                            }
                        });
                        if let Some(alias) = alias {
                            if !seen_aliases.insert(alias) {
                                return Some("duplicate_alias");
                            }
                        }
                    }
                }
            }
            Clause::With(w) => {
                clause_kinds.push("with");
                // Check for duplicate aliases in WITH (ColumnNameConflict → legacy raises SyntaxError).
                if let ReturnItems::Explicit(items) = &w.items {
                    let mut seen_aliases: HashSet<&str> = HashSet::new();
                    for item in items {
                        let alias = item.alias.as_deref().or_else(|| {
                            if let Expression::Variable(v) = &item.expression {
                                Some(v.as_str())
                            } else {
                                None
                            }
                        });
                        if let Some(alias) = alias {
                            if !seen_aliases.insert(alias) {
                                return Some("duplicate_alias");
                            }
                        }
                    }
                }
                if let Some(ref scope) = clause_scope {
                    // Note: with_orderby_out_of_scope guard removed. In LQA's flat WHERE
                    // clause, pre-WITH variables remain in SPARQL scope, so ORDER BY
                    // expressions that reference them still work correctly even if they
                    // wouldn't be in scope in legacy's sub-SELECT model.
                    //
                    // Block LQA when ORDER BY is present AND a non-passthrough alias
                    // shadows a variable from the previous scope.  LQA flattens WITH
                    // clauses into a single SPARQL WHERE block, so re-binding an
                    // already-bound variable (e.g. `WITH x % 3 AS x ORDER BY x`)
                    // produces an invalid SPARQL "SELECT overrides existing variable".
                    if w.order_by.is_some() {
                        if let ReturnItems::Explicit(items) = &w.items {
                            for item in items {
                                let alias_opt = item.alias.as_deref().or_else(|| {
                                    if let Expression::Variable(v) = &item.expression {
                                        Some(v.as_str())
                                    } else {
                                        None
                                    }
                                });
                                if let Some(alias) = alias_opt {
                                    let is_passthrough = matches!(
                                        &item.expression,
                                        Expression::Variable(v) if v.as_str() == alias
                                    );
                                    if !is_passthrough && scope.contains(alias) {
                                        return Some("with_orderby_shadow_alias");
                                    }
                                }
                            }
                        }
                    }
                }
                if let ReturnItems::Explicit(items) = &w.items {
                    let new_scope: HashSet<String> = items
                        .iter()
                        .filter_map(|item| {
                            if let Some(alias) = &item.alias {
                                Some(alias.clone())
                            } else if let Expression::Variable(v) = &item.expression {
                                Some(v.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    clause_scope = Some(new_scope);
                }
            }
            Clause::Unwind(_) => clause_kinds.push("unwind"),
            Clause::Create(_) => return Some("write_create"),
            Clause::Set(_) => return Some("write_set"),
            Clause::Remove(_) => return Some("write_remove"),
            Clause::Delete(_) => return Some("write_delete"),
            Clause::Merge(_) => return Some("write_merge"),
            Clause::Call(_) => return Some("write_call"),
            Clause::Union { .. } => clause_kinds.push("union"),
        }
    }

    // Route queries through LQA for any shape ending in "return", unless it is a
    // bare-RETURN UNION shape (RETURN…UNION…RETURN) whose column semantics require
    // the legacy path.  Previously this guard also excluded WITH-first and bare
    // RETURN shapes; those are now handled by LQA after fixing integer division and
    // boolean type-error detection.
    {
        let last = clause_kinds.last().copied();
        // Detect "RETURN … UNION … RETURN" (first clause is already "return").
        let has_bare_return_union =
            clause_kinds.first() == Some(&"return") && clause_kinds.contains(&"union");
        if last != Some("return") || has_bare_return_union {
            return Some("clause_shape");
        }
    }

    let mut bound_vars: HashSet<&str> = HashSet::new();
    let mut seen_with = false;
    for c in &ast.clauses {
        match c {
            Clause::With(w) => {
                seen_with = true;
                use ast::cypher::ReturnItems;
                if let ReturnItems::Explicit(items) = &w.items {
                    let mut new_bound: HashSet<&str> = HashSet::new();
                    for item in items {
                        if let Some(alias) = item.alias.as_deref() {
                            new_bound.insert(alias);
                        } else if let ast::cypher::Expression::Variable(v) = &item.expression {
                            new_bound.insert(v.as_str());
                        }
                    }
                    bound_vars = new_bound;
                }
            }
            Clause::Match(m) => {
                for pattern in &m.pattern.0 {
                    if pattern.variable.is_some() {
                        return Some("named_path");
                    }
                    for (idx, elem) in pattern.elements.iter().enumerate() {
                        match elem {
                            PatternElement::Node(n) => {
                                if let Some(v) = n.variable.as_deref() {
                                    bound_vars.insert(v);
                                }
                            }
                            PatternElement::Relationship(r) => {
                                if r.range.is_some() && r.properties.is_some() {
                                    return Some("varlen_rel_props");
                                }
                                if r.range.is_some() && r.variable.is_some() {
                                    return Some("varlen_named_relvar");
                                }
                                if r.variable.is_some() && seen_with {
                                    return Some("relvar_after_with");
                                }
                                if let Some(range) = &r.range {
                                    if range.upper.is_none() {
                                        use ast::cypher::Direction;
                                        let endpoint_idxs: Vec<usize> = match r.direction {
                                            Direction::Right => vec![idx + 1],
                                            Direction::Left => {
                                                if idx > 0 {
                                                    vec![idx - 1]
                                                } else {
                                                    vec![]
                                                }
                                            }
                                            Direction::Both => {
                                                let mut v = vec![idx + 1];
                                                if idx > 0 {
                                                    v.push(idx - 1);
                                                }
                                                v
                                            }
                                        };
                                        for ep_idx in endpoint_idxs {
                                            if let Some(PatternElement::Node(ep)) =
                                                pattern.elements.get(ep_idx)
                                            {
                                                let unlabeled = ep.labels.is_empty();
                                                let unbound = ep
                                                    .variable
                                                    .as_deref()
                                                    .map(|v| !bound_vars.contains(v))
                                                    .unwrap_or(true);
                                                if unlabeled && unbound {
                                                    return Some("unbounded_varlen_unlabeled");
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    None
}
