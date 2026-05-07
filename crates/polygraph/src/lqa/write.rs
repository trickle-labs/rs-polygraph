//! Phase 8 — Write-clause LQA → SPARQL UPDATE lowering.
//!
//! This module compiles LQA [`Op`] trees that contain write operators
//! (CREATE / SET / DELETE / REMOVE / MERGE) into SPARQL UPDATE strings
//! that can be executed against a SPARQL endpoint.
//!
//! The generated strings are valid SPARQL 1.1 Update (SPARQL Update
//! Language, W3C 2013) statements suitable for Oxigraph's `store.update()`
//! API.
//!
//! # Strategy
//!
//! 1. Walk the Op tree peeling off write operators.
//! 2. For each write operator, generate one or more SPARQL UPDATE strings.
//! 3. The "match context" (innermost read-only Op) is converted to WHERE
//!    clause triple patterns using [`op_to_where_parts`].
//! 4. If the query has a RETURN clause (Projection at the top), the write
//!    operators are stripped and the remaining read-only tree is compiled by
//!    [`crate::lqa::sparql::compile`] to produce the SELECT.

use std::collections::HashMap;

use crate::error::PolygraphError;
use crate::lqa::expr::{CmpOp, Expr, Literal, UnaryOp};
use crate::lqa::op::{CreateEdge, CreateNode, Direction, MergeClause, Op, RemoveItem, SetItem};

/// Convenience macro to construct an `Unsupported` error for write-clause
/// constructs.  All write fallbacks use the same spec reference.
macro_rules! write_unsupported {
    ($construct:literal) => {
        PolygraphError::Unsupported {
            construct: $construct.into(),
            spec_ref: "openCypher 9 §6".into(),
            reason: "write construct not yet supported by LQA write compiler".into(),
        }
    };
    ($construct:expr) => {
        PolygraphError::Unsupported {
            construct: $construct,
            spec_ref: "openCypher 9 §6".into(),
            reason: "write construct not yet supported by LQA write compiler".into(),
        }
    };
}

// ── RDF / XSD constants ───────────────────────────────────────────────────────

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
const DEFAULT_BASE: &str = "http://polygraph.example/";

// ── Public result type ────────────────────────────────────────────────────────

/// The compiled output of a write query.
pub struct CompiledWrite {
    /// Ordered SPARQL UPDATE strings to execute.
    pub update_strings: Vec<String>,
    /// `true` when the original query had a `RETURN` clause (i.e., the caller
    /// should run a SELECT after applying the updates).  The SELECT itself is
    /// NOT compiled here — the caller is responsible for generating it, because
    /// the correct SELECT must be aware of SET-rewritten WHERE conditions (which
    /// requires the legacy `translate_skip_writes` machinery).
    pub has_return: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Top-level entry point: compile a write Op tree.
///
/// Returns the UPDATE strings and a flag indicating whether a RETURN clause is
/// present.  The caller (see `try_lqa_path`) is responsible for generating the
/// correct SELECT query using `translator::cypher::translate_skip_writes` so
/// that SET-rewritten WHERE conditions are handled properly.
///
/// # Errors
///
/// Returns `Err(PolygraphError::Unsupported)` when the Op tree contains
/// constructs that cannot yet be compiled by this module.  The caller
/// is expected to fall back to the legacy translator in that case.
pub fn compile_write(op: &Op, base_iri: Option<&str>) -> Result<CompiledWrite, PolygraphError> {
    let base = base_iri.unwrap_or(DEFAULT_BASE);

    // Detect whether the query has a RETURN clause (Projection anywhere in the tree).
    let has_return = contains_projection(op);

    // DELETE+RETURN queries: the RETURN should reflect pre-deletion row counts.
    // The legacy translate_skip_writes path handles this correctly by skipping
    // the DELETE execution and counting matched rows before deletion.
    // Fall back so the legacy path can produce the correct result.
    if has_return && contains_delete(op) {
        return Err(write_unsupported!("write_delete_with_return"));
    }

    // Make sure this is actually a write-containing tree.
    if !contains_write(op) {
        return Err(write_unsupported!(
            "compile_write: op tree has no write operator"
        ));
    }

    // ── Generate UPDATE strings ───────────────────────────────────────────
    let mut counter = 0usize;
    let mut bnode_map: HashMap<String, String> = HashMap::new();
    let mut updates = Vec::new();
    compile_write_recursive(op, base, &mut counter, &mut bnode_map, &mut updates)?;

    Ok(CompiledWrite {
        update_strings: updates,
        has_return,
    })
}

// ── Recursive write-op visitor ────────────────────────────────────────────────

/// Walk `op` depth-first, collecting UPDATE strings into `out`.
/// Stops recursing at non-write ops (read context).
fn compile_write_recursive(
    op: &Op,
    base: &str,
    counter: &mut usize,
    bnode_map: &mut HashMap<String, String>,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    match op {
        Op::Create {
            inner,
            nodes,
            edges,
        } => {
            let match_ctx = read_context(inner);
            let where_parts = op_to_where_parts(match_ctx, base)?;

            // If inner also contains writes, compile those first.
            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_create(nodes, edges, &where_parts, base, counter, bnode_map, out)?;
        }
        Op::Set { inner, items } => {
            let match_ctx = read_context(inner);
            let where_parts = op_to_where_parts(match_ctx, base)?;

            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_set_items(items, &where_parts, base, out)?;
        }
        Op::Delete {
            inner,
            detach,
            exprs,
        } => {
            let match_ctx = read_context(inner);
            let where_parts = op_to_where_parts(match_ctx, base)?;

            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_delete(exprs, *detach, &where_parts, base, out)?;
        }
        Op::Remove { inner, items } => {
            let match_ctx = read_context(inner);
            let where_parts = op_to_where_parts(match_ctx, base)?;

            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_remove_items(items, &where_parts, base, out)?;
        }
        Op::Merge { inner, clause } => {
            let match_ctx = read_context(inner);
            let where_parts = op_to_where_parts(match_ctx, base)?;

            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_merge(clause, &where_parts, base, counter, out)?;
        }
        // CALL / FOREACH — not yet implemented; fall back to legacy
        Op::Call { .. } | Op::Foreach { .. } => {
            return Err(write_unsupported!("write_call_foreach"));
        }
        // Read-only wrapper op that may contain write ops deeper in the tree
        // (e.g. GroupBy wrapping a Merge for `MERGE … RETURN count(*)`).
        // Recurse through the wrapper to reach the write ops.
        Op::GroupBy { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Distinct { inner }
        | Op::Selection { inner, .. }
        | Op::Unwind { inner, .. }
        | Op::Expand { inner, .. }
        | Op::Projection { inner, .. }
            if contains_write(inner) =>
        {
            compile_write_recursive(inner, base, counter, bnode_map, out)?;
        }
        // Pure read op with no writes below — nothing to generate.
        _ => {}
    }
    Ok(())
}

// ── Strip writes ──────────────────────────────────────────────────────────────

/// Return a copy of `op` with all write operators removed, keeping the
/// Projection and the innermost read-only subtree.
///
/// Used to extract the SELECT op for RETURN-clause queries (Phase 8.7).
#[allow(dead_code)]
fn strip_writes(op: &Op) -> Op {
    match op {
        Op::Projection {
            inner,
            items,
            distinct,
        } => Op::Projection {
            inner: Box::new(strip_writes(inner)),
            items: items.clone(),
            distinct: *distinct,
        },
        Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. } => strip_writes(inner),
        // Read op — return as-is
        _ => op.clone(),
    }
}

/// Find the innermost contiguous read-only subtree
/// (the "match context" that supplies the WHERE clause).
fn read_context(op: &Op) -> &Op {
    match op {
        Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. } => read_context(inner),
        _ => op,
    }
}

/// Returns `true` if `op` contains a Delete operator anywhere in its subtree.
fn contains_delete(op: &Op) -> bool {
    match op {
        Op::Delete { .. } => true,
        Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. }
        | Op::Projection { inner, .. }
        | Op::Selection { inner, .. }
        | Op::Expand { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Distinct { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Unwind { inner, .. } => contains_delete(inner),
        Op::CartesianProduct { left, right }
        | Op::Union { left, right }
        | Op::UnionAll { left, right } => contains_delete(left) || contains_delete(right),
        Op::LeftOuterJoin { left, right, .. } => contains_delete(left) || contains_delete(right),
        Op::Subquery { outer, inner } => contains_delete(outer) || contains_delete(inner),
        Op::Scan { .. } | Op::Unit | Op::Values { .. } => false,
    }
}

/// Returns `true` if `op` contains any write operator in its subtree.
pub fn contains_write(op: &Op) -> bool {
    match op {
        Op::Create { .. }
        | Op::Set { .. }
        | Op::Delete { .. }
        | Op::Remove { .. }
        | Op::Merge { .. }
        | Op::Call { .. }
        | Op::Foreach { .. } => true,
        Op::Projection { inner, .. }
        | Op::Selection { inner, .. }
        | Op::Expand { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Distinct { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Unwind { inner, .. } => contains_write(inner),
        Op::CartesianProduct { left, right }
        | Op::Union { left, right }
        | Op::UnionAll { left, right } => contains_write(left) || contains_write(right),
        Op::LeftOuterJoin { left, right, .. } => contains_write(left) || contains_write(right),
        Op::Subquery { outer, inner } => contains_write(outer) || contains_write(inner),
        Op::Scan { .. } | Op::Unit | Op::Values { .. } => false,
    }
}

/// Returns `true` if `op` contains a Projection anywhere in its subtree,
/// indicating the query has a RETURN clause.
fn contains_projection(op: &Op) -> bool {
    match op {
        Op::Projection { .. } => true,
        Op::Selection { inner, .. }
        | Op::Expand { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Distinct { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Unwind { inner, .. }
        | Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. } => contains_projection(inner),
        Op::CartesianProduct { left, right }
        | Op::Union { left, right }
        | Op::UnionAll { left, right } => contains_projection(left) || contains_projection(right),
        Op::LeftOuterJoin { left, right, .. } => {
            contains_projection(left) || contains_projection(right)
        }
        Op::Subquery { outer, inner } => contains_projection(outer) || contains_projection(inner),
        Op::Scan { .. } | Op::Unit | Op::Values { .. } => false,
    }
}

// ── WHERE clause generator ─────────────────────────────────────────────────────

/// Generate SPARQL triple pattern strings from a read-only Op for use in
/// the WHERE clause of SPARQL UPDATE statements.
///
/// Returns `Err(Unsupported)` for Op kinds that cannot be statically
/// expressed as simple triple patterns.
fn op_to_where_parts(op: &Op, base: &str) -> Result<Vec<String>, PolygraphError> {
    match op {
        Op::Unit => Ok(Vec::new()),

        Op::Scan {
            variable,
            label,
            extra_labels,
        } => {
            let mut parts = Vec::new();
            // Universal node-existence sentinel.
            parts.push(format!("?{variable} <{base}__node> <{base}__node>"));
            if let Some(lbl) = label {
                parts.push(format!("?{variable} <{RDF_TYPE}> <{base}{lbl}>"));
            }
            for lbl in extra_labels {
                parts.push(format!("?{variable} <{RDF_TYPE}> <{base}{lbl}>"));
            }
            Ok(parts)
        }

        Op::Selection { inner, predicate } => {
            let mut parts = op_to_where_parts(inner, base)?;
            push_predicate_parts(predicate, base, &mut parts);
            Ok(parts)
        }

        Op::Expand {
            inner,
            from,
            rel_var: _,
            to,
            rel_types,
            direction,
            range: None,
            path_var: _,
        } => {
            let mut parts = op_to_where_parts(inner, base)?;
            if rel_types.is_empty() {
                // Untyped relationship: use a variable predicate.
                let pred_var = format!("?__pred_{}_{}", from, to);
                match direction {
                    Direction::Outgoing => {
                        parts.push(format!("?{from} {pred_var} ?{to}"));
                    }
                    Direction::Incoming => {
                        parts.push(format!("?{to} {pred_var} ?{from}"));
                    }
                    Direction::Undirected => {
                        parts.push(format!(
                            "{{ ?{from} {pred_var} ?{to} }} UNION {{ ?{to} {pred_var} ?{from} }}"
                        ));
                    }
                }
            } else {
                for rt in rel_types {
                    let type_iri = format!("{base}{rt}");
                    match direction {
                        Direction::Outgoing => {
                            parts.push(format!("?{from} <{type_iri}> ?{to}"));
                        }
                        Direction::Incoming => {
                            parts.push(format!("?{to} <{type_iri}> ?{from}"));
                        }
                        Direction::Undirected => {
                            parts.push(format!(
                                "{{ ?{from} <{type_iri}> ?{to} }} UNION {{ ?{to} <{type_iri}> ?{from} }}"
                            ));
                        }
                    }
                }
            }
            Ok(parts)
        }

        // Variable-length expand — fall back to legacy.
        Op::Expand { range: Some(_), .. } => Err(write_unsupported!("write_where_varlen_expand")),

        Op::CartesianProduct { left, right } => {
            let mut parts = op_to_where_parts(left, base)?;
            parts.extend(op_to_where_parts(right, base)?);
            Ok(parts)
        }

        Op::LeftOuterJoin { left, right, .. } => {
            let mut parts = op_to_where_parts(left, base)?;
            let right_parts = op_to_where_parts(right, base)?;
            if !right_parts.is_empty() {
                parts.push(format!("OPTIONAL {{ {} }}", right_parts.join(" . ")));
            }
            Ok(parts)
        }

        Op::Values { bindings } => {
            // Single-row VALUES (e.g. from a WITH literal) — convert to BIND/VALUES.
            let mut parts = Vec::new();
            for (var, val) in bindings {
                if let Some(lit_str) = expr_to_sparql_lit(val, base) {
                    parts.push(format!("VALUES (?{var}) {{ ({lit_str}) }}"));
                }
            }
            Ok(parts)
        }

        // Transparent read-side wrappers: ordering and distinctness don't add WHERE triples
        // or change variable names — just recurse into the inner op.
        // Note: Limit/Skip cannot be transparently stripped because they restrict which rows
        // to update (SPARQL UPDATE has no LIMIT/SKIP concept).
        Op::OrderBy { inner, .. } | Op::Distinct { inner, .. } => op_to_where_parts(inner, base),

        // Projection (WITH clause): handle identity passthroughs and simple variable
        // renames (e.g. `WITH n AS a`).  Both cases can be expressed in SPARQL via
        // BIND:  non-rename passthroughs need no BIND; renames add `BIND(?src AS ?alias)`.
        // Projections containing computed expressions (e.g. `WITH n.name AS x`) are
        // not yet supported and fall back to legacy.
        Op::Projection { inner, items, .. } => {
            // Check whether all items are plain variable expressions (passthrough or rename).
            let all_variable_items = items
                .iter()
                .all(|pi| matches!(&pi.expr, crate::lqa::Expr::Variable { .. }));
            if all_variable_items {
                let mut parts = op_to_where_parts(inner, base)?;
                // Emit BIND clauses for every non-identity rename: `WITH n AS a` → `BIND(?n AS ?a)`.
                for pi in items {
                    if let crate::lqa::Expr::Variable { name: src, .. } = &pi.expr {
                        if src != &pi.alias {
                            parts.push(format!("BIND(?{src} AS ?{})", pi.alias));
                        }
                    }
                }
                Ok(parts)
            } else {
                Err(write_unsupported!("write_where_complex_op"))
            }
        }

        // GroupBy: recurse into the inner (the MATCH pattern) to generate WHERE triples.
        // The group keys don't change which nodes are matched, only which rows are returned.
        Op::GroupBy { inner, .. } => op_to_where_parts(inner, base),

        // LIMIT / SKIP in a write context cannot be emulated safely in SPARQL UPDATE
        // (there is no SPARQL-level LIMIT on UPDATE rows). Fall back to legacy.
        Op::Limit { .. } | Op::Skip { .. } => Err(write_unsupported!("write_limit_skip_context")),

        // For other complex ops in write context — fall back so the whole query
        // is retried on the legacy path, which has richer WHERE generation.
        _ => Err(write_unsupported!("write_where_complex_op")),
    }
}

/// Attempt to push SPARQL triple patterns or FILTER clauses for a predicate
/// expression.  Simple property-equality predicates are emitted as triple
/// patterns; other predicates are emitted as FILTER expressions where
/// possible; unhandled predicates are silently skipped (conservative: may
/// update more rows than intended but will not produce incorrect results for
/// the RETURN clause, which is compiled separately by the LQA SELECT path).
fn push_predicate_parts(expr: &Expr, base: &str, parts: &mut Vec<String>) {
    match expr {
        // `n.prop = lit` → triple pattern (most efficient)
        Expr::Comparison(CmpOp::Eq, left, right) => {
            if let Some(triple) = eq_to_triple_pattern(left, right, base) {
                parts.push(triple);
            } else if let Some(triple) = eq_to_triple_pattern(right, left, base) {
                parts.push(triple);
            } else if let Some(filter) = comparison_to_filter(CmpOp::Eq, left, right, base, parts) {
                parts.push(filter);
            }
        }
        Expr::Comparison(op, left, right) => {
            if let Some(filter) = comparison_to_filter(op.clone(), left, right, base, parts) {
                parts.push(filter);
            }
        }
        Expr::And(a, b) => {
            push_predicate_parts(a, base, parts);
            push_predicate_parts(b, base, parts);
        }
        // Other predicates: skip conservatively (the LQA SELECT path handles them).
        _ => {}
    }
}

/// Try to convert `lhs.prop = literal_rhs` to a SPARQL triple pattern string.
fn eq_to_triple_pattern(lhs: &Expr, rhs: &Expr, base: &str) -> Option<String> {
    if let (Expr::Property(node_expr, key), lit_expr) = (lhs, rhs) {
        if let Expr::Variable { name: var_name, .. } = node_expr.as_ref() {
            if let Some(lit_str) = expr_to_sparql_lit(lit_expr, base) {
                return Some(format!("?{var_name} <{base}{key}> {lit_str}"));
            }
        }
    }
    None
}

/// Try to convert a comparison predicate into a SPARQL FILTER string.
/// Also pushes any needed property triple patterns into `parts`.
fn comparison_to_filter(
    op: CmpOp,
    lhs: &Expr,
    rhs: &Expr,
    base: &str,
    parts: &mut Vec<String>,
) -> Option<String> {
    let l_str = expr_to_filter_expr(lhs, base, parts)?;
    let r_str = expr_to_filter_expr(rhs, base, parts)?;
    let op_str = match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        _ => return None,
    };
    Some(format!("FILTER({l_str} {op_str} {r_str})"))
}

/// Convert an expression to a SPARQL filter-expression string, pushing any
/// required property-access triple patterns into `parts`.
fn expr_to_filter_expr(expr: &Expr, base: &str, parts: &mut Vec<String>) -> Option<String> {
    match expr {
        Expr::Literal(lit) => lit_to_sparql(lit),
        Expr::Variable { name, .. } => Some(format!("?{name}")),
        Expr::Property(node_expr, key) => {
            if let Expr::Variable { name: var_name, .. } = node_expr.as_ref() {
                let prop_var = format!("?__{var_name}_{key}_flt");
                parts.push(format!(
                    "OPTIONAL {{ ?{var_name} <{base}{key}> {prop_var} }}"
                ));
                Some(prop_var)
            } else {
                None
            }
        }
        Expr::Unary(UnaryOp::Neg, inner) => {
            let s = expr_to_filter_expr(inner, base, parts)?;
            Some(format!("(-{s})"))
        }
        _ => None,
    }
}

// ── Literal helpers ───────────────────────────────────────────────────────────

/// Convert a [`Literal`] to its SPARQL serialisation.
///
/// Returns `None` for `Literal::Null` (which has no SPARQL representation).
fn lit_to_sparql(lit: &Literal) -> Option<String> {
    match lit {
        Literal::Integer(n) => Some(format!("\"{n}\"^^<{XSD_INTEGER}>")),
        Literal::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                None
            } else {
                Some(format!("\"{f}\"^^<{XSD_DOUBLE}>"))
            }
        }
        Literal::String(s) => {
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t");
            Some(format!("\"{escaped}\""))
        }
        Literal::Boolean(b) => Some(format!(
            "\"{}\"^^<{XSD_BOOLEAN}>",
            if *b { "true" } else { "false" }
        )),
        Literal::Null => None,
        Literal::TypedLiteral(value, xsd_type) => {
            let escaped = value.replace('"', "\\\"");
            Some(format!("\"{escaped}\"^^<{xsd_type}>"))
        }
    }
}

/// Attempt to evaluate a constant LQA [`Expr`] to its SPARQL literal string.
///
/// Only handles compile-time-known literals and simple arithmetic.  Returns
/// `None` for any expression that requires runtime data.
fn expr_to_sparql_lit(expr: &Expr, _base: &str) -> Option<String> {
    match expr {
        Expr::Literal(lit) => lit_to_sparql(lit),
        Expr::Unary(UnaryOp::Neg, inner) => match inner.as_ref() {
            Expr::Literal(Literal::Integer(n)) => Some(format!("\"{}\"^^<{XSD_INTEGER}>", -n)),
            Expr::Literal(Literal::Float(f)) => Some(format!("\"{}\"^^<{XSD_DOUBLE}>", -f)),
            _ => None,
        },
        // Constant list / map literals: serialise as a compact string literal.
        Expr::List(_) | Expr::Map(_) => {
            let s = lqa_write_serialize_literal(expr)?;
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{escaped}\""))
        }
        // Concatenation of two constant lists: [1,2] + [3,4] → "[1, 2, 3, 4]"
        Expr::Add(a, b) => {
            let sa = lqa_write_serialize_literal(a)?;
            let sb = lqa_write_serialize_literal(b)?;
            // Only fold if both sides look like lists (start with '[')
            if sa.starts_with('[') && sb.starts_with('[') {
                let inner_a = sa.trim_matches(|c| c == '[' || c == ']').trim();
                let inner_b = sb.trim_matches(|c| c == '[' || c == ']').trim();
                let combined = match (inner_a.is_empty(), inner_b.is_empty()) {
                    (true, true) => "[]".to_owned(),
                    (true, _) => format!("[{inner_b}]"),
                    (_, true) => format!("[{inner_a}]"),
                    _ => format!("[{inner_a}, {inner_b}]"),
                };
                let escaped = combined.replace('\\', "\\\\").replace('"', "\\\"");
                Some(format!("\"{escaped}\""))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Serialise a constant LQA expression to a string representation.
/// Returns `None` if the expression contains runtime-dependent sub-expressions.
fn lqa_write_serialize_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::List(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(lqa_write_serialize_literal)
                .collect::<Option<_>>()?;
            Some(format!("[{}]", parts.join(", ")))
        }
        Expr::Map(pairs) => {
            let entries: Vec<String> = pairs
                .iter()
                .map(|(k, v)| lqa_write_serialize_literal(v).map(|s| format!("{k}: {s}")))
                .collect::<Option<_>>()?;
            Some(format!("{{{}}}", entries.join(", ")))
        }
        Expr::Literal(lit) => match lit {
            Literal::Integer(n) => Some(n.to_string()),
            Literal::Float(f) => Some(format!("{f}")),
            Literal::String(s) => Some(format!("'{s}'")),
            Literal::Boolean(b) => Some(if *b { "true" } else { "false" }.to_owned()),
            Literal::Null => Some("null".to_owned()),
            Literal::TypedLiteral(v, _) => Some(v.clone()),
        },
        Expr::Unary(UnaryOp::Neg, inner) => match inner.as_ref() {
            Expr::Literal(Literal::Integer(n)) => Some(format!("-{n}")),
            Expr::Literal(Literal::Float(f)) => Some(format!("{}", -f)),
            _ => None,
        },
        // List concatenation: [1,2] + [3,4] → "[1, 2, 3, 4]"
        Expr::Add(a, b) => {
            let sa = lqa_write_serialize_literal(a)?;
            let sb = lqa_write_serialize_literal(b)?;
            if sa.starts_with('[') && sb.starts_with('[') {
                let inner_a = sa.trim_matches(|c| c == '[' || c == ']').trim();
                let inner_b = sb.trim_matches(|c| c == '[' || c == ']').trim();
                Some(match (inner_a.is_empty(), inner_b.is_empty()) {
                    (true, true) => "[]".to_owned(),
                    (true, _) => format!("[{inner_b}]"),
                    (_, true) => format!("[{inner_a}]"),
                    _ => format!("[{inner_a}, {inner_b}]"),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Format a SPARQL expression (for use inside BIND or FILTER) from an LQA
/// expression.  Returns `None` when the expression cannot be compiled to a
/// static string.
fn expr_to_sparql_update_expr(expr: &Expr, var: &str, base: &str) -> Option<String> {
    match expr {
        Expr::Literal(lit) => lit_to_sparql(lit),
        Expr::Variable { name, .. } => Some(format!("?{name}")),
        Expr::Property(node_expr, key) => {
            if let Expr::Variable { name: var_name, .. } = node_expr.as_ref() {
                Some(format!("?__{var_name}_{key}_cur"))
            } else {
                None
            }
        }
        Expr::Unary(UnaryOp::Neg, inner) => {
            let s = expr_to_sparql_update_expr(inner, var, base)?;
            Some(format!("(-{s})"))
        }
        // Arithmetic
        Expr::Add(a, b) => binary_op(a, b, "+", var, base),
        Expr::Sub(a, b) => binary_op(a, b, "-", var, base),
        Expr::Mul(a, b) => binary_op(a, b, "*", var, base),
        Expr::Div(a, b) => binary_op(a, b, "/", var, base),
        _ => None,
    }
}

fn binary_op(a: &Expr, b: &Expr, op: &str, var: &str, base: &str) -> Option<String> {
    let a_str = expr_to_sparql_update_expr(a, var, base)?;
    let b_str = expr_to_sparql_update_expr(b, var, base)?;
    Some(format!("({a_str} {op} {b_str})"))
}

// ── CREATE compiler ───────────────────────────────────────────────────────────

fn compile_create(
    nodes: &[CreateNode],
    edges: &[CreateEdge],
    where_parts: &[String],
    base: &str,
    counter: &mut usize,
    bnode_map: &mut HashMap<String, String>,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let no_match = where_parts.is_empty();

    // Build a set of variable names that are already bound by the WHERE clause.
    // These correspond to nodes that exist in the MATCH context; they must be
    // referenced via `?var` in the INSERT rather than as fresh blank nodes.
    let bound_in_where: std::collections::HashSet<&str> = where_parts
        .iter()
        .flat_map(|p| {
            // Extract `?varname` from the pattern string.
            let bytes = p.as_bytes();
            let mut found = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'?' {
                    let start = i + 1;
                    let end = bytes[start..]
                        .iter()
                        .position(|&b| !b.is_ascii_alphanumeric() && b != b'_')
                        .map(|n| start + n)
                        .unwrap_or(bytes.len());
                    if start < end {
                        if let Ok(s) = std::str::from_utf8(&bytes[start..end]) {
                            found.push(s);
                        }
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            found
        })
        .collect();

    // Safety check: if the WHERE clause uses BIND-based variable renames (e.g. from
    // `WITH n AS a`) AND this CREATE would insert new blank nodes that are NOT pre-existing
    // in the match context, the post-INSERT SELECT in RETURN-clause queries would find those
    // new blank nodes through the original MATCH condition, producing wrong row counts.
    // Detect this and fall back to legacy so the correct result is returned.
    let has_bind_renames = where_parts.iter().any(|p| p.starts_with("BIND("));
    if has_bind_renames {
        let creates_new_nodes = nodes.iter().any(|n| {
            n.variable
                .as_deref()
                .map(|v| !bound_in_where.contains(v))
                .unwrap_or(true) // anonymous node (no variable) → always new
        });
        if creates_new_nodes {
            return Err(write_unsupported!("write_where_complex_op"));
        }
    }

    let mut insert_triples: Vec<String> = Vec::new();

    for node in nodes {
        let bnode = if let Some(var) = &node.variable {
            // Reuse a blank node already assigned to this variable (chained creates).
            if let Some(existing) = bnode_map.get(var) {
                existing.clone()
            } else if bound_in_where.contains(var.as_str()) {
                // This variable is already bound by the WHERE clause (it came from a MATCH
                // pattern).  Use the SPARQL variable directly; do NOT create a new blank node
                // or insert a sentinel triple (the node already exists in the graph).
                let var_ref = format!("?{var}");
                bnode_map.insert(var.clone(), var_ref.clone());
                // Skip sentinel/labels/properties for existing nodes.
                continue;
            } else {
                let bn = format!("_:__n{}", *counter);
                *counter += 1;
                bnode_map.insert(var.clone(), bn.clone());
                bn
            }
        } else {
            let bn = format!("_:__n{}", *counter);
            *counter += 1;
            bn
        };

        // Node existence sentinel.
        insert_triples.push(format!("{bnode} <{base}__node> <{base}__node>"));
        // Labels.
        for label in &node.labels {
            insert_triples.push(format!("{bnode} <{RDF_TYPE}> <{base}{label}>"));
        }
        // Properties.
        for (key, val) in &node.properties {
            if let Some(lit_str) = expr_to_sparql_lit(val, base) {
                insert_triples.push(format!("{bnode} <{base}{key}> {lit_str}"));
            }
        }
    }

    for edge in edges {
        let (src_ref, dst_ref) = match edge.direction {
            Direction::Outgoing => (
                var_ref_for_create(&edge.from, bnode_map),
                var_ref_for_create(&edge.to, bnode_map),
            ),
            Direction::Incoming => (
                var_ref_for_create(&edge.to, bnode_map),
                var_ref_for_create(&edge.from, bnode_map),
            ),
            Direction::Undirected => (
                var_ref_for_create(&edge.from, bnode_map),
                var_ref_for_create(&edge.to, bnode_map),
            ),
        };
        let type_iri = format!("{base}{}", edge.rel_type);
        insert_triples.push(format!("{src_ref} <{type_iri}> {dst_ref}"));
        // Edge properties (RDF-star).
        for (key, val) in &edge.properties {
            if let Some(lit_str) = expr_to_sparql_lit(val, base) {
                insert_triples.push(format!(
                    "<< {src_ref} <{type_iri}> {dst_ref} >> <{base}{key}> {lit_str}"
                ));
            }
        }
    }

    if insert_triples.is_empty() {
        // Nothing to insert.
        return Ok(());
    }

    let insert_body = insert_triples.join(" . ");

    if no_match {
        out.push(format!("INSERT DATA {{ {insert_body} }}"));
    } else {
        let where_body = where_parts.join(" . ");
        out.push(format!(
            "INSERT {{ {insert_body} }} WHERE {{ {where_body} }}"
        ));
    }

    Ok(())
}

/// Return the SPARQL reference for a variable in a CREATE context —
/// either a blank node (if freshly created) or a SPARQL variable (if from MATCH).
fn var_ref_for_create(var: &str, bnode_map: &HashMap<String, String>) -> String {
    if let Some(bn) = bnode_map.get(var) {
        bn.clone()
    } else {
        format!("?{var}")
    }
}

// ── SET compiler ──────────────────────────────────────────────────────────────

fn compile_set_items(
    items: &[SetItem],
    where_parts: &[String],
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let base_where = where_parts.join(" . ");

    for item in items {
        match item {
            SetItem::Property {
                variable,
                key,
                value,
            } => {
                let n_var = format!("?{variable}");
                let prop_iri = format!("{base}{key}");
                let old_var = format!("?__{variable}_{key}_old");
                let new_var = format!("?__{variable}_{key}_new");

                if let Some(lit_str) = expr_to_sparql_lit(value, base) {
                    // SET n.prop = literal
                    if base_where.is_empty() {
                        out.push(format!(
                            "DELETE {{ {n_var} <{prop_iri}> {old_var} }} \
                             INSERT {{ {n_var} <{prop_iri}> {lit_str} }} \
                             WHERE {{ {n_var} <{base}__node> <{base}__node> . \
                                      OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }} }}"
                        ));
                    } else {
                        out.push(format!(
                            "DELETE {{ {n_var} <{prop_iri}> {old_var} }} \
                             INSERT {{ {n_var} <{prop_iri}> {lit_str} }} \
                             WHERE {{ {base_where} . \
                                      OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }} }}"
                        ));
                    }
                } else if let Some(expr_str) = expr_to_sparql_update_expr(value, variable, base) {
                    // SET n.prop = <expr>
                    // Need to bind the expression and the old value.
                    let where_with_old = if base_where.is_empty() {
                        format!(
                            "{n_var} <{base}__node> <{base}__node> . \
                                 OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }}"
                        )
                    } else {
                        format!("{base_where} . OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }}")
                    };

                    // Extra property triples needed by the expression (e.g. for n.other).
                    let prop_binding = build_property_bindings(value, variable, base);

                    let bind_clause = if prop_binding.is_empty() {
                        format!("BIND({expr_str} AS {new_var})")
                    } else {
                        format!("{prop_binding} . BIND({expr_str} AS {new_var})")
                    };

                    out.push(format!(
                        "DELETE {{ {n_var} <{prop_iri}> {old_var} }} \
                         INSERT {{ {n_var} <{prop_iri}> {new_var} }} \
                         WHERE {{ {where_with_old} . {bind_clause} . \
                                  FILTER(BOUND({new_var})) }}"
                    ));
                } else {
                    // Expression not supported; fall back.
                    return Err(write_unsupported!("write_set_complex_expr"));
                }
            }

            SetItem::Label { variable, labels } => {
                // SET n:Label — insert the type triple.
                let n_var = format!("?{variable}");
                let base_where_cond = if base_where.is_empty() {
                    format!("{n_var} <{base}__node> <{base}__node>")
                } else {
                    base_where.clone()
                };

                for label in labels {
                    let label_iri = format!("{base}{label}");
                    out.push(format!(
                        "INSERT {{ {n_var} <{RDF_TYPE}> <{label_iri}> }} \
                         WHERE {{ {base_where_cond} }}"
                    ));
                }
            }

            SetItem::MergeMap { .. } | SetItem::Replace { .. } => {
                // SET n += {map} or SET n = {map}: fall back to legacy.
                // Correct implementation requires running SELECT before updates (so that
                // MATCH filters still hold for the RETURN clause after SET removes properties).
                // The legacy translator handles these correctly via skip_writes mode.
                return Err(write_unsupported!("write_set_replace_or_merge_map"));
            }
        }
    }
    Ok(())
}

/// Build extra OPTIONAL property-binding clauses needed to evaluate an
/// expression like `n.other + 1` inside a BIND.
fn build_property_bindings(expr: &Expr, primary_var: &str, base: &str) -> String {
    let mut bindings: Vec<String> = Vec::new();
    collect_property_bindings(expr, primary_var, base, &mut bindings);
    bindings.join(" . ")
}

fn collect_property_bindings(
    expr: &Expr,
    primary_var: &str,
    base: &str,
    bindings: &mut Vec<String>,
) {
    match expr {
        Expr::Property(node_expr, key) => {
            if let Expr::Variable { name: var_name, .. } = node_expr.as_ref() {
                let bound_var = format!("?__{var_name}_{key}_cur");
                let binding = format!("OPTIONAL {{ ?{var_name} <{base}{key}> {bound_var} }}");
                if !bindings.contains(&binding) {
                    bindings.push(binding);
                }
            }
        }
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
            collect_property_bindings(a, primary_var, base, bindings);
            collect_property_bindings(b, primary_var, base, bindings);
        }
        Expr::Unary(_, inner) => {
            collect_property_bindings(inner, primary_var, base, bindings);
        }
        _ => {}
    }
    let _ = primary_var; // suppress unused warning
}

// ── REMOVE compiler ───────────────────────────────────────────────────────────

fn compile_remove_items(
    items: &[RemoveItem],
    where_parts: &[String],
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let base_where = where_parts.join(" . ");

    for item in items {
        match item {
            RemoveItem::Property { variable, key } => {
                let n_var = format!("?{variable}");
                let prop_iri = format!("{base}{key}");
                let del_var = format!("?__{variable}_{key}_del");
                let base_where_cond = if base_where.is_empty() {
                    format!("{n_var} <{base}__node> <{base}__node>")
                } else {
                    base_where.clone()
                };

                out.push(format!(
                    "DELETE {{ {n_var} <{prop_iri}> {del_var} }} \
                     WHERE {{ {base_where_cond} . \
                              OPTIONAL {{ {n_var} <{prop_iri}> {del_var} }} }}"
                ));
            }

            RemoveItem::Label { variable, labels } => {
                let n_var = format!("?{variable}");
                for label in labels {
                    let label_iri = format!("{base}{label}");
                    // The WHERE condition for REMOVE :Label must match the node
                    // having that label (so that nodes without the label are skipped).
                    let base_where_cond = if base_where.is_empty() {
                        format!("{n_var} <{RDF_TYPE}> <{label_iri}>")
                    } else {
                        format!("{base_where} . {n_var} <{RDF_TYPE}> <{label_iri}>")
                    };

                    out.push(format!(
                        "DELETE {{ {n_var} <{RDF_TYPE}> <{label_iri}> }} \
                         WHERE {{ {base_where_cond} }}"
                    ));
                }
            }
        }
    }
    Ok(())
}

// ── DELETE compiler ───────────────────────────────────────────────────────────

fn compile_delete(
    exprs: &[Expr],
    detach: bool,
    where_parts: &[String],
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let base_where = where_parts.join(" . ");

    for expr in exprs {
        match expr {
            Expr::Variable { name: var, .. } => {
                let n_var = format!("?{var}");
                let base_where_cond = if base_where.is_empty() {
                    format!("{n_var} <{base}__node> <{base}__node>")
                } else {
                    base_where.clone()
                };

                if detach {
                    // DETACH DELETE: remove all triples where the node is subject or object.
                    // Also remove the node itself (sentinel triple).
                    let p_var = format!("?__del_{var}_p");
                    let o_var = format!("?__del_{var}_o");
                    let s_var = format!("?__del_{var}_s");

                    // Delete outgoing triples (node as subject).
                    out.push(format!(
                        "DELETE {{ {n_var} {p_var} {o_var} }} \
                         WHERE {{ {base_where_cond} . {n_var} {p_var} {o_var} }}"
                    ));
                    // Delete incoming triples (node as object).
                    out.push(format!(
                        "DELETE {{ {s_var} {p_var} {n_var} }} \
                         WHERE {{ {base_where_cond} . {s_var} {p_var} {n_var} . \
                                  FILTER({s_var} != {n_var}) }}"
                    ));
                } else {
                    // Non-DETACH DELETE: only delete the node triples (subject side).
                    let p_var = format!("?__del_{var}_p");
                    let o_var = format!("?__del_{var}_o");
                    out.push(format!(
                        "DELETE {{ {n_var} {p_var} {o_var} }} \
                         WHERE {{ {base_where_cond} . {n_var} {p_var} {o_var} }}"
                    ));
                }
            }
            // DELETE of a relationship variable — remove the edge triple.
            _ => {
                // Complex DELETE expressions: fall back.
                return Err(write_unsupported!("write_delete_complex_expr"));
            }
        }
    }
    Ok(())
}

// ── MERGE compiler ────────────────────────────────────────────────────────────

fn compile_merge(
    clause: &MergeClause,
    outer_where: &[String],
    base: &str,
    counter: &mut usize,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    // Get the match pattern from clause.pattern (the Op that defines what to match-or-create).
    let pattern_parts = op_to_where_parts(&clause.pattern, base)?;

    match clause.pattern.as_ref() {
        // ── Node MERGE ───────────────────────────────────────────────────
        Op::Selection { inner, .. } => {
            // Selection wrapping a Scan — node with properties.
            compile_merge_node_scan(
                inner,
                &clause.pattern,
                &pattern_parts,
                outer_where,
                &clause.on_match,
                &clause.on_create,
                base,
                counter,
                out,
            )?;
        }
        Op::Scan { .. } => {
            // Bare Scan without properties.
            compile_merge_node_scan(
                &clause.pattern,
                &clause.pattern,
                &pattern_parts,
                outer_where,
                &clause.on_match,
                &clause.on_create,
                base,
                counter,
                out,
            )?;
        }
        Op::Expand {
            inner,
            from,
            rel_var: _,
            to,
            rel_types,
            direction,
            range: None,
            path_var: _,
        } => {
            // Relationship MERGE.
            compile_merge_rel(
                inner,
                from,
                to,
                rel_types,
                direction.clone(),
                &clause.on_match,
                &clause.on_create,
                outer_where,
                base,
                out,
            )?;
        }
        _ => {
            return Err(write_unsupported!("write_merge_complex_pattern"));
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_merge_node_scan(
    scan_op: &Op,
    _full_pattern: &Op,
    pattern_parts: &[String],
    outer_where: &[String],
    on_match: &[SetItem],
    on_create: &[SetItem],
    base: &str,
    counter: &mut usize,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    // Node MERGE with outer MATCH context: LQA's INSERT WHERE creates one new node
    // per outer result row, which is incorrect — Cypher's node MERGE checks for the
    // existence of the pattern globally, independent of the number of outer rows.
    // Relationship MERGE (handled by compile_merge_rel) is per-row and works correctly.
    if !outer_where.is_empty() {
        return Err(write_unsupported!("write_merge_with_outer_match"));
    }

    // Validate that we can parse the scan op's WHERE parts.
    let _scan_parts = op_to_where_parts(scan_op, base)
        .map_err(|_| write_unsupported!("write_merge_node_scan_parts"))?;

    // Determine node variable name from scan.
    let var_name = match scan_op {
        Op::Scan { variable, .. } => variable.clone(),
        _ => {
            // Find interior scan.
            if let Some(v) = find_scan_var(scan_op) {
                v
            } else {
                format!("__merge_n{}", counter)
            }
        }
    };

    // Build the blank node for the INSERT part.
    let bnode = format!("_:__mn{}", *counter);
    *counter += 1;

    // INSERT part: create the node with all labels+props from pattern_parts.
    let mut insert_parts: Vec<String> = Vec::new();
    insert_parts.push(format!("{bnode} <{base}__node> <{base}__node>"));

    // Add labels and properties from the pattern.
    for part in pattern_parts {
        // Convert `?var_name <iri> value` to `bnode <iri> value` for INSERT.
        let insert_part = part.replace(&format!("?{var_name}"), &bnode);
        // Skip OPTIONAL and FILTER parts.
        if !insert_part.contains("OPTIONAL")
            && !insert_part.contains("FILTER")
            && insert_part != format!("{bnode} <{base}__node> <{base}__node>")
        {
            insert_parts.push(insert_part);
        }
    }

    // ON CREATE SET items added to INSERT.
    let mut on_create_parts: Vec<String> = Vec::new();
    for item in on_create {
        match item {
            SetItem::Property { key, value, .. } => {
                if let Some(lit_str) = expr_to_sparql_lit(value, base) {
                    on_create_parts.push(format!("{bnode} <{base}{key}> {lit_str}"));
                }
            }
            SetItem::Label { labels, .. } => {
                for label in labels {
                    on_create_parts.push(format!("{bnode} <{RDF_TYPE}> <{base}{label}>"));
                }
            }
            _ => {}
        }
    }

    let all_insert = [insert_parts.clone(), on_create_parts].concat();
    let insert_body = all_insert.join(" . ");

    // NOT EXISTS condition: check if a matching node already exists.
    let exists_body = pattern_parts.join(" . ");

    // WHERE condition for INSERT: outer MATCH context + NOT EXISTS.
    let where_for_insert = if outer_where.is_empty() {
        format!("FILTER NOT EXISTS {{ {exists_body} }}")
    } else {
        format!(
            "{} . FILTER NOT EXISTS {{ {exists_body} }}",
            outer_where.join(" . ")
        )
    };

    out.push(format!(
        "INSERT {{ {insert_body} }} WHERE {{ {where_for_insert} }}"
    ));

    // ON MATCH SET: apply to matched node.
    if !on_match.is_empty() {
        let match_where = if outer_where.is_empty() {
            pattern_parts.join(" . ")
        } else {
            format!(
                "{} . {}",
                outer_where.join(" . "),
                pattern_parts.join(" . ")
            )
        };
        // Generate SET updates using the match pattern as WHERE.
        let parts_vec: Vec<String> = if outer_where.is_empty() {
            pattern_parts.to_vec()
        } else {
            [outer_where.to_vec(), pattern_parts.to_vec()].concat()
        };
        compile_set_items(on_match, &parts_vec, base, out)?;
        let _ = match_where; // suppress unused warning
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_merge_rel(
    match_context: &Op,
    from: &str,
    to: &str,
    rel_types: &[String],
    direction: Direction,
    on_match: &[SetItem],
    on_create: &[SetItem],
    outer_where: &[String],
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    // Get WHERE parts for nodes.
    let ctx_parts = op_to_where_parts(match_context, base)?;

    let base_where_parts = if outer_where.is_empty() {
        ctx_parts.clone()
    } else {
        [outer_where.to_vec(), ctx_parts.clone()].concat()
    };
    let base_where = base_where_parts.join(" . ");

    // If the base WHERE doesn't constrain the from/to nodes (both variables
    // would be unbound), the INSERT would generate triples with unbound variables
    // which SPARQL silently ignores. Fall back to legacy in this case.
    let from_is_constrained = base_where_parts
        .iter()
        .any(|p| p.contains(&format!("?{from}")));
    let to_is_constrained = base_where_parts
        .iter()
        .any(|p| p.contains(&format!("?{to}")));
    if !from_is_constrained || !to_is_constrained {
        return Err(write_unsupported!("write_merge_rel_unbound_nodes"));
    }

    for rt in rel_types {
        let type_iri = format!("{base}{rt}");
        let (actual_src, actual_dst) = match direction {
            Direction::Incoming => (to, from),
            _ => (from, to),
        };

        let mut insert_parts: Vec<String> =
            vec![format!("?{actual_src} <{type_iri}> ?{actual_dst}")];

        // ON CREATE SET items for the relationship.
        for item in on_create {
            if let SetItem::Property { key, value, .. } = item {
                if let Some(lit_str) = expr_to_sparql_lit(value, base) {
                    insert_parts.push(format!(
                        "<< ?{actual_src} <{type_iri}> ?{actual_dst} >> <{base}{key}> {lit_str}"
                    ));
                }
            }
        }

        let insert_body = insert_parts.join(" . ");

        // NOT EXISTS check (in both directions for undirected).
        let not_exists_str = match direction {
            Direction::Undirected => format!(
                "{{ ?{actual_src} <{type_iri}> ?{actual_dst} }} UNION \
                 {{ ?{actual_dst} <{type_iri}> ?{actual_src} }}"
            ),
            _ => format!("?{actual_src} <{type_iri}> ?{actual_dst}"),
        };

        let where_body = if base_where.is_empty() {
            format!("FILTER NOT EXISTS {{ {not_exists_str} }}")
        } else {
            format!("{base_where} . FILTER NOT EXISTS {{ {not_exists_str} }}")
        };

        out.push(format!(
            "INSERT {{ {insert_body} }} WHERE {{ {where_body} }}"
        ));

        // ON MATCH SET for the relationship.
        if !on_match.is_empty() {
            let rel_match_parts = [
                base_where_parts.clone(),
                vec![format!("?{actual_src} <{type_iri}> ?{actual_dst}")],
            ]
            .concat();
            compile_set_items(on_match, &rel_match_parts, base, out)?;
        }
    }
    Ok(())
}

/// Find the variable name of the first Scan in an Op tree.
fn find_scan_var(op: &Op) -> Option<String> {
    match op {
        Op::Scan { variable, .. } => Some(variable.clone()),
        Op::Selection { inner, .. }
        | Op::Expand { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Distinct { inner, .. }
        | Op::GroupBy { inner, .. } => find_scan_var(inner),
        Op::CartesianProduct { left, .. }
        | Op::Union { left, .. }
        | Op::UnionAll { left, .. }
        | Op::LeftOuterJoin { left, .. } => find_scan_var(left),
        _ => None,
    }
}
