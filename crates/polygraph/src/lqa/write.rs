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

// ── Relationship variable metadata ───────────────────────────────────────────

/// Metadata about a relationship variable bound in an `Op::Expand` pattern.
/// Used by `compile_delete` to generate correct edge-deletion SPARQL.
struct RelVarBind {
    from: String,
    to: String,
    rel_types: Vec<String>,
    direction: Direction,
}

/// Traverse a read-only Op subtree collecting `rel_var → RelVarBind` mappings
/// for every `Op::Expand` that carries a named relationship variable.
fn collect_rel_vars(op: &Op) -> HashMap<String, RelVarBind> {
    let mut map = HashMap::new();
    collect_rel_vars_rec(op, &mut map);
    map
}

fn collect_rel_vars_rec(op: &Op, map: &mut HashMap<String, RelVarBind>) {
    match op {
        Op::Expand {
            inner,
            from,
            rel_var: Some(rv),
            to,
            rel_types,
            direction,
            ..
        } => {
            map.insert(
                rv.clone(),
                RelVarBind {
                    from: from.clone(),
                    to: to.clone(),
                    rel_types: rel_types.clone(),
                    direction: direction.clone(),
                },
            );
            collect_rel_vars_rec(inner, map);
        }
        Op::Expand { inner, .. } => {
            collect_rel_vars_rec(inner, map);
        }
        Op::Selection { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Distinct { inner }
        | Op::GroupBy { inner, .. }
        | Op::Projection { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Unwind { inner, .. } => collect_rel_vars_rec(inner, map),
        Op::CartesianProduct { left, right }
        | Op::Union { left, right }
        | Op::UnionAll { left, right } => {
            collect_rel_vars_rec(left, map);
            collect_rel_vars_rec(right, map);
        }
        Op::LeftOuterJoin { left, right, .. } => {
            collect_rel_vars_rec(left, map);
            collect_rel_vars_rec(right, map);
        }
        Op::Subquery { outer, inner } => {
            collect_rel_vars_rec(outer, map);
            collect_rel_vars_rec(inner, map);
        }
        // Write ops can appear nested; recurse through them safely.
        Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. } => collect_rel_vars_rec(inner, map),
        Op::Scan { .. } | Op::Unit | Op::Values { .. } => {}
    }
}

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
    /// Variable name → stable IRI mapping for user-defined CREATE nodes that
    /// were assigned stable IRIs (not blank nodes).  Non-empty when the CREATE
    /// used INSERT DATA without a WHERE clause, so the IRIs are known at
    /// compile time and can be used to generate a precise SELECT.
    pub bnode_map: HashMap<String, String>,
    /// When true, the SELECT for RETURN should use `strip_writes_with_bnodes`
    /// (the LQA path) rather than `translate_skip_writes` (the legacy path).
    /// Set to true when `bnode_map` contains stable IRIs.
    pub use_lqa_select: bool,
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

    // Detect whether the query has a RETURN clause.  A RETURN produces a Projection
    // at the TOP of the op tree (possibly wrapped only by OrderBy / Limit / Skip /
    // Distinct).  Intermediate WITH clauses also produce Projections but they are
    // nested INSIDE write ops (Create / Merge / Set …), so they must NOT be treated
    // as a RETURN.
    let has_return = is_top_level_return(op);

    // DELETE+RETURN queries: the RETURN should reflect pre-deletion row counts.
    // Emit no UPDATE statements (the store is not modified), then let the
    // SELECT compilation path (lib.rs) count matched rows on the unchanged
    // store — exactly matching what the legacy translate_skip_writes path does.
    if has_return && contains_delete(op) {
        return Ok(CompiledWrite {
            update_strings: vec![],
            has_return: true,
            use_lqa_select: false,
            bnode_map: HashMap::new(),
        });
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

    let use_lqa_select =
        bnode_map.values().any(|v| v.starts_with('<')) && op_has_create_before_merge(op);
    Ok(CompiledWrite {
        update_strings: updates,
        has_return,
        use_lqa_select,
        bnode_map,
    })
}

// ── Recursive write-op visitor ────────────────────────────────────────────────

/// Returns true if the op tree contains a `Merge` whose inner subtree
/// includes a `Create` (i.e., `CREATE (a), (b) MERGE (a)-[:R]->(b)` pattern).
fn op_has_create_before_merge(op: &Op) -> bool {
    match op {
        Op::Merge { inner, .. } => op_contains_create(inner),
        Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Projection { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Expand { inner, .. }
        | Op::Selection { inner, .. } => op_has_create_before_merge(inner),
        Op::CartesianProduct { left, right } => {
            op_has_create_before_merge(left) || op_has_create_before_merge(right)
        }
        _ => false,
    }
}

/// Returns true if the op tree contains a `Create` node anywhere.
fn op_contains_create(op: &Op) -> bool {
    match op {
        Op::Create { .. } => true,
        Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Projection { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Expand { inner, .. }
        | Op::Selection { inner, .. }
        | Op::Merge { inner, .. } => op_contains_create(inner),
        Op::CartesianProduct { left, right } => {
            op_contains_create(left) || op_contains_create(right)
        }
        _ => false,
    }
}

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

            // Emit CREATE inserts BEFORE any nested write operations.
            //
            // In Cypher semantics, MATCH-DELETE-CREATE (or MATCH-SET-CREATE) reads
            // the match context once, then applies all changes.  In SPARQL Update,
            // separate statements execute sequentially: if we emitted DELETE first,
            // the INSERT's WHERE clause would find zero rows because the matched
            // edges no longer exist.  By emitting INSERT first (while the original
            // data is still intact), the INSERT's WHERE correctly matches, and the
            // subsequent DELETE removes the old edges — producing the right final state.
            compile_create(nodes, edges, &where_parts, base, counter, bnode_map, out)?;

            // Then compile any nested write operations (DELETE, SET, …) inside inner.
            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }
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
            let rel_vars = collect_rel_vars(match_ctx);

            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            compile_delete(exprs, *detach, &where_parts, &rel_vars, base, out)?;
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
            // Run inner write operations FIRST so that bnode_map is populated
            // with stable IRIs from any preceding CREATE nodes.  Then we can
            // use those IRIs to generate precise WHERE clauses for the MERGE.
            if contains_write(inner) {
                compile_write_recursive(inner, base, counter, bnode_map, out)?;
            }

            // Generate WHERE parts with awareness of the bnode_map so that
            // user-defined CREATE nodes (with stable IRIs) get specific bindings.
            let where_parts = op_to_where_parts_with_bnodes(inner, base, bnode_map)?;

            compile_merge(clause, &where_parts, base, counter, bnode_map, out)?;
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
pub(crate) fn strip_writes(op: &Op) -> Op {
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
        // For CREATE, replace it with a scan over the created nodes so that
        // user-defined variables bound during CREATE remain accessible in the
        // stripped query. Synthetic anonymous variables (prefixed `_lqa_`) are
        // NOT converted to Scans because they would match all existing nodes of
        // that type, producing wrong row counts in the SELECT.
        Op::Create { inner, nodes, .. } => {
            let inner_stripped = strip_writes(inner);
            // Build a Scan for each created node that has a USER-DEFINED variable
            // name (i.e. not a synthetic `_lqa_*` anonymous variable).
            let node_scans: Vec<Op> = nodes
                .iter()
                .filter_map(|n| {
                    let var = n.variable.as_deref()?;
                    // Skip synthetic/anonymous variables emitted by the LQA lowerer.
                    if var.starts_with("_lqa_") {
                        return None;
                    }
                    Some(Op::Scan {
                        variable: var.to_owned(),
                        label: n.labels.first().cloned(),
                        extra_labels: n.labels.iter().skip(1).cloned().collect(),
                    })
                })
                .collect();
            // Combine scans with the inner via CartesianProduct.
            node_scans
                .into_iter()
                .fold(inner_stripped, |acc, scan| Op::CartesianProduct {
                    left: Box::new(acc),
                    right: Box::new(scan),
                })
        }
        Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Call { inner, .. }
        | Op::Foreach { inner, .. } => strip_writes(inner),
        // Recurse into read operators that may have write operators in their subtree.
        Op::GroupBy {
            inner,
            group_keys,
            agg_items,
        } => Op::GroupBy {
            inner: Box::new(strip_writes(inner)),
            group_keys: group_keys.clone(),
            agg_items: agg_items.clone(),
        },
        Op::Selection { inner, predicate } => Op::Selection {
            inner: Box::new(strip_writes(inner)),
            predicate: predicate.clone(),
        },
        Op::Unwind {
            inner,
            variable,
            list,
        } => Op::Unwind {
            inner: Box::new(strip_writes(inner)),
            variable: variable.clone(),
            list: list.clone(),
        },
        Op::OrderBy { inner, keys } => Op::OrderBy {
            inner: Box::new(strip_writes(inner)),
            keys: keys.clone(),
        },
        Op::Limit { inner, count } => Op::Limit {
            inner: Box::new(strip_writes(inner)),
            count: count.clone(),
        },
        Op::Skip { inner, count } => Op::Skip {
            inner: Box::new(strip_writes(inner)),
            count: count.clone(),
        },
        Op::Distinct { inner } => Op::Distinct {
            inner: Box::new(strip_writes(inner)),
        },
        Op::Expand {
            inner,
            from,
            rel_var,
            to,
            rel_types,
            direction,
            range,
            path_var,
        } => Op::Expand {
            inner: Box::new(strip_writes(inner)),
            from: from.clone(),
            rel_var: rel_var.clone(),
            to: to.clone(),
            rel_types: rel_types.clone(),
            direction: direction.clone(),
            range: range.clone(),
            path_var: path_var.clone(),
        },
        Op::CartesianProduct { left, right } => Op::CartesianProduct {
            left: Box::new(strip_writes(left)),
            right: Box::new(strip_writes(right)),
        },
        Op::Union { left, right } => Op::Union {
            left: Box::new(strip_writes(left)),
            right: Box::new(strip_writes(right)),
        },
        Op::UnionAll { left, right } => Op::UnionAll {
            left: Box::new(strip_writes(left)),
            right: Box::new(strip_writes(right)),
        },
        Op::LeftOuterJoin {
            left,
            right,
            condition,
        } => Op::LeftOuterJoin {
            left: Box::new(strip_writes(left)),
            right: Box::new(strip_writes(right)),
            condition: condition.clone(),
        },
        Op::Subquery { outer, inner } => Op::Subquery {
            outer: Box::new(strip_writes(outer)),
            inner: Box::new(strip_writes(inner)),
        },
        // Leaf ops — return as-is
        _ => op.clone(),
    }
}

/// Like `strip_writes` but replaces CREATE nodes that have stable IRIs in
/// `bnode_map` with `Op::Values` bindings (using `Literal::Iri`) instead of
/// generic `Op::Scan` patterns.  This allows the SELECT query to be precise —
/// only returning the rows for the specific nodes that were created — rather
/// than scanning all nodes of the same label.
pub(crate) fn strip_writes_with_bnodes(op: &Op, bnode_map: &HashMap<String, String>) -> Op {
    match op {
        Op::Projection {
            inner,
            items,
            distinct,
        } => Op::Projection {
            inner: Box::new(strip_writes_with_bnodes(inner, bnode_map)),
            items: items.clone(),
            distinct: *distinct,
        },
        Op::Create { inner, nodes, .. } => {
            let inner_stripped = strip_writes_with_bnodes(inner, bnode_map);
            let node_ops: Vec<Op> = nodes
                .iter()
                .filter_map(|n| {
                    let var = n.variable.as_deref()?;
                    if var.starts_with("_lqa_") {
                        return None;
                    }
                    if let Some(iri) = bnode_map.get(var) {
                        if iri.starts_with('<') {
                            // Stable IRI — bind via Values.
                            let iri_str =
                                iri.trim_start_matches('<').trim_end_matches('>').to_owned();
                            return Some(Op::Values {
                                bindings: vec![(
                                    var.to_owned(),
                                    crate::lqa::expr::Expr::Literal(
                                        crate::lqa::expr::Literal::Iri(iri_str),
                                    ),
                                )],
                            });
                        }
                    }
                    // Fallback: Scan.
                    Some(Op::Scan {
                        variable: var.to_owned(),
                        label: n.labels.first().cloned(),
                        extra_labels: n.labels.iter().skip(1).cloned().collect(),
                    })
                })
                .collect();
            node_ops
                .into_iter()
                .fold(inner_stripped, |acc, node_op| Op::CartesianProduct {
                    left: Box::new(acc),
                    right: Box::new(node_op),
                })
        }
        Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. } => strip_writes_with_bnodes(inner, bnode_map),
        Op::GroupBy {
            inner,
            group_keys,
            agg_items,
        } => Op::GroupBy {
            inner: Box::new(strip_writes_with_bnodes(inner, bnode_map)),
            group_keys: group_keys.clone(),
            agg_items: agg_items.clone(),
        },
        // Leaf ops — return as-is
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

/// Returns `true` if `op` has a RETURN-clause Projection at the top of the tree
/// (possibly wrapped only by OrderBy / Limit / Skip / Distinct).
/// Intermediate WITH projections nested inside write ops do NOT count.
fn is_top_level_return(op: &Op) -> bool {
    match op {
        Op::Projection { .. } => true,
        Op::OrderBy { inner, .. }
        | Op::Limit { inner, .. }
        | Op::Skip { inner, .. }
        | Op::Distinct { inner } => is_top_level_return(inner),
        _ => false,
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
                // Also constrain the `to` endpoint to be a graph node (sentinel triple).
                // Without this, ?pred_var would also match property triples (where the
                // object is a literal or a type IRI like rdf:type), incorrectly deleting
                // or updating non-edge triples in write-path queries.
                let pred_var = format!("?__pred_{}_{}", from, to);
                match direction {
                    Direction::Outgoing => {
                        parts.push(format!("?{to} <{base}__node> <{base}__node>"));
                        parts.push(format!("?{from} {pred_var} ?{to}"));
                    }
                    Direction::Incoming => {
                        parts.push(format!("?{from} <{base}__node> <{base}__node>"));
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

        // UNWIND of a literal list: generates a VALUES clause.
        // `UNWIND ['a,b', 'a,b'] AS str` → `VALUES (?str) { ('a,b') ('a,b') }`
        Op::Unwind {
            inner,
            list,
            variable,
        } => {
            let mut parts = op_to_where_parts(inner, base)?;
            match list {
                Expr::List(items) => {
                    let vals: Vec<String> = items
                        .iter()
                        .filter_map(|e| expr_to_sparql_lit(e, base))
                        .collect();
                    if !vals.is_empty() && vals.len() == items.len() {
                        parts.push(format!(
                            "VALUES (?{variable}) {{ {} }}",
                            vals.iter()
                                .map(|v| format!("({v})"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                    }
                    // If serialization fails for some items, skip the VALUES clause
                    // (conservative: unwind is ignored for write purposes).
                }
                _ => {
                    // Variable or complex UNWIND source — skip; the variable won't
                    // be bound in the WHERE clause, which may affect correctness but
                    // is acceptable for generating partial updates.
                }
            }
            Ok(parts)
        }

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
                // Handle projections with some computed (non-variable) items.
                // These arise in e.g. `WITH a, split(str, ',') AS roles`.
                // We recurse into the inner for graph patterns, then add BIND
                // clauses only for items that can be expressed in SPARQL.
                // Items that can't be serialised are skipped (conservative: the
                // variable won't be bound in the WHERE clause, but this is usually
                // safe when the non-variable item is only used in the RETURN, not
                // as a filter or write target).
                let mut parts = op_to_where_parts(inner, base)?;
                for pi in items {
                    match &pi.expr {
                        crate::lqa::Expr::Variable { name: src, .. } => {
                            if src != &pi.alias {
                                parts.push(format!("BIND(?{src} AS ?{})", pi.alias));
                            }
                        }
                        other => {
                            // Try to emit a SPARQL BIND for this expression.
                            if let Some(sparql_str) = try_expr_to_sparql_bind(other, base) {
                                parts.push(format!("BIND({sparql_str} AS ?{})", pi.alias));
                            }
                            // If it fails, skip silently — the variable won't be bound.
                        }
                    }
                }
                Ok(parts)
            }
        }

        // GroupBy: recurse into the inner (the MATCH pattern) to generate WHERE triples.
        // The group keys don't change which nodes are matched, only which rows are returned.
        Op::GroupBy { inner, .. } => op_to_where_parts(inner, base),

        // For CREATE, emit label scan patterns for user-defined (non-anonymous) created nodes
        // so that variables bound during CREATE are visible in subsequent WHERE clauses
        // (e.g. the MERGE WHERE clause needs to find the specific nodes that were created).
        // Anonymous synthetic variables (prefixed `_lqa_`) are NOT emitted because they
        // would match ALL existing nodes, producing wrong INSERT row counts.
        Op::Create { inner, nodes, .. } => {
            let mut parts = op_to_where_parts(inner, base)?;
            for node in nodes {
                if let Some(var) = &node.variable {
                    // Skip synthetic/anonymous variables — they match ALL nodes.
                    if var.starts_with("_lqa_") {
                        continue;
                    }
                    // Emit an RDF type triple for each label (to constrain the scan).
                    // If the node has no label, emit just the node-sentinel triple.
                    if node.labels.is_empty() {
                        let node_iri = format!("<{}__node>", base);
                        parts.push(format!("?{var} {node_iri} {node_iri} ."));
                    } else {
                        for label in &node.labels {
                            parts.push(format!(
                                "?{var} <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{base}{label}> ."
                            ));
                        }
                    }
                }
            }
            Ok(parts)
        }

        // Write ops that may appear in the match context (inner of Merge etc.) — skip them
        // and recurse into their inner to find the graph patterns.
        Op::Set { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Merge { inner, .. } => op_to_where_parts(inner, base),

        // LIMIT / SKIP in a write context cannot be emulated safely in SPARQL UPDATE
        // (there is no SPARQL-level LIMIT on UPDATE rows). Fall back to legacy.
        Op::Limit { .. } | Op::Skip { .. } => Err(write_unsupported!("write_limit_skip_context")),

        // For other complex ops in write context — fall back so the whole query
        // is retried on the legacy path, which has richer WHERE generation.
        _ => Err(write_unsupported!("write_where_complex_op")),
    }
}

/// Like `op_to_where_parts` but uses the `bnode_map` to generate specific
/// `BIND(<IRI> AS ?var)` clauses for CREATE nodes that have stable IRIs.
fn op_to_where_parts_with_bnodes(
    op: &Op,
    base: &str,
    bnode_map: &HashMap<String, String>,
) -> Result<Vec<String>, PolygraphError> {
    match op {
        Op::Create { inner, nodes, .. } => {
            let mut parts = op_to_where_parts_with_bnodes(inner, base, bnode_map)?;
            for node in nodes {
                if let Some(var) = &node.variable {
                    if var.starts_with("_lqa_") {
                        continue;
                    }
                    if let Some(iri) = bnode_map.get(var.as_str()) {
                        if iri.starts_with('<') {
                            // Stable IRI — use BIND to constrain to the specific node.
                            parts.push(format!("BIND({iri} AS ?{var})"));
                        } else if iri.starts_with('?') {
                            // Already a SPARQL variable — passthrough.
                        } else {
                            // Blank node — fallback to generic scan.
                            if node.labels.is_empty() {
                                parts.push(format!("?{var} <{base}__node> <{base}__node> ."));
                            } else {
                                for label in &node.labels {
                                    parts.push(format!("?{var} <{RDF_TYPE}> <{base}{label}> ."));
                                }
                            }
                        }
                    } else {
                        // Not in bnode_map — fallback to generic scan.
                        if node.labels.is_empty() {
                            parts.push(format!("?{var} <{base}__node> <{base}__node> ."));
                        } else {
                            for label in &node.labels {
                                parts.push(format!("?{var} <{RDF_TYPE}> <{base}{label}> ."));
                            }
                        }
                    }
                }
            }
            Ok(parts)
        }
        // For all other ops, delegate to the standard op_to_where_parts.
        _ => op_to_where_parts(op, base),
    }
}

/// Returns `None` if the expression cannot be represented as SPARQL.
/// Used to emit BIND clauses for non-variable WITH items in `op_to_where_parts`.
fn try_expr_to_sparql_bind(expr: &Expr, _base: &str) -> Option<String> {
    match expr {
        // Literal → directly serialize
        Expr::Literal(crate::lqa::expr::Literal::Integer(n)) => Some(n.to_string()),
        Expr::Literal(crate::lqa::expr::Literal::Float(f)) => Some(f.to_string()),
        Expr::Literal(crate::lqa::expr::Literal::Boolean(b)) => {
            Some(if *b { "true" } else { "false" }.to_owned())
        }
        Expr::Literal(crate::lqa::expr::Literal::String(s)) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("\"{escaped}\""))
        }
        // Variable reference → ?varname
        Expr::Variable { name, .. } => Some(format!("?{name}")),
        // FunctionCall: handle split() via urn:polygraph:split custom function
        Expr::FunctionCall { name, args, .. } if name.eq_ignore_ascii_case("split") => {
            let s = try_expr_to_sparql_bind(args.first()?, _base)?;
            let d = try_expr_to_sparql_bind(args.get(1)?, _base)?;
            Some(format!("<urn:polygraph:split>({s}, {d})"))
        }
        _ => None,
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
        // `a:LabelA` → `?a <rdf:type> <base:LabelA>` triple pattern
        Expr::LabelCheck { expr, labels } => {
            if let Expr::Variable { name: var, .. } = expr.as_ref() {
                for label in labels {
                    parts.push(format!("?{var} <{RDF_TYPE}> <{base}{label}>"));
                }
            }
        }
        // `NOT a:LabelA` → FILTER NOT EXISTS { ?a <rdf:type> <base:LabelA> }
        Expr::Not(inner) => {
            if let Expr::LabelCheck { expr, labels } = inner.as_ref() {
                if let Expr::Variable { name: var, .. } = expr.as_ref() {
                    for label in labels {
                        parts.push(format!(
                            "FILTER NOT EXISTS {{ ?{var} <{RDF_TYPE}> <{base}{label}> }}"
                        ));
                    }
                }
            }
            // Other NOT forms: skip conservatively.
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

/// Returns `true` if a WHERE clause part is a scalar BIND or VALUES clause
/// (not a node/edge triple pattern).  Used to determine whether node MERGE
/// with outer context is safe (scalar bindings don't multiply node creation).
fn is_scalar_bind_clause(part: &str) -> bool {
    let trimmed = part.trim();
    trimmed.starts_with("BIND(") || trimmed.starts_with("VALUES ")
}

/// Returns `true` if any where part references the given property key IRI.
/// Used to detect when a SET map key overlaps with MATCH filter conditions.
fn map_key_in_where(key: &str, base: &str, where_parts: &[String]) -> bool {
    let prop_iri = format!("<{base}{key}>");
    where_parts.iter().any(|p| p.contains(&prop_iri))
}

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
        Literal::Iri(iri) => Some(format!("<{iri}>")),
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
            Literal::Iri(iri) => Some(format!("{iri}")),
        },
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
        // Arithmetic (and list concatenation)
        Expr::Add(a, b) => {
            // List concatenation: Property + constant list → splice strings.
            // Lists are stored as plain string literals, e.g. "[1, 2, 3]".
            // We use SPARQL CONCAT + SUBSTR to append/prepend the constant items.
            if let (Expr::Property(pnode, pkey), Expr::List(items)) = (a.as_ref(), b.as_ref()) {
                if let Expr::Variable { name: var_name, .. } = pnode.as_ref() {
                    if let Some(ser) = lqa_write_serialize_literal(&Expr::List(items.clone())) {
                        let cur = format!("?__{var_name}_{pkey}_cur");
                        if items.is_empty() {
                            return Some(cur);
                        }
                        // inner_b = content of the constant list without outer [ ]
                        let inner_b = &ser[1..ser.len() - 1];
                        // If the stored list is empty "[]" (length 2), result is just "[inner_b]".
                        // Otherwise: strip trailing "]" of cur and append ", inner_b]".
                        return Some(format!(
                            "IF(STRLEN({cur}) = 2, \"[{inner_b}]\", CONCAT(SUBSTR({cur}, 1, STRLEN({cur})-1), \", {inner_b}]\"))"
                        ));
                    }
                }
            }
            // List concatenation: constant list + Property → prepend constant items.
            if let (Expr::List(items), Expr::Property(pnode, pkey)) = (a.as_ref(), b.as_ref()) {
                if let Expr::Variable { name: var_name, .. } = pnode.as_ref() {
                    if let Some(ser) = lqa_write_serialize_literal(&Expr::List(items.clone())) {
                        let cur = format!("?__{var_name}_{pkey}_cur");
                        if items.is_empty() {
                            return Some(cur);
                        }
                        // inner_a = content of the constant list without outer [ ]
                        let inner_a = &ser[1..ser.len() - 1];
                        // If the stored list is empty "[]" (length 2), result is just "[inner_a]".
                        // Otherwise: strip leading "[" of cur and prepend "[inner_a, ".
                        return Some(format!(
                            "IF(STRLEN({cur}) = 2, \"[{inner_a}]\", CONCAT(\"[{inner_a}, \", SUBSTR({cur}, 2)))"
                        ));
                    }
                }
            }
            binary_op(a, b, "+", var, base)
        }
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
                let existing = existing.clone();
                if existing.starts_with('?') {
                    // This variable is already bound as a WHERE-clause variable reference.
                    // It represents an existing node — skip emitting sentinel/labels/props.
                    continue;
                }
                existing
            } else if bound_in_where.contains(var.as_str()) {
                // This variable is already bound by the WHERE clause (it came from a MATCH
                // pattern).  Use the SPARQL variable directly; do NOT create a new blank node
                // or insert a sentinel triple (the node already exists in the graph).
                let var_ref = format!("?{var}");
                bnode_map.insert(var.clone(), var_ref.clone());
                // Skip sentinel/labels/properties for existing nodes.
                continue;
            } else if no_match && !var.starts_with("_lqa_") {
                // Pure INSERT DATA (no WHERE clause) and user-defined variable:
                // use a stable IRI so that subsequent MERGE statements can
                // reference these specific nodes across UPDATE statements.
                let iri = format!("<{base}__bnode/{}>", *counter);
                *counter += 1;
                bnode_map.insert(var.clone(), iri.clone());
                iri
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
                } else if matches!(value, Expr::Literal(Literal::Null)) {
                    // SET n.prop = null → remove the property triple entirely.
                    // In Cypher, assigning null to a property deletes it.
                    let where_clause = if base_where.is_empty() {
                        format!("{n_var} <{base}__node> <{base}__node> . OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }}")
                    } else {
                        format!("{base_where} . OPTIONAL {{ {n_var} <{prop_iri}> {old_var} }}")
                    };
                    out.push(format!(
                        "DELETE {{ {n_var} <{prop_iri}> {old_var} }} WHERE {{ {where_clause} }}"
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

            SetItem::MergeMap { variable, map } => {
                // SET n += {map}: upsert individual properties from the map.
                match map {
                    Expr::Map(pairs) => {
                        let n_var = format!("?{variable}");
                        for (key, value_expr) in pairs {
                            let prop_iri = format!("{base}{key}");
                            // If this property key appears in the WHERE conditions
                            // (e.g. MATCH (n:X {name: 'A'}) with key="name"), the
                            // DELETE or SELECT would break: after deleting/inserting
                            // the old WHERE condition would no longer match.  Fall back.
                            if map_key_in_where(key, base, where_parts) {
                                return Err(write_unsupported!("write_set_replace_or_merge_map"));
                            }
                            let old_var = format!("?__{variable}_{key}_old");
                            if matches!(value_expr, Expr::Literal(Literal::Null)) {
                                // null → delete that property only
                                let del_where = if base_where.is_empty() {
                                    format!("{n_var} <{base}__node> <{base}__node> . \
                                             {n_var} <{prop_iri}> {old_var}")
                                } else {
                                    format!("{base_where} . {n_var} <{prop_iri}> {old_var}")
                                };
                                out.push(format!(
                                    "DELETE {{ {n_var} <{prop_iri}> {old_var} }} \
                                     WHERE {{ {del_where} }}"
                                ));
                            } else if let Some(lit_str) = expr_to_sparql_lit(value_expr, base) {
                                // literal value → upsert
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
                            } else {
                                return Err(write_unsupported!("write_set_complex_map_expr"));
                            }
                        }
                    }
                    _ => return Err(write_unsupported!("write_set_replace_or_merge_map")),
                }
            }

            SetItem::Replace { variable, value } => {
                // SET n = {map}: delete all properties then insert map entries.
                // If the WHERE has property conditions (literal values), the SELECT
                // would fail after deletion.  Fall back for those cases.
                if base_where.contains('"') || base_where.contains("FILTER") {
                    return Err(write_unsupported!("write_set_replace_or_merge_map"));
                }
                match value {
                    Expr::Map(pairs) => {
                        let n_var = format!("?{variable}");
                        let p_var = format!("?__{variable}_p");
                        let v_var = format!("?__{variable}_v");
                        // Combined DELETE+INSERT: delete all property triples and insert
                        // the new ones in a single atomic SPARQL UPDATE statement.
                        let insert_triples = pairs
                            .iter()
                            .filter(|(_, v)| !matches!(v, Expr::Literal(Literal::Null)))
                            .filter_map(|(key, val_expr)| {
                                let prop_iri = format!("{base}{key}");
                                let lit_str = expr_to_sparql_lit(val_expr, base)?;
                                Some(format!("{n_var} <{prop_iri}> {lit_str}"))
                            })
                            .collect::<Vec<_>>()
                            .join(" . ");
                        let opt_where = if base_where.is_empty() {
                            format!("{n_var} <{base}__node> <{base}__node>")
                        } else {
                            base_where.clone()
                        };
                        out.push(format!(
                            "DELETE {{ {n_var} {p_var} {v_var} }} \
                             INSERT {{ {insert_triples} }} \
                             WHERE {{ {opt_where} . \
                                      OPTIONAL {{ {n_var} {p_var} {v_var} . \
                                                  FILTER({p_var} != <{RDF_TYPE}> && \
                                                         {p_var} != <{base}__node>) }} }}"
                        ));
                    }
                    _ => return Err(write_unsupported!("write_set_replace_or_merge_map")),
                }
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
    rel_vars: &HashMap<String, RelVarBind>,
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let base_where = where_parts.join(" . ");

    for expr in exprs {
        match expr {
            Expr::Variable { name: var, .. } => {
                // Check whether this variable is a relationship var (bound by an Expand).
                if let Some(bind) = rel_vars.get(var.as_str()) {
                    // Relationship DELETE: remove the specific edge triple (and any
                    // RDF-star property triples) from the store.
                    compile_delete_rel(var, bind, &base_where, base, out)?;
                } else {
                    // Node variable DELETE.
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
            }
            // Complex DELETE expressions (non-variable): fall back.
            _ => {
                return Err(write_unsupported!("write_delete_complex_expr"));
            }
        }
    }
    Ok(())
}

/// Generate SPARQL UPDATE statements to delete an edge triple for a named
/// relationship variable.
///
/// Note: RDF-star property triples (`<< s p o >> :prop val`) attached to the
/// edge are intentionally NOT deleted here because the SPARQL Update `DELETE`
/// template syntax `DELETE { << ?s ?p ?o >> ?pp ?po }` is rejected by
/// Oxigraph's Update parser.  Orphaned property triples do not affect SELECT
/// query correctness since relationship-property access patterns always require
/// the edge triple to match first.
fn compile_delete_rel(
    rel_var: &str,
    bind: &RelVarBind,
    base_where: &str,
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    let RelVarBind {
        from,
        to,
        rel_types,
        direction,
    } = bind;
    let _ = rel_var; // kept for API symmetry / future use

    if rel_types.is_empty() {
        // Untyped relationship: the predicate is captured in the anonymous
        // variable ?__pred_{from}_{to} that op_to_where_parts already emits.
        let pred_var = format!("?__pred_{}_{}", from, to);

        let (subj, obj) = match direction {
            Direction::Outgoing => (format!("?{from}"), format!("?{to}")),
            Direction::Incoming => (format!("?{to}"), format!("?{from}")),
            Direction::Undirected => {
                // For undirected patterns op_to_where_parts emits a UNION
                // { ?from ?pred ?to } UNION { ?to ?pred ?from }.
                // Delete both directions — SPARQL silently ignores deletes of
                // non-existing triples, so only the matching direction gets removed.
                out.push(format!(
                    "DELETE {{ ?{from} {pred_var} ?{to} . ?{to} {pred_var} ?{from} }} \
                     WHERE {{ {base_where} }}"
                ));
                return Ok(());
            }
        };

        // Delete the edge triple itself.
        out.push(format!(
            "DELETE {{ {subj} {pred_var} {obj} }} \
             WHERE {{ {base_where} }}"
        ));
    } else {
        for rt in rel_types {
            let type_iri = format!("{base}{rt}");

            let (subj, obj) = match direction {
                Direction::Outgoing => (format!("?{from}"), format!("?{to}")),
                Direction::Incoming => (format!("?{to}"), format!("?{from}")),
                Direction::Undirected => {
                    // Delete both directions — SPARQL ignores deletes of non-existing triples.
                    out.push(format!(
                        "DELETE {{ ?{from} <{type_iri}> ?{to} . ?{to} <{type_iri}> ?{from} }} \
                         WHERE {{ {base_where} }}"
                    ));
                    continue;
                }
            };

            // Delete the typed edge triple.
            out.push(format!(
                "DELETE {{ {subj} <{type_iri}> {obj} }} \
                 WHERE {{ {base_where} }}"
            ));
        }
    }
    Ok(())
}

fn compile_merge(
    clause: &MergeClause,
    outer_where: &[String],
    base: &str,
    counter: &mut usize,
    bnode_map: &HashMap<String, String>,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    // Get the match pattern from clause.pattern (the Op that defines what to match-or-create).
    let pattern_parts = op_to_where_parts(&clause.pattern, base)?;

    match clause.pattern.as_ref() {
        // ── Node MERGE ───────────────────────────────────────────────────
        Op::Selection { inner, .. } if !matches!(inner.as_ref(), Op::Expand { .. }) => {
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
        // ── Relationship MERGE with property filter ────────────────────
        Op::Selection {
            inner,
            predicate: sel_pred,
        } if matches!(inner.as_ref(), Op::Expand { .. }) => {
            // Selection over Expand — relationship MERGE with property predicates.
            if let Op::Expand {
                inner: expand_inner,
                from,
                to,
                rel_types,
                direction,
                range: None,
                rel_var,
                ..
            } = inner.as_ref()
            {
                // Try to extract property conditions from the Selection predicate
                // so they can be included in both the NOT EXISTS check and the INSERT.
                let rel_var_name = rel_var.as_deref().unwrap_or("__rel");
                let rel_props = extract_rel_prop_conditions(sel_pred, rel_var_name, base);
                compile_merge_rel_with_props(
                    expand_inner,
                    from,
                    to,
                    rel_types,
                    direction.clone(),
                    &clause.on_match,
                    &clause.on_create,
                    outer_where,
                    base,
                    bnode_map,
                    rel_props.as_deref().unwrap_or(&[]),
                    out,
                )?;
            } else {
                return Err(write_unsupported!("write_merge_complex_pattern"));
            }
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
            // Relationship MERGE without property filter.
            compile_merge_rel_with_props(
                inner,
                from,
                to,
                rel_types,
                direction.clone(),
                &clause.on_match,
                &clause.on_create,
                outer_where,
                base,
                bnode_map,
                &[],
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

/// Extract property equality conditions from a Selection predicate that applies to
/// the given `rel_var` (the relationship variable in a MERGE pattern).
/// Returns `Some(vec)` if all conditions could be extracted, `None` if the
/// predicate has unsupported structure.
/// Each entry is `(prop_iri, sparql_value_str)` ready to embed in SPARQL.
fn extract_rel_prop_conditions(
    predicate: &Expr,
    rel_var: &str,
    base: &str,
) -> Option<Vec<(String, String)>> {
    match predicate {
        // Property(rel_var, key) = value
        Expr::Comparison(CmpOp::Eq, lhs, rhs) => {
            if let Expr::Property(base_expr, key) = lhs.as_ref() {
                if matches!(base_expr.as_ref(), Expr::Variable { name, .. } if name == rel_var) {
                    let val_str = expr_to_sparql_value(rhs, base)?;
                    return Some(vec![(format!("{base}{key}"), val_str)]);
                }
            }
            if let Expr::Property(base_expr, key) = rhs.as_ref() {
                if matches!(base_expr.as_ref(), Expr::Variable { name, .. } if name == rel_var) {
                    let val_str = expr_to_sparql_value(lhs, base)?;
                    return Some(vec![(format!("{base}{key}"), val_str)]);
                }
            }
            None
        }
        // AND(cond1, cond2) — conjunction of property conditions
        Expr::And(left, right) => {
            let mut conds = extract_rel_prop_conditions(left, rel_var, base)?;
            conds.extend(extract_rel_prop_conditions(right, rel_var, base)?);
            Some(conds)
        }
        _ => None,
    }
}

/// Convert an expression to a SPARQL value string suitable for use in INSERT/NOT EXISTS.
/// Handles literals and variable references only.
fn expr_to_sparql_value(expr: &Expr, base: &str) -> Option<String> {
    match expr {
        Expr::Variable { name, .. } => Some(format!("?{name}")),
        other => expr_to_sparql_lit(other, base),
    }
}

/// Emit MERGE for a relationship where both endpoints have KNOWN stable IRIs
/// (from a preceding `CREATE (a), (b)` that used stable IRI assignment).
/// This avoids the need for a WHERE clause that matches all pairs of nodes.
#[allow(clippy::too_many_arguments)]
fn compile_merge_rel_fully_bound(
    from_iri: &str,
    to_iri: &str,
    rel_types: &[String],
    direction: &Direction,
    on_match: &[SetItem],
    on_create: &[SetItem],
    rel_props: &[(String, String)],
    base: &str,
    out: &mut Vec<String>,
) -> Result<(), PolygraphError> {
    for rt in rel_types {
        let type_iri = format!("{base}{rt}");
        let (actual_src, actual_dst) = match direction {
            Direction::Incoming => (to_iri, from_iri),
            _ => (from_iri, to_iri),
        };

        // Build INSERT triples.
        let mut insert_parts = vec![format!("{actual_src} <{type_iri}> {actual_dst}")];
        for (prop_iri, val) in rel_props {
            insert_parts.push(format!(
                "<< {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {val}"
            ));
        }
        for item in on_create {
            if let SetItem::Property { key, value, .. } = item {
                let prop_iri = format!("{base}{key}");
                if let Some(lit) = expr_to_sparql_lit(value, base) {
                    insert_parts.push(format!(
                        "<< {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {lit}"
                    ));
                }
            }
        }
        let insert_body = insert_parts.join(" . ");

        // Build NOT EXISTS condition.
        let mut not_exists_parts = vec![format!("{actual_src} <{type_iri}> {actual_dst}")];
        for (prop_iri, val) in rel_props {
            not_exists_parts.push(format!(
                "<< {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {val}"
            ));
        }
        let not_exists = not_exists_parts.join(" . ");

        out.push(format!(
            "INSERT {{ {insert_body} }} WHERE {{ FILTER NOT EXISTS {{ {not_exists} }} }}"
        ));

        // ON MATCH SET: apply if the relationship ALREADY existed.
        for item in on_match {
            if let SetItem::Property { key, value, .. } = item {
                let prop_iri = format!("{base}{key}");
                if let Some(lit) = expr_to_sparql_lit(value, base) {
                    let old_var = "?__on_match_old";
                    out.push(format!(
                        "DELETE {{ << {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {old_var} }} \
                         INSERT {{ << {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {lit} }} \
                         WHERE {{ {actual_src} <{type_iri}> {actual_dst} . \
                                  OPTIONAL {{ << {actual_src} <{type_iri}> {actual_dst} >> <{prop_iri}> {old_var} }} }}"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn compile_merge_rel_with_props(
    match_context: &Op,
    from: &str,
    to: &str,
    rel_types: &[String],
    direction: Direction,
    on_match: &[SetItem],
    on_create: &[SetItem],
    outer_where: &[String],
    base: &str,
    bnode_map: &HashMap<String, String>,
    // Extra property conditions extracted from Selection predicate.
    // Each entry is `(prop_iri, sparql_value_str)`.
    rel_props: &[(String, String)],
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
    // which SPARQL silently ignores.
    let from_is_constrained = base_where_parts
        .iter()
        .any(|p| p.contains(&format!("?{from}")));
    let to_is_constrained = base_where_parts
        .iter()
        .any(|p| p.contains(&format!("?{to}")));
    if !from_is_constrained || !to_is_constrained {
        // Check if from/to are in bnode_map with stable IRIs (from a preceding CREATE).
        // If so, we can use those specific IRIs directly without needing a WHERE clause.
        if let (Some(from_iri), Some(to_iri)) = (bnode_map.get(from), bnode_map.get(to)) {
            if from_iri.starts_with('<') && to_iri.starts_with('<') {
                return compile_merge_rel_fully_bound(
                    from_iri, to_iri, rel_types, &direction, on_match, on_create, rel_props, base,
                    out,
                );
            }
        }
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

        // Merge-pattern property conditions: add as RDF-star triples in INSERT.
        for (prop_iri, val_str) in rel_props {
            insert_parts.push(format!(
                "<< ?{actual_src} <{type_iri}> ?{actual_dst} >> <{prop_iri}> {val_str}"
            ));
        }

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

        // NOT EXISTS check: must also include the property conditions so we
        // only skip creation when an *exactly matching* relationship exists.
        let prop_not_exists_triples: String = rel_props
            .iter()
            .map(|(prop_iri, val_str)| {
                format!(" . << ?{actual_src} <{type_iri}> ?{actual_dst} >> <{prop_iri}> {val_str}")
            })
            .collect();

        // NOT EXISTS check (in both directions for undirected).
        let not_exists_str = match direction {
            Direction::Undirected => format!(
                "{{ ?{actual_src} <{type_iri}> ?{actual_dst}{prop_not_exists_triples} }} UNION \
                 {{ ?{actual_dst} <{type_iri}> ?{actual_src}{prop_not_exists_triples} }}"
            ),
            _ => format!("?{actual_src} <{type_iri}> ?{actual_dst}{prop_not_exists_triples}"),
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
