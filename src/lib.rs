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

    // Check for definite semantic errors (duplicate aliases) that should be
    // raised as Translation errors, not silently swallowed by legacy.
    if let Some(reason) = lqa_safe_reason(ast) {
        if reason == "duplicate_alias" {
            return Err(PolygraphError::Translation {
                message: "Duplicate column name in RETURN/WITH: ColumnNameConflict".into(),
            });
        }
        if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
            eprintln!("[LEGACY] is_lqa_safe=false reason={reason}");
        }
        return Ok(None);
    }

    let mut lowerer = lqa::lower::AstLowerer::new();
    let op = match lowerer.lower_query(ast) {
        Ok(op) => op,
        Err(PolygraphError::Unsupported { ref construct, .. })
        | Err(PolygraphError::UnsupportedFeature {
            feature: ref construct,
        }) => {
            if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                eprintln!("[LEGACY] lqa_lower=Unsupported construct={construct}");
            }
            return Ok(None);
        }
        Err(e) => return Err(e),
    };

    // ── Write path ────────────────────────────────────────────────────────
    if lqa::write::contains_write(&op) {
        let base_iri = engine.base_iri();
        let cw = match lqa::write::compile_write(&op, base_iri.as_deref()) {
            Ok(cw) => cw,
            Err(PolygraphError::Unsupported { ref construct, .. }) => {
                if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                    eprintln!("[LEGACY] lqa_write_compile=Unsupported construct={construct}");
                }
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        // For the SELECT part (RETURN clause), use translate_skip_writes so that
        // SET-rewritten WHERE conditions are handled correctly.  The LQA write
        // compiler deliberately does NOT generate the SELECT (see CompiledWrite::has_return).
        let select = if cw.has_return {
            let result = match translator::cypher::translate_skip_writes(
                ast,
                engine.base_iri().as_deref(),
                engine.supports_rdf_star(),
            ) {
                Ok(r) => r,
                Err(PolygraphError::Unsupported { ref construct, .. })
                | Err(PolygraphError::UnsupportedFeature {
                    feature: ref construct,
                }) => {
                    if std::env::var("POLYGRAPH_TRACE_LEGACY").is_ok() {
                        eprintln!(
                            "[LEGACY] lqa_write_select=Unsupported construct={construct}"
                        );
                    }
                    return Ok(None); // fall back entirely to legacy path
                }
                Err(e) => return Err(e),
            };
            let sparql = match engine.finalize(result.sparql) {
                Ok(s) => s,
                Err(PolygraphError::Unsupported { .. })
                | Err(PolygraphError::UnsupportedFeature { .. }) => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
            Some(Box::new(TranspileOutput::complete(sparql, result.schema)))
        } else {
            None
        };
        return Ok(Some(TranspileOutput::Write {
            updates: cw.update_strings,
            select,
        }));
    }

    // ── Read path ─────────────────────────────────────────────────────────
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

/// Returns `None` if safe for LQA, or `Some(("reason", is_error))` where
/// `is_error=true` indicates a definite semantic error that should propagate
/// as a `PolygraphError::Translation` (not legacy fallback).
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
            Clause::Create(_) => clause_kinds.push("write"),
            Clause::Set(_) => clause_kinds.push("write"),
            Clause::Remove(_) => clause_kinds.push("write"),
            Clause::Delete(_) => clause_kinds.push("write"),
            Clause::Merge(_) => clause_kinds.push("write"),
            Clause::Call(_) => clause_kinds.push("read_call"),
            Clause::Union { .. } => clause_kinds.push("union"),
        }
    }

    // Determine whether this is a write query or a pure-read query.
    let has_write = clause_kinds.iter().any(|&k| k == "write");

    // Route queries through LQA:
    // - Pure-read queries must end with "return".
    // - Write queries can end with "write" (no RETURN) or "return" (RETURN clause after write).
    if !has_write {
        let last = clause_kinds.last().copied();
        if last != Some("return") {
            return Some("clause_shape");
        }
    } else {
        let last = clause_kinds.last().copied();
        if last != Some("write") && last != Some("return") {
            return Some("clause_shape");
        }
    }

    let mut bound_vars: HashSet<&str> = HashSet::new();
    let mut seen_with = false;
    // Track which named rel-vars are currently "live" for cross-WITH reuse.
    // A rel-var survives a WITH only if it appears as a simple identity
    // passthrough item (alias == original name, expression is a plain variable).
    // Any WITH that renames a variable (alias != source) makes the flat SPARQL
    // variable chain unreliable, so we clear live_rel_vars entirely.
    let mut live_rel_vars: HashSet<&str> = HashSet::new();
    for c in &ast.clauses {
        match c {
            Clause::With(w) => {
                seen_with = true;
                use ast::cypher::ReturnItems;
                let mut new_bound: HashSet<&str> = HashSet::new();
                if let ReturnItems::Explicit(items) = &w.items {
                    // Detect variable renames (alias ≠ source var name).
                    let has_rename = items.iter().any(|item| {
                        if let ast::cypher::Expression::Variable(v) = &item.expression {
                            item.alias.as_deref().map_or(false, |a| a != v.as_str())
                        } else {
                            false
                        }
                    });
                    if has_rename {
                        // With-variable renames break LQA's flat variable model;
                        // cross-WITH rel-var reuse is unsafe after any rename.
                        live_rel_vars.clear();
                    } else {
                        // No renames: rel-vars survive if they are identity-passthrough
                        // items in this WITH (regardless of whether aggregates also appear).
                        let surviving: HashSet<&str> = items
                            .iter()
                            .filter_map(|item| {
                                if let ast::cypher::Expression::Variable(v) = &item.expression {
                                    let alias = item.alias.as_deref().unwrap_or(v.as_str());
                                    if alias == v.as_str() && live_rel_vars.contains(v.as_str()) {
                                        return Some(v.as_str());
                                    }
                                }
                                None
                            })
                            .collect();
                        live_rel_vars = surviving;
                    }
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
                                // Named paths with varlen relationships cannot be
                                // statically resolved in the LQA path (hop count and
                                // node list are dynamic).  Fall back to legacy.
                                if pattern.variable.is_some() && r.range.is_some() {
                                    return Some("named_path_varlen");
                                }
                                // Cross-WITH rel-var handling: if a named rel-var appears
                                // after a WITH clause, only allow LQA when the var was
                                // explicitly passed through all preceding WITHs as an
                                // identity item (live_rel_vars).  Any other case (fresh
                                // var after WITH, aggregated-away var reused, renamed var)
                                // is routed to legacy to avoid BIND conflicts and
                                // cross-product issues in LQA's flat WHERE model.
                                if let Some(rv) = r.variable.as_deref() {
                                    if seen_with && !live_rel_vars.contains(rv) {
                                        return Some("relvar_after_with");
                                    }
                                    // Register this rel-var as live for potential future
                                    // cross-WITH reuse (unsafe cast away lifetime: the
                                    // string is borrowed from ast which lives for the
                                    // duration of this function).
                                    live_rel_vars.insert(rv);
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

    // If any MATCH clause uses a named path AND any RETURN/WITH clause contains
    // a real aggregate (AVG, SUM, MIN, MAX, COLLECT — not COUNT), fall back to
    // legacy.  LQA's aggregate subquery form can produce wrong results (0 rows
    // instead of 1 row with null) for empty result sets via Oxigraph, which
    // does not affect the legacy path's simpler SPARQL formulation.
    let has_named_path = ast.clauses.iter().any(|c| {
        if let Clause::Match(m) = c {
            m.pattern.0.iter().any(|p| p.variable.is_some())
        } else {
            false
        }
    });
    if has_named_path {
        let has_real_agg = ast.clauses.iter().any(|c| match c {
            Clause::Return(r) => {
                if let ReturnItems::Explicit(items) = &r.items {
                    items.iter().any(|i| expr_has_real_aggregate(&i.expression))
                } else {
                    false
                }
            }
            Clause::With(w) => {
                if let ReturnItems::Explicit(items) = &w.items {
                    items.iter().any(|i| expr_has_real_aggregate(&i.expression))
                } else {
                    false
                }
            }
            _ => false,
        });
        if has_real_agg {
            return Some("named_path_with_real_agg");
        }
    }

    None
}

/// Returns `true` if `expr` (or any sub-expression) is a real (non-Count)
/// aggregate: AVG, SUM, MIN, MAX, or COLLECT.  Used by `lqa_safe_reason` to
/// detect when a named-path query uses an aggregate that LQA cannot yet
/// generate correct null-group SPARQL for.
fn expr_has_real_aggregate(expr: &ast::cypher::Expression) -> bool {
    use ast::cypher::{AggregateExpr, Expression};
    match expr {
        Expression::Aggregate(agg) => matches!(
            agg,
            AggregateExpr::Avg { .. }
                | AggregateExpr::Sum { .. }
                | AggregateExpr::Min { .. }
                | AggregateExpr::Max { .. }
                | AggregateExpr::Collect { .. }
        ),
        Expression::Add(a, b)
        | Expression::Subtract(a, b)
        | Expression::Multiply(a, b)
        | Expression::Divide(a, b)
        | Expression::Modulo(a, b)
        | Expression::Power(a, b)
        | Expression::And(a, b)
        | Expression::Or(a, b)
        | Expression::Xor(a, b)
        | Expression::Comparison(a, _, b)
        | Expression::Subscript(a, b) => expr_has_real_aggregate(a) || expr_has_real_aggregate(b),
        Expression::Not(e)
        | Expression::Negate(e)
        | Expression::IsNull(e)
        | Expression::IsNotNull(e) => expr_has_real_aggregate(e),
        Expression::Property(e, _) => expr_has_real_aggregate(e),
        Expression::FunctionCall { args, .. } => args.iter().any(expr_has_real_aggregate),
        Expression::CaseExpression {
            operand,
            whens,
            else_expr,
        } => {
            operand.as_deref().map_or(false, expr_has_real_aggregate)
                || whens
                    .iter()
                    .any(|(w, t)| expr_has_real_aggregate(w) || expr_has_real_aggregate(t))
                || else_expr.as_deref().map_or(false, expr_has_real_aggregate)
        }
        Expression::ListComprehension {
            list,
            predicate,
            projection,
            ..
        } => {
            expr_has_real_aggregate(list)
                || predicate.as_deref().map_or(false, expr_has_real_aggregate)
                || projection.as_deref().map_or(false, expr_has_real_aggregate)
        }
        Expression::QuantifierExpr {
            list, predicate, ..
        } => {
            expr_has_real_aggregate(list)
                || predicate.as_deref().map_or(false, expr_has_real_aggregate)
        }
        Expression::ListSlice { list, start, end } => {
            expr_has_real_aggregate(list)
                || start.as_deref().map_or(false, expr_has_real_aggregate)
                || end.as_deref().map_or(false, expr_has_real_aggregate)
        }
        _ => false,
    }
}
