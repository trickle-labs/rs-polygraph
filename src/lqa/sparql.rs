//! Phase 4.5 — LQA → SPARQL lowering pass.
//!
//! Compiles an [`Op`] tree produced by [`crate::lqa::lower`] into a complete
//! [`spargebra::Query`] that can be serialised and executed.
//!
//! # Design
//!
//! The central challenge is **property access**: `Expr::Property(n, "age")`
//! cannot be directly expressed as a SPARQL expression — it must be materialised
//! as a fresh SPARQL variable `?_n_age_0` with an accompanying BGP triple
//! `?n <base:age> ?_n_age_0` injected into the surrounding graph pattern.
//!
//! This module threads a [`Ctx`] carrying `pending_triples` through all
//! expression-lowering calls.  After lowering an expression, the caller is
//! responsible for flushing `pending_triples` into the current graph pattern
//! (see `flush_pending`).
//!
//! # Fallback
//!
//! Complex constructs (variable-length paths, temporal arithmetic, list
//! comprehensions, write operators) return [`PolygraphError::Unsupported`].
//! The calling code in [`crate::lib`] catches this and falls back to the
//! legacy [`crate::translator::cypher`] path, so the TCK floor is maintained.

use std::collections::{HashMap, HashSet};

use spargebra::algebra::{
    AggregateExpression, AggregateFunction, Expression as SparExpr, Function, GraphPattern,
    OrderExpression,
};
use spargebra::term::{
    Literal as SparLit, NamedNode, NamedNodePattern, TermPattern, TriplePattern, Variable,
};
use spargebra::Query;

use crate::error::PolygraphError;
use crate::lqa::expr::{AggKind, CmpOp, Expr, Literal, QuantKind, SortDir, UnaryOp};
use crate::lqa::op::{AggItem, Direction, Op, ProjItem, SortKey};
use crate::result_mapping::schema::{ColumnKind, ProjectedColumn, ProjectionSchema};

// Helper to build a scalar projected column with a single SPARQL variable.
fn scalar_col(name: impl Into<String>) -> ProjectedColumn {
    let n = name.into();
    ProjectedColumn {
        name: n.clone(),
        kind: ColumnKind::Scalar { var: n },
    }
}

// Helper to build a projected column where the Cypher output name may differ from
// the SPARQL variable name. When `display` == `sparql_var`, this is equivalent to
// `scalar_col`. Use this for aggregate/computed projections that have a natural
// Cypher alias like `max(x)` but a safe SPARQL variable name like `_gen_0`.
fn named_col(sparql_var: impl Into<String>, cypher_name: impl Into<String>) -> ProjectedColumn {
    ProjectedColumn {
        name: cypher_name.into(),
        kind: ColumnKind::Scalar {
            var: sparql_var.into(),
        },
    }
}

// Build a projected column from a ProjItem, using display_name (if any) as the
// Cypher output column name while using alias as the SPARQL variable name.
fn proj_item_col(pi: &crate::lqa::op::ProjItem) -> ProjectedColumn {
    match &pi.display_name {
        Some(display) => named_col(&pi.alias, display.as_str()),
        None => scalar_col(&pi.alias),
    }
}

// ── RDF / XSD constants ───────────────────────────────────────────────────────

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD_INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
const XSD_DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
#[allow(dead_code)]
const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
const XSD_TIME: &str = "http://www.w3.org/2001/XMLSchema#time";
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
const DEFAULT_BASE: &str = "http://polygraph.example/";

// ── Public result type ────────────────────────────────────────────────────────

pub struct CompiledQuery {
    pub sparql: String,
    pub schema: ProjectionSchema,
}

// ── Compiler state ────────────────────────────────────────────────────────────

/// Predicate info for a named relationship variable.
#[derive(Debug, Clone)]
enum EdgePred {
    /// Single typed relationship: the IRI is statically known.
    Static(NamedNode),
    /// Multi-type or untyped: a SPARQL variable captures the predicate at runtime.
    Dynamic(Variable),
}

/// Tracking info needed to lower property access and `type()` on a named rel-var.
#[derive(Debug, Clone)]
struct EdgeVarInfo {
    /// SPARQL variable name for the canonical RDF triple's subject.
    subj: String,
    /// SPARQL variable name for the canonical RDF triple's object.
    obj: String,
    /// How to refer to the edge predicate in SPARQL.
    pred: EdgePred,
    /// Whether the pattern was undirected (both-direction UNION).
    /// For undirected edges the stored triple might be in either direction,
    /// so reification lookups must check both <<(subj pred obj)>> and <<(obj pred subj)>>.
    undirected: bool,
}

struct Compiler {
    base_iri: String,
    counter: u32,
    /// Property-access triple patterns accumulated while lowering an expression.
    pending_triples: Vec<TriplePattern>,
    /// Property-access triple patterns that must be emitted as OPTIONAL { } blocks
    /// (e.g. arguments to coalesce() where the property may be absent).
    pending_optional_triples: Vec<TriplePattern>,
    /// Variables that may be null (produced by OPTIONAL MATCH).
    nullable: HashSet<String>,
    /// For each edge variable, the set of rel-type IRIs (used in error diagnostics).
    #[allow(dead_code)]
    edge_types: HashMap<String, Vec<String>>,
    /// Projected column schema collected from the topmost Projection op.
    projected_columns: Vec<ProjectedColumn>,
    return_distinct: bool,
    /// Variables bound by BIND/Extend (not by Scan/Expand) that hold scalar RDF values
    /// (literals, dates, etc.) rather than node IRIs.  Property access on these variables
    /// cannot be lowered to a triple pattern and must fall back to the legacy translator.
    scalar_vars: HashSet<String>,
    /// Tracking info for named relationship variables — used to lower `r.prop` and `type(r)`.
    edge_vars: HashMap<String, EdgeVarInfo>,
    /// Groups of optional triples that must be kept together in one OPTIONAL { } block.
    /// Edge property access (RDF-star reification) emits two triples that share a reifier
    /// variable and must not be split across separate OPTIONAL blocks.
    pending_optional_groups: Vec<Vec<TriplePattern>>,
    /// Arbitrary OPTIONAL graph patterns (e.g. UNION-based reification for undirected edges).
    pending_optional_patterns: Vec<GraphPattern>,
    /// Variables introduced by Scan / Expand ops ("node" and "edge" variables),
    /// tracked so that BIND in projections can detect collisions and use fresh names.
    scan_vars: HashSet<String>,
    /// Variables produced by UNWIND of a list that contained at least one null.
    /// For these variables, aggregate GROUP BY patterns add FILTER(BOUND(?var))
    /// to work around Oxigraph's non-spec behaviour where MAX/MIN over VALUES
    /// with UNDEF returns null instead of the non-null values.
    unwind_null_vars: HashSet<String>,
    /// Pending BIND expressions: `(fresh_var, sparql_expr)` pairs that need to be
    /// emitted as `GraphPattern::Extend` nodes before they are referenced.
    /// Used by `IsNull`/`IsNotNull` lowering when the inner expression is complex
    /// (not a simple variable): the expression must be evaluated and bound so that
    /// `BOUND(?probe)` reliably detects null/error propagation.
    pending_binds: Vec<(Variable, SparExpr)>,
    /// Anonymous edge hops in the current MATCH pattern, tracked for relationship-uniqueness
    /// filtering.  Each entry is (from_var_name, predicate_info, to_var_name) for one hop.
    anon_edge_info: Vec<(String, EdgePred, String)>,
    /// Variables in the current scope that are bound to temporal typed literals.
    /// Maps variable name → XSD type URI ("http://www.w3.org/2001/XMLSchema#date" etc.).
    /// Populated when a WITH clause binds a variable to a TypedLiteral.
    /// Used to apply temporal arithmetic (date + duration) correctly.
    temporal_type_vars: HashMap<String, String>,
    /// Variables in the current scope bound to known constant literal values.
    /// Maps variable name → the raw string value (e.g. "2020-01-01", "PT22H").
    /// Used to evaluate property access on scalar temporal/duration variables.
    scalar_lit_vals: HashMap<String, String>,
}

impl Compiler {
    fn new(base_iri: String) -> Self {
        Self {
            base_iri,
            counter: 0,
            pending_triples: Vec::new(),
            pending_optional_triples: Vec::new(),
            nullable: HashSet::new(),
            edge_types: HashMap::new(),
            projected_columns: Vec::new(),
            return_distinct: false,
            scalar_vars: HashSet::new(),
            edge_vars: HashMap::new(),
            pending_optional_groups: Vec::new(),
            pending_optional_patterns: Vec::new(),
            scan_vars: HashSet::new(),
            unwind_null_vars: HashSet::new(),
            pending_binds: Vec::new(),
            anon_edge_info: Vec::new(),
            temporal_type_vars: HashMap::new(),
            scalar_lit_vals: HashMap::new(),
        }
    }

    fn fresh(&mut self, prefix: &str) -> Variable {
        let c = self.counter;
        self.counter += 1;
        Variable::new_unchecked(format!("_{prefix}_{c}"))
    }

    fn var(name: &str) -> Variable {
        Variable::new_unchecked(name)
    }

    // ── IRI helpers ───────────────────────────────────────────────────────────

    fn prop_iri(&self, key: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("{}{}", self.base_iri, key))
    }

    fn label_iri(&self, label: &str) -> NamedNode {
        NamedNode::new_unchecked(format!("{}{}", self.base_iri, label))
    }

    fn lit_integer(n: i64) -> SparExpr {
        SparExpr::Literal(SparLit::new_typed_literal(
            n.to_string(),
            NamedNode::new_unchecked(XSD_INTEGER),
        ))
    }

    fn lit_double(f: f64) -> SparExpr {
        SparExpr::Literal(SparLit::new_typed_literal(
            format!("{f:?}"),
            NamedNode::new_unchecked(XSD_DOUBLE),
        ))
    }

    fn lit_bool(b: bool) -> SparExpr {
        SparExpr::Literal(SparLit::new_typed_literal(
            b.to_string(),
            NamedNode::new_unchecked(XSD_BOOLEAN),
        ))
    }

    fn lit_str(s: &str) -> SparExpr {
        SparExpr::Literal(SparLit::new_simple_literal(s))
    }

    // ── Pending triple flush ──────────────────────────────────────────────────

    /// Take all pending BGP triples and join them into `pat` as a BGP.
    /// Any pending *optional* triples (from coalesce args, etc.) are appended
    /// as `OPTIONAL { }` blocks via LEFT JOIN.
    fn flush_pending(&mut self, pat: GraphPattern) -> GraphPattern {
        let triples = std::mem::take(&mut self.pending_triples);
        let opt_triples = std::mem::take(&mut self.pending_optional_triples);
        let opt_groups = std::mem::take(&mut self.pending_optional_groups);
        let opt_patterns = std::mem::take(&mut self.pending_optional_patterns);
        let binds = std::mem::take(&mut self.pending_binds);
        let mut result = if triples.is_empty() {
            pat
        } else {
            join(pat, GraphPattern::Bgp { patterns: triples })
        };
        for ot in opt_triples {
            result = GraphPattern::LeftJoin {
                left: Box::new(result),
                right: Box::new(GraphPattern::Bgp { patterns: vec![ot] }),
                expression: None,
            };
        }
        // Grouped optional triples (e.g. RDF-star reification pairs) must stay
        // together in one OPTIONAL block so the reifier variable links them.
        for group in opt_groups {
            if !group.is_empty() {
                result = GraphPattern::LeftJoin {
                    left: Box::new(result),
                    right: Box::new(GraphPattern::Bgp { patterns: group }),
                    expression: None,
                };
            }
        }
        // Arbitrary OPTIONAL patterns (e.g. UNION-based reification for undirected edges).
        for opt_pat in opt_patterns {
            result = GraphPattern::LeftJoin {
                left: Box::new(result),
                right: Box::new(opt_pat),
                expression: None,
            };
        }
        // Pending BINDs: emit as Extend nodes after the inner pattern so that
        // IS NULL probe variables are bound before being referenced.
        for (var, expr) in binds {
            result = GraphPattern::Extend {
                inner: Box::new(result),
                variable: var,
                expression: expr,
            };
        }
        result
    }

    // ── Op lowering ───────────────────────────────────────────────────────────

    /// Lower the Op tree and produce a full SELECT query.
    fn compile_inner(&mut self, op: &Op, base_iri: &str) -> Result<CompiledQuery, PolygraphError> {
        let pattern = self.lower_op_as_query(op)?;
        let schema = ProjectionSchema {
            columns: self.projected_columns.clone(),
            distinct: self.return_distinct,
            base_iri: base_iri.to_string(),
            rdf_star: false,
        };
        let query = Query::Select {
            dataset: None,
            pattern,
            base_iri: None,
        };
        Ok(CompiledQuery {
            sparql: query.to_string(),
            schema,
        })
    }

    /// Walk the top of the Op tree, peeling off query-level wrappers.
    fn lower_op_as_query(&mut self, op: &Op) -> Result<GraphPattern, PolygraphError> {
        match op {
            Op::Limit { inner, count } => {
                let length = expr_to_usize(count)?;
                let inner_pat = self.lower_op_as_query(inner)?;
                // If the direct inner is a SKIP-only Slice (from Op::Skip), merge it
                // with this LIMIT into a single Slice rather than creating nested
                // Slices that spargebra cannot always flatten into one OFFSET+LIMIT.
                let (start, unwrapped) = match inner_pat {
                    GraphPattern::Slice {
                        inner: skip_inner,
                        start: skip_start,
                        length: None,
                    } => (skip_start, *skip_inner),
                    other => (0, other),
                };
                Ok(GraphPattern::Slice {
                    inner: Box::new(unwrapped),
                    start,
                    length: Some(length),
                })
            }
            Op::Skip { inner, count } => {
                let start = expr_to_usize(count)?;
                let inner_pat = self.lower_op_as_query(inner)?;
                Ok(GraphPattern::Slice {
                    inner: Box::new(inner_pat),
                    start,
                    length: None,
                })
            }
            Op::OrderBy { inner, keys } => {
                // When ORDER BY wraps a Projection (RETURN clause), flatten the
                // projected body so that sort-key property triples live in the
                // same WHERE scope as the MATCH patterns.  Creating a nested
                // sub-SELECT here would hide ?node variables from sort triples
                // added after the sub-SELECT boundary.
                if let Op::Projection {
                    inner: proj_inner,
                    items,
                    distinct,
                } = inner.as_ref()
                {
                    // 1. Lower sort-key expressions first; capture any property
                    //    triples they generate.
                    //
                    //    If the sort key is a variable alias from the RETURN clause:
                    //    - If it's a GROUP BY key or aggregate output, use the
                    //      variable directly (it's already bound by the Group).
                    //    - If it's a computed expression alias (e.g. n.name + '!'),
                    //      inline the underlying expression so ORDER BY doesn't
                    //      reference a SELECT-clause alias that may be unbound at
                    //      sort time in some SPARQL engines.
                    let agg_alias_set: std::collections::HashSet<&str> =
                        if let Op::GroupBy { agg_items, .. } = proj_inner.as_ref() {
                            agg_items.iter().map(|a| a.alias.as_str()).collect()
                        } else {
                            std::collections::HashSet::new()
                        };
                    // GROUP BY key aliases are also "already bound" after evaluation
                    // of the Group pattern — no need to expand them to property exprs.
                    let group_key_aliases: std::collections::HashSet<&str> =
                        if let Op::GroupBy { group_keys, .. } = proj_inner.as_ref() {
                            group_keys.iter().map(|k| k.as_str()).collect()
                        } else {
                            std::collections::HashSet::new()
                        };
                    let sort_exprs = keys
                        .iter()
                        .map(|sk| {
                            // Expand alias reference to underlying expression when
                            // the alias refers to a computed (non-variable) RETURN
                            // expression and is not a GROUP BY key or aggregate alias.
                            //
                            // Also: if the sort key expression directly matches a
                            // projection item's expression (e.g. `ORDER BY a.name` where
                            // `a.name AS name` is projected), substitute with the alias
                            // variable — the GroupBy scope hides `?a` but `?name` is bound.
                            //
                            // Build alias substitution table: alias → underlying expression
                            // for all non-variable, non-aggregate, non-group-key proj items.
                            let alias_subst_table: Vec<(&str, &Expr)> = items
                                .iter()
                                .filter(|pi| {
                                    !agg_alias_set.contains(pi.alias.as_str())
                                        && !group_key_aliases.contains(pi.alias.as_str())
                                        && !matches!(pi.expr, Expr::Variable { .. })
                                })
                                .map(|pi| (pi.alias.as_str(), &pi.expr))
                                .collect();

                            let effective_owned: Expr;
                            let effective: &Expr = if let Expr::Variable { name, .. } = &sk.expr {
                                let is_agg = agg_alias_set.contains(name.as_str());
                                let is_gk = group_key_aliases.contains(name.as_str());
                                if !is_agg && !is_gk {
                                    items
                                        .iter()
                                        .find(|pi| {
                                            pi.alias == *name
                                                && !matches!(pi.expr, Expr::Variable { .. })
                                        })
                                        .map(|pi| &pi.expr)
                                        .unwrap_or(&sk.expr)
                                } else {
                                    &sk.expr
                                }
                            } else {
                                // For non-variable sort keys (e.g. `a.name`, `a.name + 'C'`),
                                // check if the sort key directly matches a projection expr.
                                // If so, use the projection alias variable.
                                if let Some(alias) = items.iter().find_map(|pi| {
                                    if exprs_equivalent(&sk.expr, &pi.expr) {
                                        Some(pi.alias.as_str())
                                    } else {
                                        None
                                    }
                                }) {
                                    let sort_expr = SparExpr::Variable(Self::var(alias));
                                    return Ok(match sk.dir {
                                        SortDir::Asc => OrderExpression::Asc(sort_expr),
                                        SortDir::Desc => OrderExpression::Desc(sort_expr),
                                    });
                                }
                                // Recursively substitute alias references within compound
                                // expressions (e.g. `ORDER BY n + 2` where `n` is a RETURN
                                // alias for `n.num`). This replaces alias variable refs with
                                // their underlying expressions before SPARQL lowering.
                                effective_owned =
                                    subst_aliases_in_expr(&sk.expr, &alias_subst_table);
                                &effective_owned
                            };
                            let sparql_expr = self.lower_expr(effective)?;
                            Ok(match sk.dir {
                                SortDir::Asc => OrderExpression::Asc(sparql_expr),
                                SortDir::Desc => OrderExpression::Desc(sparql_expr),
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    let sort_req = std::mem::take(&mut self.pending_triples);
                    let sort_opt = std::mem::take(&mut self.pending_optional_triples);

                    // 2. Flatten the Projection body (handles GroupBy etc.).
                    let (proj_gp, _agg_vars) = self.lower_projection_inner(proj_inner, items)?;
                    let project_vars = self.build_project_vars(items)?;

                    // 3. Flush projection's own pending triples.
                    let mut flat = self.flush_pending(proj_gp);

                    // 4. Inject sort-key triples into the same flat scope.
                    if !sort_req.is_empty() {
                        flat = join(flat, GraphPattern::Bgp { patterns: sort_req });
                    }
                    for ot in sort_opt {
                        flat = GraphPattern::LeftJoin {
                            left: Box::new(flat),
                            right: Box::new(GraphPattern::Bgp { patterns: vec![ot] }),
                            expression: None,
                        };
                    }

                    // 5. Wrap: OrderBy → Project → (Distinct if needed).
                    let ordered = GraphPattern::OrderBy {
                        inner: Box::new(flat),
                        expression: sort_exprs,
                    };
                    let projected = if project_vars.is_empty() {
                        ordered
                    } else {
                        GraphPattern::Project {
                            inner: Box::new(ordered),
                            variables: project_vars,
                        }
                    };
                    return Ok(if *distinct {
                        GraphPattern::Distinct {
                            inner: Box::new(projected),
                        }
                    } else {
                        projected
                    });
                }

                // Default path: inner is not a Projection (e.g. mid-pipeline
                // OrderBy from a WITH clause).
                let inner_pat = self.lower_op_as_query(inner)?;
                let expressions = keys
                    .iter()
                    .map(|sk| self.lower_order_key(sk))
                    .collect::<Result<Vec<_>, _>>()?;
                let flushed = self.flush_pending(inner_pat);
                Ok(GraphPattern::OrderBy {
                    inner: Box::new(flushed),
                    expression: expressions,
                })
            }
            Op::Distinct { inner } => {
                self.return_distinct = true;
                let inner_pat = self.lower_op_as_query(inner)?;
                Ok(GraphPattern::Distinct {
                    inner: Box::new(inner_pat),
                })
            }
            Op::Projection {
                inner,
                items,
                distinct,
            } => {
                // Peel off Limit/Skip modifiers that should become SPARQL OFFSET/LIMIT
                // modifiers OUTSIDE the SELECT clause (Project). In the LQA IR, the
                // RETURN Projection is always outermost, but mid-pipeline SKIP/LIMIT
                // from a preceding WITH clause may be nested inside it.
                let mut peel_inner: &Op = inner;
                let mut slice_start: usize = 0;
                let mut slice_length: Option<usize> = None;
                loop {
                    match peel_inner {
                        Op::Limit { inner: li, count } => {
                            slice_length = Some(expr_to_usize(count)?);
                            peel_inner = li;
                        }
                        Op::Skip { inner: si, count } => {
                            slice_start = expr_to_usize(count)?;
                            peel_inner = si;
                        }
                        _ => break,
                    }
                }

                self.return_distinct = *distinct;
                let (inner_gp, _agg_vars) = self.lower_projection_inner(peel_inner, items)?;
                let project_vars = self.build_project_vars(items)?;
                let flushed = self.flush_pending(inner_gp);

                // For RETURN *, populate projected_columns from all currently in-scope
                // user-visible variables (scan vars + scalar vars from WITH aliases).
                // Without this, SELECT * returns internal SPARQL variables too and the
                // result schema would have no columns.
                let is_star = items.iter().any(|pi| pi.alias == "*");
                if is_star && self.projected_columns.is_empty() {
                    // Collect all in-scope Cypher variable names (node vars + WITH aliases)
                    // that don't start with _ (internal generated vars).
                    let mut scope_vars: Vec<String> = self
                        .scan_vars
                        .iter()
                        .chain(self.scalar_vars.iter())
                        .filter(|v| !v.starts_with('_'))
                        .cloned()
                        .collect();
                    scope_vars.sort();
                    scope_vars.dedup();
                    for v in &scope_vars {
                        self.projected_columns.push(scalar_col(v));
                    }
                }

                let explicit_project_vars: Vec<Variable> = if is_star {
                    // Project only the scope vars instead of SELECT *
                    self.projected_columns
                        .iter()
                        .map(|c| {
                            use crate::result_mapping::schema::ColumnKind;
                            match &c.kind {
                                ColumnKind::Scalar { var } => Self::var(var),
                                _ => Self::var(&c.name),
                            }
                        })
                        .collect()
                } else {
                    project_vars
                };

                let mut projected = if explicit_project_vars.is_empty() {
                    flushed
                } else {
                    GraphPattern::Project {
                        inner: Box::new(flushed),
                        variables: explicit_project_vars,
                    }
                };
                if *distinct {
                    projected = GraphPattern::Distinct {
                        inner: Box::new(projected),
                    };
                }
                // Hoist mid-pipeline Limit/Skip (peeled above) OUTSIDE the SELECT Project.
                // This ensures OFFSET/LIMIT appear as outer SELECT modifiers rather than
                // being embedded inside a subquery group.
                if slice_length.is_some() || slice_start > 0 {
                    projected = GraphPattern::Slice {
                        inner: Box::new(projected),
                        start: slice_start,
                        length: slice_length,
                    };
                }
                Ok(projected)
            }
            other => self.lower_op(other),
        }
    }

    fn lower_projection_inner(
        &mut self,
        inner: &Op,
        proj_items: &[ProjItem],
    ) -> Result<(GraphPattern, Vec<Variable>), PolygraphError> {
        if let Op::GroupBy {
            inner: gb_inner,
            group_keys,
            agg_items,
        } = inner
        {
            let inner_gp = self.lower_op(gb_inner)?;
            let flushed = self.flush_pending(inner_gp);

            let group_key_set: std::collections::HashSet<&str> =
                group_keys.iter().map(|s| s.as_str()).collect();

            // Group variables from keys; start collecting them.
            let mut group_vars: Vec<Variable> = Vec::new();
            // Complex group-key expressions (not simple Variable or Property) that need
            // a BIND inside the group body before the Group is formed.
            let mut complex_group_binds: Vec<(Variable, SparExpr)> = Vec::new();

            // For GROUP BY key expressions that are Property accesses
            // (e.g. `n.city AS city`), generate the property triple
            // inside the Group inner using the alias variable directly —
            // no fresh intermediate variable.  That way the GROUP BY
            // variable is the same as the output alias.
            for pi in proj_items {
                if !group_key_set.contains(pi.alias.as_str()) {
                    continue; // aggregate output or wildcard, skip
                }
                let alias_var = Self::var(&pi.alias);
                match &pi.expr {
                    Expr::Variable { name, .. } => {
                        // Pre-bound variable — just track it as a group var.
                        group_vars.push(Self::var(name));
                    }
                    Expr::Property(node_expr, prop_key) => {
                        // Property access: produce ?node :prop ?groupkey triple inside
                        // the Group inner.  If the alias name collides with a MATCH
                        // scan variable (e.g. `RETURN n.num AS n, count(n) AS c`), the
                        // alias and the node variable share the same SPARQL name, which
                        // would generate the self-referential triple `?n :num ?n`.
                        // Instead use a fresh GROUP BY variable in that case.
                        let node_var = match node_expr.as_ref() {
                            Expr::Variable { name, .. } => Self::var(name),
                            other => {
                                return Err(PolygraphError::Unsupported {
                                    construct: format!("complex GROUP BY key expr {:?}", other),
                                    spec_ref: "openCypher 9 §3.4".into(),
                                    reason: "non-variable base in property GROUP BY key".into(),
                                })
                            }
                        };
                        let pred = NamedNodePattern::NamedNode(self.prop_iri(prop_key));
                        let (group_var, col) = if self.scan_vars.contains(pi.alias.as_str()) {
                            // alias collides with a scan variable: use a fresh GROUP BY
                            // key variable and record it in projected_columns so that
                            // build_project_vars can look it up by alias name.
                            let fresh = self.fresh(prop_key);
                            let col = ProjectedColumn {
                                name: pi.alias.clone(),
                                kind: crate::result_mapping::schema::ColumnKind::Scalar {
                                    var: fresh.as_str().to_string(),
                                },
                            };
                            (fresh, col)
                        } else {
                            (alias_var.clone(), proj_item_col(pi))
                        };
                        // Use OPTIONAL so a missing property gives null rather than
                        // dropping the row, matching openCypher null propagation.
                        self.pending_optional_triples.push(TriplePattern {
                            subject: TermPattern::Variable(node_var),
                            predicate: pred,
                            object: TermPattern::Variable(group_var.clone()),
                        });
                        group_vars.push(group_var);
                        self.projected_columns.push(col);
                    }
                    _ => {
                        // Complex expression (e.g. `x IS NULL`, function calls) —
                        // evaluate and bind to the alias variable inside the group body
                        // so that GROUP BY can reference it.
                        let e = self.lower_expr(&pi.expr)?;
                        // Flush any required pending triples produced by lower_expr.
                        // We'll apply them to the group inner below.
                        let pending_req = std::mem::take(&mut self.pending_triples);
                        // Re-add them so flush_pending picks them up later.
                        self.pending_triples.extend(pending_req);
                        complex_group_binds.push((alias_var.clone(), e));
                        group_vars.push(alias_var.clone());
                        self.projected_columns.push(proj_item_col(pi));
                    }
                }
            }

            // Lower aggregates — this may add property-access triples to
            // `pending_triples` (e.g. AVG(n.age) → fresh ?_age_0 + pending triple).
            // Those triples must live INSIDE the Group inner, not outside it.
            //
            // IMPORTANT: aggregate argument property triples MUST be required (not
            // optional) so that nodes lacking the aggregated property are excluded
            // from the group. Oxigraph's GROUP BY returns unbound aggregates when
            // any group member has an unbound value (OPTIONAL behavour), whereas
            // Cypher semantics say "sum only non-null values" (skip unbound). By
            // making the aggregate-argument triples required, only rows where the
            // property EXISTS participate in the group, matching Cypher semantics.
            let aggregates = agg_items
                .iter()
                .map(|ai| {
                    let opt_before = self.pending_optional_triples.len();
                    let result = self.lower_agg_item(ai);
                    // Move any newly-added optional triples (from the agg arg) to
                    // required triples so that property absence excludes the row.
                    let new_opts: Vec<_> =
                        self.pending_optional_triples.drain(opt_before..).collect();
                    self.pending_triples.extend(new_opts);
                    result
                })
                .collect::<Result<Vec<_>, _>>()?;

            // Flush all pending triples (group-key property triples +
            // agg-arg property triples) into the inner pattern.
            let mut group_inner = self.flush_pending(flushed);
            // Apply complex group-key BIND expressions inside the group body.
            for (var, expr) in complex_group_binds {
                group_inner = GraphPattern::Extend {
                    inner: Box::new(group_inner),
                    variable: var,
                    expression: expr,
                };
            }

            // Add FILTER(BOUND(?var)) for variables that came from UNWIND lists
            // containing null. Without this, Oxigraph's MAX/MIN/SUM over a VALUES
            // block that has UNDEF rows may return null instead of the correct
            // aggregate of non-null values (non-conformant with SPARQL 1.1 §18.5.1).
            for null_var_name in &self.unwind_null_vars {
                // Only filter if this variable actually appears in the aggregation.
                let var_appears_in_agg = agg_items.iter().any(|ai| {
                    if let Expr::Aggregate { arg: Some(a), .. } = &ai.expr {
                        matches!(a.as_ref(), Expr::Variable { name, .. } if name == null_var_name)
                    } else {
                        false
                    }
                });
                if var_appears_in_agg {
                    let bound_filter = SparExpr::Bound(Self::var(null_var_name));
                    group_inner = GraphPattern::Filter {
                        expr: bound_filter,
                        inner: Box::new(group_inner),
                    };
                }
            }

            let group_pattern = GraphPattern::Group {
                inner: Box::new(group_inner),
                variables: group_vars.clone(),
                aggregates,
            };

            // Emit any remaining non-group, non-agg proj items as Extends
            // (aggregate output aliases need no Extend; they're bound by the Group).
            let agg_alias_set: std::collections::HashSet<&str> =
                agg_items.iter().map(|a| a.alias.as_str()).collect();
            let mut extended = group_pattern;
            for pi in proj_items {
                if agg_alias_set.contains(pi.alias.as_str()) {
                    // Aggregate output: variable bound by the Group pattern.
                    self.projected_columns.push(proj_item_col(pi));
                    continue;
                }
                if group_key_set.contains(pi.alias.as_str()) {
                    // Already handled above (property triple or variable passthrough).
                    continue;
                }
                let sparql_expr = self.lower_expr(&pi.expr)?;
                let flush = std::mem::take(&mut self.pending_triples);
                let target = Self::var(&pi.alias);
                extended = GraphPattern::Extend {
                    inner: Box::new(extended),
                    variable: target,
                    expression: sparql_expr,
                };
                if !flush.is_empty() {
                    extended = join(GraphPattern::Bgp { patterns: flush }, extended);
                }
                self.projected_columns.push(proj_item_col(pi));
            }

            Ok((extended, group_vars))
        } else {
            let inner_gp = self.lower_op(inner)?;
            let mut extended = inner_gp;
            for pi in proj_items {
                if pi.alias == "*" {
                    continue;
                }
                if let Expr::Variable { name, .. } = &pi.expr {
                    if *name == pi.alias {
                        self.projected_columns.push(scalar_col(name.clone()));
                        continue;
                    }
                }
                let sparql_expr = self.lower_expr(&pi.expr)?;
                // Flush required and optional pending triples BEFORE wrapping in
                // Extend, so that OPTIONAL { } blocks appear BEFORE the BIND and
                // the bound variables are in scope when BIND executes.
                extended = self.flush_pending(extended);
                // If the alias name collides with an existing scan variable (node or
                // edge variable from MATCH), use a fresh SPARQL variable to avoid
                // the SPARQL error "cannot BIND to a variable already in scope".
                // The result schema maps the fresh variable back to the alias name.
                let (bind_var, col): (Variable, ProjectedColumn) =
                    if self.scan_vars.contains(pi.alias.as_str()) {
                        let fresh = self.fresh(&format!("proj_{}", pi.alias));
                        let col = ProjectedColumn {
                            name: pi.display_name.as_deref().unwrap_or(&pi.alias).to_owned(),
                            kind: crate::result_mapping::schema::ColumnKind::Scalar {
                                var: fresh.as_str().to_string(),
                            },
                        };
                        (fresh, col)
                    } else {
                        let v = Self::var(&pi.alias);
                        (v, proj_item_col(pi))
                    };
                extended = GraphPattern::Extend {
                    inner: Box::new(extended),
                    variable: bind_var,
                    expression: sparql_expr,
                };
                self.projected_columns.push(col);
            }
            Ok((extended, vec![]))
        }
    }

    fn lower_agg_item(
        &mut self,
        ai: &AggItem,
    ) -> Result<(Variable, AggregateExpression), PolygraphError> {
        let out_var = Self::var(&ai.alias);
        if let Expr::Aggregate {
            kind,
            distinct,
            arg,
        } = &ai.expr
        {
            let agg_expr = match kind {
                AggKind::Count => {
                    if let Some(arg_expr) = arg {
                        AggregateExpression::FunctionCall {
                            name: AggregateFunction::Count,
                            expr: self.lower_expr(arg_expr)?,
                            distinct: *distinct,
                        }
                    } else {
                        AggregateExpression::CountSolutions {
                            distinct: *distinct,
                        }
                    }
                }
                AggKind::Sum => AggregateExpression::FunctionCall {
                    name: AggregateFunction::Sum,
                    expr: self.lower_expr(arg.as_deref().unwrap())?,
                    distinct: *distinct,
                },
                AggKind::Avg => AggregateExpression::FunctionCall {
                    name: AggregateFunction::Avg,
                    expr: self.lower_expr(arg.as_deref().unwrap())?,
                    distinct: *distinct,
                },
                AggKind::Min => AggregateExpression::FunctionCall {
                    name: AggregateFunction::Min,
                    expr: self.lower_expr(arg.as_deref().unwrap())?,
                    distinct: *distinct,
                },
                AggKind::Max => AggregateExpression::FunctionCall {
                    name: AggregateFunction::Max,
                    expr: self.lower_expr(arg.as_deref().unwrap())?,
                    distinct: *distinct,
                },
                AggKind::Collect => {
                    // collect() maps to SPARQL GROUP_CONCAT which serialises to a
                    // string, not a list.  Cypher `collect()` semantics require a
                    // true list type which the LQA path doesn't yet encode; fall
                    // back to the legacy translator for any query using collect().
                    return Err(PolygraphError::Unsupported {
                        construct: "collect() aggregate".into(),
                        spec_ref: "openCypher 9 §3.4.6".into(),
                        reason: "collect() requires list encoding not yet in LQA path; legacy fallback applies".into(),
                    });
                }
                AggKind::CountStar => AggregateExpression::CountSolutions {
                    distinct: *distinct,
                },
            };
            Ok((out_var, agg_expr))
        } else {
            Err(PolygraphError::Translation {
                message: format!("AggItem.expr is not Aggregate: {:?}", ai.expr),
            })
        }
    }

    fn build_project_vars(&self, items: &[ProjItem]) -> Result<Vec<Variable>, PolygraphError> {
        if items.iter().any(|pi| pi.alias == "*") {
            return Ok(vec![]);
        }
        // Use the SPARQL variable names from projected_columns (which may have
        // been renamed to avoid collision with existing scan vars).
        // Fall back to the alias directly if not found in projected_columns.
        Ok(items
            .iter()
            .filter(|pi| pi.alias != "*")
            .map(|pi| {
                // Find the matching projected_column by output name
                if let Some(col) = self
                    .projected_columns
                    .iter()
                    .rev()
                    .find(|c| c.name == pi.alias)
                {
                    use crate::result_mapping::schema::ColumnKind;
                    match &col.kind {
                        ColumnKind::Scalar { var } => Self::var(var),
                        _ => Self::var(&pi.alias),
                    }
                } else {
                    Self::var(&pi.alias)
                }
            })
            .collect())
    }

    fn lower_op(&mut self, op: &Op) -> Result<GraphPattern, PolygraphError> {
        match op {
            Op::Unit => Ok(GraphPattern::Bgp { patterns: vec![] }),

            Op::Scan {
                variable,
                label,
                extra_labels,
            } => {
                self.scan_vars.insert(variable.clone());
                let subj = TermPattern::Variable(Self::var(variable));

                // If the variable is already bound by a BIND/Extend (scalar_vars), it is not
                // a fresh graph scan — the variable already holds a computed value.  Emitting
                // a sentinel or label triple would wildcard-match the graph and corrupt the
                // value when it is null (from an upstream OPTIONAL).  Return an empty BGP.
                if self.scalar_vars.contains(variable.as_str()) {
                    return Ok(GraphPattern::Bgp { patterns: vec![] });
                }

                let label = match label {
                    Some(l) => l,
                    None => {
                        // Unlabeled node scan: use the __node existence sentinel.
                        // Every graph node carries exactly one `<base:__node> <base:__node>`
                        // triple inserted by the TCK data loader.
                        let sentinel_iri =
                            NamedNode::new_unchecked(format!("{}{}", self.base_iri, "__node"));
                        return Ok(GraphPattern::Bgp {
                            patterns: vec![TriplePattern {
                                subject: subj,
                                predicate: NamedNodePattern::NamedNode(sentinel_iri.clone()),
                                object: TermPattern::NamedNode(sentinel_iri),
                            }],
                        });
                    }
                };

                let mut patterns = vec![TriplePattern {
                    subject: subj.clone(),
                    predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDF_TYPE)),
                    object: TermPattern::NamedNode(self.label_iri(label)),
                }];

                for lbl in extra_labels {
                    patterns.push(TriplePattern {
                        subject: subj.clone(),
                        predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(RDF_TYPE)),
                        object: TermPattern::NamedNode(self.label_iri(lbl)),
                    });
                }

                Ok(GraphPattern::Bgp { patterns })
            }

            Op::Expand {
                inner,
                from,
                rel_var,
                to,
                rel_types,
                direction,
                range,
                ..
            } => {
                self.scan_vars.insert(from.clone());
                self.scan_vars.insert(to.clone());
                let inner_pat = self.lower_op(inner)?;

                // If the FROM node may be null (produced by OPTIONAL MATCH or BIND on a
                // nullable expression), enforce Cypher's null→empty-match semantics.
                // A FILTER(BOUND(?from)) inside a flat SPARQL group does not block the
                // subsequent edge triple pattern from rebinding ?from.  Instead, use a
                // "safe-from" variable: IF(BOUND(?from), ?from, <__null__>) — which yields
                // a dummy IRI when ?from is unbound.  Since <__null__> has no outgoing
                // triples, the edge pattern produces no results for the null case.
                let (inner_pat, from_tp) = if self.nullable.contains(from.as_str()) {
                    let null_iri = NamedNode::new_unchecked(format!("{}__null__", self.base_iri));
                    let safe_var = self.fresh("safe_from");
                    // Flush pending optional triples before the BIND so they appear in order.
                    let flushed = self.flush_pending(inner_pat);
                    let safe_bind = GraphPattern::Extend {
                        inner: Box::new(flushed),
                        variable: safe_var.clone(),
                        expression: SparExpr::If(
                            Box::new(SparExpr::Bound(Self::var(from))),
                            Box::new(SparExpr::Variable(Self::var(from))),
                            Box::new(SparExpr::NamedNode(null_iri)),
                        ),
                    };
                    (safe_bind, TermPattern::Variable(safe_var))
                } else {
                    (inner_pat, TermPattern::Variable(Self::var(from)))
                };

                let from_tp = from_tp;
                let to_var = Self::var(to);
                let to_tp = TermPattern::Variable(to_var.clone());

                // ── Variable-length paths ───────────────────────────────────
                if let Some(path_range) = range {
                    let edge_bgp =
                        self.lower_varlen(from_tp, to_tp, rel_types, direction, path_range)?;
                    // For the target endpoint of a variable-length path, add the
                    // __node sentinel triple so that literal values (e.g. property
                    // values accidentally reachable via 1-hop paths) are excluded.
                    // Labeled endpoint nodes also carry this triple (the TCK loader
                    // inserts it for every graph node), so this is safe also for
                    // labeled-endpoint varlen patterns.
                    let sentinel_iri =
                        NamedNode::new_unchecked(format!("{}{}", self.base_iri, "__node"));
                    let to_sentinel = GraphPattern::Bgp {
                        patterns: vec![TriplePattern {
                            subject: TermPattern::Variable(to_var),
                            predicate: NamedNodePattern::NamedNode(sentinel_iri.clone()),
                            object: TermPattern::NamedNode(sentinel_iri),
                        }],
                    };
                    return Ok(join(inner_pat, join(edge_bgp, to_sentinel)));
                }

                // ── Named relationship variable ──────────────────────────────
                if let Some(rv) = rel_var {
                    // Register static rel types for type(r) fast path.
                    self.edge_types.insert(rv.clone(), rel_types.clone());
                    // Track rel-var in scan_vars so that RETURN projections using the
                    // same alias (e.g. RETURN type(r) AS r) use a fresh SPARQL variable
                    // instead of conflicting with the BIND(?pred AS ?r) emitted inside
                    // the match pattern, which would raise an Oxigraph "SELECT overrides
                    // an existing variable" error.
                    self.scan_vars.insert(rv.clone());

                    // Snapshot edge_vars to detect prior named edges in the same MATCH.
                    let prior_edge_vars: Vec<(String, EdgeVarInfo)> = self
                        .edge_vars
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect();

                    let edge_bgp = self.lower_expand_rel_var(from, to, rel_types, direction, rv)?;

                    // For undirected edges add a relationship-uniqueness FILTER to prevent
                    // the backward UNION arm from reusing a prior edge in the same MATCH.
                    // IMPORTANT: the filter must be applied AFTER join(inner_pat, edge_bgp),
                    // not inside edge_bgp's sub-group. Variables from prior patterns are only
                    // visible at the outer scope; placing FILTER inside the sub-group causes
                    // Oxigraph to evaluate it with those variables unbound (→ 0 rows).
                    if matches!(direction, Direction::Undirected) {
                        let cur_info = self.edge_vars.get(rv.as_str()).cloned();
                        let cur_pred = cur_info.as_ref().map(|i| i.pred.clone());
                        let prior_anon = self.anon_edge_info.clone();
                        let joined = join(inner_pat, edge_bgp);
                        return Ok(match cur_pred {
                            Some(cp) if !prior_edge_vars.is_empty() || !prior_anon.is_empty() => {
                                match self.build_rel_uniqueness_filter_expr(
                                    from,
                                    to,
                                    &cp,
                                    &prior_edge_vars,
                                    &prior_anon,
                                ) {
                                    Some(expr) => GraphPattern::Filter {
                                        expr,
                                        inner: Box::new(joined),
                                    },
                                    None => joined,
                                }
                            }
                            _ => joined,
                        });
                    }

                    return Ok(join(inner_pat, edge_bgp));
                }

                // ── Anonymous expansion (no rel-var, no path range) ──────────
                // Deduplicate rel_types to avoid generating duplicate UNION branches
                // that would return the same row multiple times (e.g. [:T|:T]).
                let rel_types_dedup: Vec<&String> = {
                    let mut seen = std::collections::HashSet::new();
                    rel_types
                        .iter()
                        .filter(|rt| seen.insert(rt.as_str()))
                        .collect()
                };

                // Snapshot prior edge info before registering this hop.
                let prior_named_edges: Vec<(String, EdgeVarInfo)> = self
                    .edge_vars
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let prior_anon_edges: Vec<(String, EdgePred, String)> = self.anon_edge_info.clone();

                // For an anonymous any-type edge, record the hop for later uniqueness checks.
                let anon_pred_for_filter: Option<EdgePred> = if rel_types_dedup.is_empty() {
                    // pred_var is created inside the if-block below; capture it here too.
                    None // will be set after fresh() call
                } else if rel_types_dedup.len() == 1 {
                    Some(EdgePred::Static(NamedNode::new_unchecked(format!(
                        "{}{}",
                        self.base_iri, rel_types_dedup[0]
                    ))))
                } else {
                    None // multi-type UNION: skip uniqueness for now
                };

                let rel_bgp = if rel_types_dedup.is_empty() {
                    let pred_var = self.fresh("rtype");
                    // Record this hop (from_var, pred, to_var) for later uniqueness checks.
                    if let TermPattern::Variable(fv) = &from_tp {
                        self.anon_edge_info.push((
                            fv.as_str().to_string(),
                            EdgePred::Dynamic(pred_var.clone()),
                            to.to_string(),
                        ));
                    }
                    self.lower_expand_any_type(from_tp, pred_var, to_tp, direction)
                } else if rel_types_dedup.len() == 1 {
                    let pred = NamedNodePattern::NamedNode(NamedNode::new_unchecked(format!(
                        "{}{}",
                        self.base_iri, rel_types_dedup[0]
                    )));
                    self.lower_expand_typed(from_tp, pred, to_tp, direction)
                } else {
                    let mut union_pats: Vec<GraphPattern> = rel_types_dedup
                        .iter()
                        .map(|rt| {
                            let pred = NamedNodePattern::NamedNode(NamedNode::new_unchecked(
                                format!("{}{}", self.base_iri, rt),
                            ));
                            self.lower_expand_typed(from_tp.clone(), pred, to_tp.clone(), direction)
                        })
                        .collect();
                    let first = union_pats.remove(0);
                    union_pats
                        .into_iter()
                        .fold(first, |acc, pat| GraphPattern::Union {
                            left: Box::new(acc),
                            right: Box::new(pat),
                        })
                };

                // For undirected anonymous expansions, apply a relationship-uniqueness
                // FILTER *after* the join so that prior edge variables are in scope.
                let joined = join(inner_pat, rel_bgp);
                // Determine the current edge's predicate info (from anon_edge_info or static).
                let cur_pred_opt: Option<EdgePred> = if rel_types_dedup.is_empty() {
                    // Was recorded in anon_edge_info; retrieve the last entry.
                    self.anon_edge_info.last().map(|(_, ep, _)| ep.clone())
                } else {
                    anon_pred_for_filter
                };
                let result = if matches!(direction, Direction::Undirected) {
                    if let Some(cur_pred) = cur_pred_opt {
                        if !prior_named_edges.is_empty() || !prior_anon_edges.is_empty() {
                            match self.build_rel_uniqueness_filter_expr(
                                from,
                                to,
                                &cur_pred,
                                &prior_named_edges,
                                &prior_anon_edges,
                            ) {
                                Some(expr) => GraphPattern::Filter {
                                    expr,
                                    inner: Box::new(joined),
                                },
                                None => joined,
                            }
                        } else {
                            joined
                        }
                    } else {
                        joined
                    }
                } else {
                    joined
                };
                Ok(result)
            }

            Op::Values { bindings } => {
                if bindings.is_empty() {
                    return Ok(GraphPattern::Bgp { patterns: vec![] });
                }
                let vars: Vec<Variable> =
                    bindings.iter().map(|(name, _)| Self::var(name)).collect();
                let row = bindings
                    .iter()
                    .map(|(_, expr)| literal_to_ground(expr))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(GraphPattern::Values {
                    variables: vars,
                    bindings: vec![row],
                })
            }

            Op::Selection { inner, predicate } => {
                // Special case: WHERE after `WITH DISTINCT`. The WHERE predicate must be
                // applied INSIDE the DISTINCT scope while the pre-WITH scan variables are
                // still visible. If we apply the filter after the `SELECT DISTINCT` subquery,
                // the scan variables (like `?a`) are hidden by the subquery boundary.
                if let Op::Projection {
                    inner: proj_inner,
                    items,
                    distinct: true,
                } = inner.as_ref()
                {
                    // Replicate the flat Extend path for the projection body, then
                    // inject the WHERE filter, then wrap in DISTINCT.
                    let mut gp = self.lower_op(proj_inner)?;
                    for pi in items {
                        if pi.alias == "*" {
                            continue;
                        }
                        match &pi.expr {
                            Expr::Variable { name, .. } if *name == pi.alias => { /* passthrough */
                            }
                            _ => {
                                let e = self.lower_expr(&pi.expr)?;
                                gp = self.flush_pending(gp);
                                // Use collision-aware BIND (same as the no-collision flat path)
                                let (bind_var, _col) = if self.scan_vars.contains(pi.alias.as_str())
                                {
                                    let fresh = self.fresh(&format!("proj_{}", pi.alias));
                                    (fresh, ())
                                } else {
                                    (Self::var(&pi.alias), ())
                                };
                                gp = GraphPattern::Extend {
                                    inner: Box::new(gp),
                                    variable: bind_var,
                                    expression: e,
                                };
                            }
                        }
                    }
                    // Apply the WHERE predicate inside this scope (before DISTINCT).
                    let filter_expr = self.lower_expr(predicate)?;
                    gp = self.flush_pending(gp);
                    gp = GraphPattern::Filter {
                        expr: filter_expr,
                        inner: Box::new(gp),
                    };
                    // Wrap in SELECT DISTINCT.
                    let distinct_vars: Vec<Variable> = items
                        .iter()
                        .filter(|pi| pi.alias != "*")
                        .map(|pi| Self::var(&pi.alias))
                        .collect();
                    gp = GraphPattern::Distinct {
                        inner: Box::new(GraphPattern::Project {
                            inner: Box::new(gp),
                            variables: distinct_vars,
                        }),
                    };
                    // Update scope like the collision path does.
                    let passthrough_scans: std::collections::HashSet<String> = items
                        .iter()
                        .filter_map(|pi| {
                            if let Expr::Variable { name, .. } = &pi.expr {
                                if *name == pi.alias && self.scan_vars.contains(name.as_str()) {
                                    return Some(name.clone());
                                }
                            }
                            None
                        })
                        .collect();
                    self.scan_vars.retain(|v| passthrough_scans.contains(v));
                    for pi in items {
                        if pi.alias != "*" && !passthrough_scans.contains(&pi.alias) {
                            self.scalar_vars.insert(pi.alias.clone());
                        }
                    }
                    return Ok(gp);
                }
                let inner_pat = self.lower_op(inner)?;
                let expr = self.lower_expr(predicate)?;
                let flushed = self.flush_pending(inner_pat);
                Ok(GraphPattern::Filter {
                    expr,
                    inner: Box::new(flushed),
                })
            }

            Op::Projection {
                inner,
                items,
                distinct,
            } => {
                // Mid-pipeline Projection (from WITH clause): flatten as Extend + Filter
                // rather than creating a nested SELECT subquery. A nested SELECT in SPARQL
                // hides internal variables from the outer scope, breaking WHERE clauses and
                // RETURN expressions that reference those variables.
                //
                // Exception 1: when the inner op is a GroupBy (WITH clause that contains
                // aggregation, e.g. `WITH count(n) AS c`), delegate to lower_projection_inner
                // which knows how to generate the SPARQL GROUP BY / aggregate pattern.
                if matches!(inner.as_ref(), Op::GroupBy { .. }) {
                    let (gp, _) = self.lower_projection_inner(inner, items)?;
                    let flushed = self.flush_pending(gp);
                    for pi in items {
                        if pi.alias != "*" {
                            self.scalar_vars.insert(pi.alias.clone());
                        }
                    }
                    return Ok(flushed);
                }

                // Exception 2: when an alias shadows a MATCH scan variable (e.g.
                // `WITH n.name AS n`), the flat BIND fails in Oxigraph because `?n` is
                // already in scope.  In that case we wrap the inner in a SELECT subquery
                // that projects only the fresh intermediate variables, thereby hiding the
                // old `?n` from the outer scope so that the rename BIND is valid.
                let mut gp = self.lower_op(inner)?;

                // Detect whether any non-passthrough alias collides with a scan variable.
                let has_collision = items.iter().any(|pi| {
                    pi.alias != "*"
                        && !matches!(&pi.expr, Expr::Variable { name, .. } if *name == pi.alias)
                        && self.scan_vars.contains(pi.alias.as_str())
                });

                if has_collision {
                    // Subquery-isolation path.
                    //
                    // For every non-passthrough item, compute the expression and bind it
                    // to a fresh intermediate variable inside the inner scope.  Then wrap
                    // the whole inner in a SELECT projecting only those fresh vars (and
                    // any pure passthrough vars that are not scan-collisions).  Finally,
                    // emit outer BINDs that rename each fresh var to its alias.
                    let mut subq_vars: Vec<Variable> = Vec::new();
                    let mut outer_binds: Vec<(Variable, SparExpr)> = Vec::new();

                    for pi in items {
                        if pi.alias == "*" {
                            continue;
                        }
                        if let Expr::Variable { name, .. } = &pi.expr {
                            if *name == pi.alias {
                                // Pure passthrough — always export from subquery.
                                subq_vars.push(Self::var(name));
                                // Propagate nullable status for passthrough.
                                if self.nullable.contains(name.as_str()) {
                                    self.nullable.insert(pi.alias.clone());
                                }
                                continue;
                            }
                        }
                        // Non-passthrough: bind to a fresh intermediate variable.
                        let e = self.lower_expr(&pi.expr)?;
                        // If the expression depends on nullable vars, mark both fresh and alias.
                        let is_nullable = self.expr_contains_nullable_var(&pi.expr);
                        gp = self.flush_pending(gp);
                        let fresh = self.fresh(&format!("mid_{}", pi.alias));
                        gp = GraphPattern::Extend {
                            inner: Box::new(gp),
                            variable: fresh.clone(),
                            expression: e,
                        };
                        if is_nullable {
                            self.nullable.insert(fresh.as_str().to_owned());
                            self.nullable.insert(pi.alias.clone());
                        }
                        subq_vars.push(fresh.clone());
                        outer_binds.push((Self::var(&pi.alias), SparExpr::Variable(fresh)));
                    }

                    // Wrap in a SELECT subquery projecting only collected vars.
                    // This hides the old scan variables from the outer scope.
                    gp = GraphPattern::Project {
                        inner: Box::new(gp),
                        variables: subq_vars,
                    };

                    // Apply rename BINDs in the outer scope, where the alias is unbound.
                    for (alias_var, fresh_expr) in outer_binds {
                        gp = GraphPattern::Extend {
                            inner: Box::new(gp),
                            variable: alias_var,
                            expression: fresh_expr,
                        };
                    }

                    // Update compiler scope: passthrough scan vars remain scan vars;
                    // computed aliases become scalar (post-bind) vars.
                    let passthrough_scans: std::collections::HashSet<String> = items
                        .iter()
                        .filter_map(|pi| {
                            if let Expr::Variable { name, .. } = &pi.expr {
                                if *name == pi.alias && self.scan_vars.contains(name.as_str()) {
                                    return Some(name.clone());
                                }
                            }
                            None
                        })
                        .collect();
                    self.scan_vars.retain(|v| passthrough_scans.contains(v));
                    for pi in items {
                        if pi.alias != "*" && !passthrough_scans.contains(&pi.alias) {
                            self.scalar_vars.insert(pi.alias.clone());
                        }
                    }

                    return Ok(gp);
                }

                // No collision: original flat Extend path.
                for pi in items {
                    if pi.alias == "*" {
                        continue;
                    }
                    match &pi.expr {
                        Expr::Variable { name, .. } if *name == pi.alias => {
                            // Pure passthrough — no Extend needed.
                            // If the source var is nullable, the passthrough alias is too.
                            if self.nullable.contains(name.as_str()) {
                                self.nullable.insert(pi.alias.clone());
                            }
                        }
                        _ => {
                            // Emit Extend to bind the alias variable.
                            let e = self.lower_expr(&pi.expr)?;
                            // Propagate nullability: if the expression references a nullable var,
                            // the alias may also be null (e.g. COALESCE of optional vars).
                            if self.expr_contains_nullable_var(&pi.expr) {
                                self.nullable.insert(pi.alias.clone());
                            }
                            // Flush both required and optional pending triples BEFORE the BIND
                            // so the OPTIONAL { } blocks that define helper variables appear
                            // in SPARQL order before the BIND that uses them.
                            gp = self.flush_pending(gp);
                            gp = GraphPattern::Extend {
                                inner: Box::new(gp),
                                variable: Self::var(&pi.alias),
                                expression: e,
                            };
                            self.scalar_vars.insert(pi.alias.clone());
                            // Track temporal-typed variables for date/time arithmetic.
                            if let Expr::Literal(Literal::TypedLiteral(_, xsd_type)) = &pi.expr {
                                if !xsd_type.is_empty() {
                                    self.temporal_type_vars
                                        .insert(pi.alias.clone(), xsd_type.clone());
                                }
                            }
                            // Track scalar literal values for temporal/duration property access.
                            match &pi.expr {
                                Expr::Literal(Literal::TypedLiteral(val, _)) => {
                                    self.scalar_lit_vals.insert(pi.alias.clone(), val.clone());
                                }
                                Expr::Literal(Literal::String(val)) => {
                                    self.scalar_lit_vals.insert(pi.alias.clone(), val.clone());
                                }
                                _ => {}
                            }
                        }
                    }
                }
                // WITH DISTINCT: wrap in a SELECT DISTINCT subquery projecting only
                // the aliased output variables, so that subsequent operations see a
                // deduplicated scope.
                if *distinct {
                    let distinct_vars: Vec<Variable> = items
                        .iter()
                        .filter(|pi| pi.alias != "*")
                        .map(|pi| Self::var(&pi.alias))
                        .collect();
                    gp = GraphPattern::Distinct {
                        inner: Box::new(GraphPattern::Project {
                            inner: Box::new(gp),
                            variables: distinct_vars,
                        }),
                    };
                }
                // Update scope: passthrough scan vars remain; non-passthrough scan
                // vars are hidden by the WITH clause and should not appear in RETURN *.
                {
                    let passthrough_scans: std::collections::HashSet<String> = items
                        .iter()
                        .filter_map(|pi| {
                            if let Expr::Variable { name, .. } = &pi.expr {
                                if *name == pi.alias && self.scan_vars.contains(name.as_str()) {
                                    return Some(name.clone());
                                }
                            }
                            None
                        })
                        .collect();
                    self.scan_vars.retain(|v| passthrough_scans.contains(v));
                }
                Ok(gp)
            }

            Op::GroupBy {
                inner,
                group_keys: _,
                agg_items: _,
            } => {
                // GroupBy mid-pipeline should not happen without a surrounding Projection;
                // lower the inner and propagate (the GroupBy is handled by lower_projection_inner).
                self.lower_op(inner)
            }

            Op::OrderBy { inner, keys } => {
                // When ORDER BY wraps a mid-pipeline Projection-over-GroupBy
                // (i.e. `WITH aggregation ORDER BY pre-scope-var`), substitute any
                // sort key that matches a projection item's expression with its alias
                // variable.  This is needed because the GroupBy (or a DISTINCT subquery)
                // creates a new scope where the original pre-scope variables (e.g. `?a`)
                // are no longer accessible, but the alias (e.g. `?name`) is bound.
                let alias_map: Vec<(&str, &Expr)> =
                    if let Op::Projection { items, .. } = inner.as_ref() {
                        // Build alias substitution for ALL projection items (not just GroupBy),
                        // so that `ORDER BY a.name` after `WITH DISTINCT a.name AS name` can
                        // substitute `a.name` → `name` when `?a` is hidden by the subquery.
                        items
                            .iter()
                            .map(|pi| (pi.alias.as_str(), &pi.expr))
                            .collect()
                    } else {
                        vec![]
                    };

                // Also build a recursive substitution table for compound sort key expressions
                // like `ORDER BY a.name + 'C'` where `a.name` is a projection alias sub-expression.
                let alias_subst_for_sort: Vec<(&str, &Expr)> =
                    if let Op::Projection { items, .. } = inner.as_ref() {
                        items
                            .iter()
                            .filter(|pi| !matches!(&pi.expr, Expr::Variable { .. }))
                            .map(|pi| (pi.alias.as_str(), &pi.expr))
                            .collect()
                    } else {
                        vec![]
                    };

                let inner_pat = self.lower_op(inner)?;
                let expressions = keys
                    .iter()
                    .map(|sk| {
                        // Try to substitute the sort key with a matching alias when
                        // the sort expression directly matches a projection item's expr.
                        if let Some((alias, _)) = alias_map
                            .iter()
                            .find(|(_, proj_expr)| exprs_equivalent(&sk.expr, proj_expr))
                        {
                            let sparql_expr = SparExpr::Variable(Self::var(*alias));
                            return Ok(match sk.dir {
                                SortDir::Asc => OrderExpression::Asc(sparql_expr),
                                SortDir::Desc => OrderExpression::Desc(sparql_expr),
                            });
                        }
                        // For compound expressions (e.g. `a.name + 'C'`), recursively
                        // substitute underlying expressions with their alias variables.
                        // This handles cases where inner scope vars (e.g. `a`) are hidden
                        // by a GroupBy or DISTINCT subquery; the alias (e.g. `name`) is
                        // in scope instead. We use the REVERSE substitution:
                        // `underlying_expr → Variable(alias)`.
                        let effective_owned: Expr;
                        let effective_expr: &Expr = if !alias_subst_for_sort.is_empty() {
                            effective_owned =
                                subst_exprs_with_aliases(&sk.expr, &alias_subst_for_sort);
                            &effective_owned
                        } else {
                            &sk.expr
                        };
                        let sparql_expr =
                            self.lower_order_key_expr(effective_expr, sk.dir.clone())?;
                        Ok(sparql_expr)
                    })
                    .collect::<Result<Vec<_>, PolygraphError>>()?;
                let flushed = self.flush_pending(inner_pat);
                Ok(GraphPattern::OrderBy {
                    inner: Box::new(flushed),
                    expression: expressions,
                })
            }

            Op::Skip { inner, count } => {
                let start = expr_to_usize(count)?;
                let inner_pat = self.lower_op_as_query(inner)?;
                Ok(GraphPattern::Slice {
                    inner: Box::new(inner_pat),
                    start,
                    length: None,
                })
            }

            Op::Limit { inner, count } => {
                let length = expr_to_usize(count)?;
                let inner_pat = self.lower_op_as_query(inner)?;
                // Merge a preceding SKIP-only Slice (from Op::Skip) into a single
                // OFFSET+LIMIT—same as the top-level lower_op_as_query(Limit) handler.
                let (start, unwrapped) = match inner_pat {
                    GraphPattern::Slice {
                        inner: skip_inner,
                        start: skip_start,
                        length: None,
                    } => (skip_start, *skip_inner),
                    other => (0, other),
                };
                Ok(GraphPattern::Slice {
                    inner: Box::new(unwrapped),
                    start,
                    length: Some(length),
                })
            }

            Op::Distinct { inner } => {
                let inner_pat = self.lower_op(inner)?;
                Ok(GraphPattern::Distinct {
                    inner: Box::new(inner_pat),
                })
            }

            Op::Unwind {
                inner,
                list,
                variable,
            } => {
                if let Expr::List(items) = list {
                    let inner_pat = self.lower_op(inner)?;
                    let var = Self::var(variable);
                    let has_null = items
                        .iter()
                        .any(|i| matches!(i, Expr::Literal(crate::lqa::expr::Literal::Null)));
                    let bindings = items
                        .iter()
                        .map(|item| literal_to_ground(item).map(|g| vec![g]))
                        .collect::<Result<Vec<_>, _>>()?;
                    // Track variables produced by UNWIND lists that contain nulls.
                    // Aggregates using these variables need FILTER(BOUND(?var)) to work
                    // around Oxigraph returning null instead of the non-null values
                    // (Oxigraph's aggregate UNDEF handling does not fully match SPARQL 1.1).
                    if has_null {
                        self.unwind_null_vars.insert(variable.clone());
                    }
                    let values = GraphPattern::Values {
                        variables: vec![var],
                        bindings,
                    };
                    Ok(join(inner_pat, values))
                } else {
                    Err(PolygraphError::Unsupported {
                        construct: "UNWIND with variable/expression list in LQA path".into(),
                        spec_ref: "openCypher 9 §4.5".into(),
                        reason: "runtime list UNWIND requires legacy path".into(),
                    })
                }
            }

            Op::UnionAll { left, right } => {
                let lp = self.lower_op(left)?;
                let rp = self.lower_op(right)?;
                Ok(GraphPattern::Union {
                    left: Box::new(lp),
                    right: Box::new(rp),
                })
            }

            Op::Union { left, right } => {
                let lp = self.lower_op(left)?;
                let rp = self.lower_op(right)?;
                Ok(GraphPattern::Distinct {
                    inner: Box::new(GraphPattern::Union {
                        left: Box::new(lp),
                        right: Box::new(rp),
                    }),
                })
            }

            Op::CartesianProduct { left, right } => {
                let lp = self.lower_op(left)?;
                let rp = self.lower_op(right)?;
                // If the right pattern is a Filter (i.e. the right side came from a
                // MATCH…WHERE clause), lift the FILTER above the join so that variables
                // bound by BIND in the left side remain visible.  Without this, spargebra
                // wraps the right side in a nested `{ }` group that hides outer BIND
                // variables from the FILTER condition.
                match rp {
                    GraphPattern::Filter { expr, inner } => Ok(GraphPattern::Filter {
                        expr,
                        inner: Box::new(join(lp, *inner)),
                    }),
                    other => Ok(join(lp, other)),
                }
            }

            Op::LeftOuterJoin {
                left,
                right,
                condition,
            } => {
                // Lower the left (required) side first to establish baseline scan_vars.
                let lp = self.lower_op(left)?;
                let scan_after_left = self.scan_vars.clone();
                // Lower the right (optional) side.
                let rp = self.lower_op(right)?;
                // Any variable introduced on the right side that was not already scanned
                // on the left side is potentially null (OPTIONAL MATCH semantics).
                for v in self.scan_vars.iter() {
                    if !scan_after_left.contains(v.as_str()) {
                        self.nullable.insert(v.clone());
                    }
                }
                let cond = condition.as_ref().map(|c| self.lower_expr(c)).transpose()?;
                let flushed_l = self.flush_pending(lp);
                let flushed_r = self.flush_pending(rp);
                Ok(GraphPattern::LeftJoin {
                    left: Box::new(flushed_l),
                    right: Box::new(flushed_r),
                    expression: cond,
                })
            }

            Op::Subquery { outer, inner } => {
                let outer_pat = self.lower_op(outer)?;
                let inner_pat = self.lower_op(inner)?;
                Ok(join(outer_pat, inner_pat))
            }

            Op::Create { .. }
            | Op::Merge { .. }
            | Op::Set { .. }
            | Op::Delete { .. }
            | Op::Remove { .. } => Err(PolygraphError::Unsupported {
                construct: "write clause".into(),
                spec_ref: "openCypher 9 §6".into(),
                reason: "write operators are not handled in the LQA SPARQL path".into(),
            }),

            Op::Foreach { .. } => Err(PolygraphError::Unsupported {
                construct: "FOREACH".into(),
                spec_ref: "openCypher 9 §4.8".into(),
                reason: "FOREACH not yet in LQA path".into(),
            }),

            Op::Call { .. } => Err(PolygraphError::Unsupported {
                construct: "CALL subquery".into(),
                spec_ref: "openCypher 9 §7".into(),
                reason: "CALL subquery not yet in LQA path".into(),
            }),
        }
    }

    // ── Relationship expansion helpers ────────────────────────────────────────

    /// Build a FILTER that enforces relationship uniqueness for an undirected edge.
    ///
    /// In Cypher, no two relationship variables in the same MATCH may bind to the
    /// same underlying stored triple.  For undirected edges the backward UNION arm
    /// may traverse the same stored triple as a prior directed or undirected edge.
    /// This filter adds NOT conditions to exclude those cases.
    /// Build a relationship-uniqueness FILTER expression for a new undirected hop.
    ///
    /// Returns `None` if no filter is needed, or `Some(expr)`.
    ///
    /// Uses the full bidirectional edge-identity check:
    /// edges P=(p_from, p_pred, p_to) and C=(c_from, c_pred, c_to) are the same RDF
    /// triple iff pred_P = pred_C AND (
    ///   (from_P = from_C AND to_P = to_C)   // same direction
    ///   OR
    ///   (from_P = to_C AND to_P = from_C)   // reversed
    /// )
    /// The NOT of this is the uniqueness condition.
    ///
    /// This form correctly handles both chained patterns (shared middle variable)
    /// and cyclic patterns (where the destination variable equals a prior source variable).
    ///
    /// **IMPORTANT**: the returned expression must be applied *outside* (after)
    /// the join of both edge patterns, not inside the sub-group for the new edge.
    /// Variables from the prior edge patterns are only visible at the outer scope.
    fn build_rel_uniqueness_filter_expr(
        &self,
        c_from: &str, // current hop's from-variable
        c_to: &str,   // current hop's to-variable
        cur_pred: &EdgePred,
        // Prior named-rel-var edges: (rel_var_name, EdgeVarInfo{subj=from, obj=to}).
        prior_named: &[(String, EdgeVarInfo)],
        // Prior anonymous-edge hops: (from_var_name, EdgePred, to_var_name).
        prior_anon: &[(String, EdgePred, String)],
    ) -> Option<SparExpr> {
        let cur_pred_expr: SparExpr = match cur_pred {
            EdgePred::Static(iri) => SparExpr::NamedNode(iri.clone()),
            EdgePred::Dynamic(v) => SparExpr::Variable(v.clone()),
        };

        let mut conditions: Vec<SparExpr> = Vec::new();

        // Helper closure: build NOT(pred_eq AND (same_dir OR reversed))
        let mut push_cond = |p_from: &str, p_to: &str, p_pred_expr: SparExpr| {
            let same_dir = SparExpr::And(
                Box::new(SparExpr::Equal(
                    Box::new(SparExpr::Variable(Self::var(p_from))),
                    Box::new(SparExpr::Variable(Self::var(c_from))),
                )),
                Box::new(SparExpr::Equal(
                    Box::new(SparExpr::Variable(Self::var(p_to))),
                    Box::new(SparExpr::Variable(Self::var(c_to))),
                )),
            );
            let reversed = SparExpr::And(
                Box::new(SparExpr::Equal(
                    Box::new(SparExpr::Variable(Self::var(p_from))),
                    Box::new(SparExpr::Variable(Self::var(c_to))),
                )),
                Box::new(SparExpr::Equal(
                    Box::new(SparExpr::Variable(Self::var(p_to))),
                    Box::new(SparExpr::Variable(Self::var(c_from))),
                )),
            );
            conditions.push(SparExpr::Not(Box::new(SparExpr::And(
                Box::new(SparExpr::Equal(
                    Box::new(p_pred_expr),
                    Box::new(cur_pred_expr.clone()),
                )),
                Box::new(SparExpr::Or(Box::new(same_dir), Box::new(reversed))),
            ))));
        };

        for (_, prior) in prior_named {
            let prior_pred_expr: SparExpr = match &prior.pred {
                EdgePred::Static(iri) => SparExpr::NamedNode(iri.clone()),
                EdgePred::Dynamic(v) => SparExpr::Variable(v.clone()),
            };
            push_cond(&prior.subj, &prior.obj, prior_pred_expr);
        }

        for (prior_from_var, prior_pred, prior_to_var) in prior_anon {
            let prior_pred_expr: SparExpr = match prior_pred {
                EdgePred::Static(iri) => SparExpr::NamedNode(iri.clone()),
                EdgePred::Dynamic(v) => SparExpr::Variable(v.clone()),
            };
            push_cond(prior_from_var, prior_to_var, prior_pred_expr);
        }

        if conditions.is_empty() {
            None
        } else {
            Some(
                conditions
                    .into_iter()
                    .reduce(|acc, c| SparExpr::And(Box::new(acc), Box::new(c)))
                    .unwrap(),
            )
        }
    }

    /// Collect all node/edge variables introduced (bound) by an Op subtree.
    /// Used to identify which variables become nullable after an OPTIONAL MATCH.
    fn collect_op_scan_vars(op: &Op) -> HashSet<String> {
        let mut vars = HashSet::new();
        Self::collect_op_scan_vars_inner(op, &mut vars);
        vars
    }

    fn collect_op_scan_vars_inner(op: &Op, vars: &mut HashSet<String>) {
        match op {
            Op::Scan { variable, .. } => {
                vars.insert(variable.clone());
            }
            Op::Expand {
                from,
                to,
                rel_var,
                inner,
                ..
            } => {
                vars.insert(from.clone());
                vars.insert(to.clone());
                if let Some(rv) = rel_var {
                    vars.insert(rv.clone());
                }
                Self::collect_op_scan_vars_inner(inner, vars);
            }
            Op::Projection { inner, .. }
            | Op::Selection { inner, .. }
            | Op::OrderBy { inner, .. }
            | Op::Limit { inner, .. }
            | Op::Skip { inner, .. }
            | Op::Distinct { inner } => Self::collect_op_scan_vars_inner(inner, vars),
            Op::LeftOuterJoin { left, right, .. }
            | Op::CartesianProduct { left, right }
            | Op::UnionAll { left, right }
            | Op::Union { left, right }
            | Op::Subquery {
                outer: left,
                inner: right,
            } => {
                Self::collect_op_scan_vars_inner(left, vars);
                Self::collect_op_scan_vars_inner(right, vars);
            }
            Op::GroupBy { inner, .. } => Self::collect_op_scan_vars_inner(inner, vars),
            Op::Unit | Op::Values { .. } => {}
            _ => {}
        }
    }

    /// Check whether an expression references any nullable variable.
    fn expr_contains_nullable_var(&self, expr: &Expr) -> bool {
        use crate::lqa::expr::Expr;
        match expr {
            Expr::Variable { name, .. } => self.nullable.contains(name.as_str()),
            Expr::FunctionCall { args, .. } => {
                args.iter().any(|a| self.expr_contains_nullable_var(a))
            }
            Expr::Add(l, r)
            | Expr::Sub(l, r)
            | Expr::Mul(l, r)
            | Expr::Div(l, r)
            | Expr::Mod(l, r)
            | Expr::Pow(l, r)
            | Expr::And(l, r)
            | Expr::Or(l, r)
            | Expr::Xor(l, r)
            | Expr::Comparison(_, l, r) => {
                self.expr_contains_nullable_var(l) || self.expr_contains_nullable_var(r)
            }
            Expr::Unary(_, e)
            | Expr::Not(e)
            | Expr::IsNull(e)
            | Expr::IsNotNull(e)
            | Expr::Property(e, _) => self.expr_contains_nullable_var(e),
            Expr::List(items) => items.iter().any(|e| self.expr_contains_nullable_var(e)),
            _ => false,
        }
    }

    /// Lower a named relationship-variable expand into SPARQL.
    ///
    /// Registers the edge in `self.edge_vars` so that downstream property-access
    /// and `type(r)` expressions can resolve it.  Returns the BGP/UNION pattern.
    fn lower_expand_rel_var(
        &mut self,
        from: &str,
        to: &str,
        rel_types: &[String],
        direction: &Direction,
        rv: &str,
    ) -> Result<GraphPattern, PolygraphError> {
        use spargebra::term::GroundTerm;
        let from_tp = TermPattern::Variable(Self::var(from));
        let to_tp = TermPattern::Variable(Self::var(to));

        // Canonical RDF triple subject/object (used for property-access reification).
        let (rdf_subj, rdf_obj) = match direction {
            Direction::Outgoing | Direction::Undirected => (from.to_owned(), to.to_owned()),
            Direction::Incoming => (to.to_owned(), from.to_owned()),
        };

        if rel_types.is_empty() {
            // Untyped: use a variable predicate with a negated-property-set filter
            // to exclude internal triples (rdf:type, __node).
            let pred_var = self.fresh(&format!("{rv}_type"));
            let bgp = self.lower_expand_any_type(from_tp, pred_var.clone(), to_tp, direction);
            // Bind the rel-var to the dynamic predicate variable so IS NULL checks
            // (`r IS NULL` → `!BOUND(?r)`) work correctly when in OPTIONAL MATCH.
            // `?pred_var` is bound by the triple pattern when a match is found.
            let bgp_with_marker = GraphPattern::Extend {
                inner: Box::new(bgp),
                variable: Self::var(rv),
                expression: SparExpr::Variable(pred_var.clone()),
            };
            self.edge_vars.insert(
                rv.to_owned(),
                EdgeVarInfo {
                    subj: rdf_subj,
                    obj: rdf_obj,
                    pred: EdgePred::Dynamic(pred_var),
                    undirected: matches!(direction, Direction::Undirected),
                },
            );
            return Ok(bgp_with_marker);
        }

        if rel_types.len() == 1 {
            // Typed single-hop: static predicate.
            let iri = NamedNode::new_unchecked(format!("{}{}", self.base_iri, &rel_types[0]));
            let pred = NamedNodePattern::NamedNode(iri.clone());
            let bgp = self.lower_expand_typed(from_tp, pred, to_tp, direction);
            // Bind the rel-var to the relationship type IRI so that IS NULL checks
            // (`r IS NULL` → `!BOUND(?r)`) work correctly in OPTIONAL MATCH contexts.
            // When the OPTIONAL triple pattern matches, `?rv` is bound; when the
            // OPTIONAL has no match, `?rv` remains unbound (null).
            let xsd_any_uri = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#anyURI");
            let marker_lit = SparLit::new_typed_literal(
                format!("{}{}", self.base_iri, &rel_types[0]),
                xsd_any_uri,
            );
            let bgp_with_marker = GraphPattern::Extend {
                inner: Box::new(bgp),
                variable: Self::var(rv),
                expression: SparExpr::Literal(marker_lit),
            };
            self.edge_vars.insert(
                rv.to_owned(),
                EdgeVarInfo {
                    subj: rdf_subj,
                    obj: rdf_obj,
                    pred: EdgePred::Static(iri),
                    undirected: matches!(direction, Direction::Undirected),
                },
            );
            return Ok(bgp_with_marker);
        }

        // Multi-type: introduce a pred variable bound via VALUES so reification can
        // use it, then UNION branches per type each with that VALUES constraint.
        let pred_var = self.fresh(&format!("{rv}_type"));
        let bindings: Vec<Vec<Option<GroundTerm>>> = rel_types
            .iter()
            .map(|rt| {
                vec![Some(GroundTerm::NamedNode(NamedNode::new_unchecked(
                    format!("{}{}", self.base_iri, rt),
                )))]
            })
            .collect();
        let values_pat = GraphPattern::Values {
            variables: vec![pred_var.clone()],
            bindings,
        };
        let triple_tp = TermPattern::Variable(Self::var(from));
        let triple_obj = TermPattern::Variable(Self::var(to));
        let bgp = match direction {
            Direction::Outgoing => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: triple_tp,
                    predicate: NamedNodePattern::Variable(pred_var.clone()),
                    object: triple_obj,
                }],
            },
            Direction::Incoming => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: triple_obj,
                    predicate: NamedNodePattern::Variable(pred_var.clone()),
                    object: triple_tp,
                }],
            },
            Direction::Undirected => {
                let fwd = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: triple_tp.clone(),
                        predicate: NamedNodePattern::Variable(pred_var.clone()),
                        object: triple_obj.clone(),
                    }],
                };
                let bwd = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: triple_obj,
                        predicate: NamedNodePattern::Variable(pred_var.clone()),
                        object: triple_tp,
                    }],
                };
                GraphPattern::Union {
                    left: Box::new(fwd),
                    right: Box::new(bwd),
                }
            }
        };
        // Join VALUES before the triple pattern so pred_var is bound first.
        let edge_bgp = join(values_pat, bgp);
        self.edge_vars.insert(
            rv.to_owned(),
            EdgeVarInfo {
                subj: rdf_subj,
                obj: rdf_obj,
                pred: EdgePred::Dynamic(pred_var),
                undirected: matches!(direction, Direction::Undirected),
            },
        );
        Ok(edge_bgp)
    }

    /// Lower a variable-length expansion into a SPARQL property path pattern.
    fn lower_varlen(
        &mut self,
        from_tp: TermPattern,
        to_tp: TermPattern,
        rel_types: &[String],
        direction: &Direction,
        range: &crate::lqa::op::PathRange,
    ) -> Result<GraphPattern, PolygraphError> {
        use spargebra::algebra::PropertyPathExpression as PPE;

        let lower = range.lower;
        let upper = range.upper;

        // ── Special cases before building the property path ──────────────────
        // Empty interval: no paths can match, return an always-false filter.
        if let Some(hi) = upper {
            if hi < lower {
                return Ok(GraphPattern::Filter {
                    expr: SparExpr::Literal(SparLit::new_typed_literal(
                        "false",
                        NamedNode::new_unchecked(XSD_BOOLEAN),
                    )),
                    inner: Box::new(GraphPattern::Bgp { patterns: vec![] }),
                });
            }
        }
        // Zero hops exactly: `?from = ?to` (the identity path).
        if lower == 0 && upper == Some(0) {
            if let (TermPattern::Variable(from_var), TermPattern::Variable(to_var)) =
                (&from_tp, &to_tp)
            {
                return Ok(GraphPattern::Extend {
                    inner: Box::new(GraphPattern::Bgp { patterns: vec![] }),
                    variable: to_var.clone(),
                    expression: SparExpr::Variable(from_var.clone()),
                });
            }
        }

        // Zero-to-N hops (*0..n for n >= 2): UNION of the identity path (zero hops)
        // with the 1..n-hop property path.
        // Note: *0..0 is handled above (zero-hops-only).
        //       *0..1 is handled by ZeroOrOne(base_ppe) further down.
        if lower == 0 {
            if let Some(n) = upper {
                if n >= 2 {
                    // Build the zero-hop branch: anchor ?from via the __node sentinel so
                    // that SPARQL engines that evaluate UNION arms independently (without
                    // outer bindings) still see ?from as bound to a real node.
                    // Without the sentinel, BIND(?from AS ?to) inside a UNION arm can be
                    // evaluated with ?from = unbound, producing null for ?to and matching
                    // ALL nodes in the subsequent sentinel check.
                    let zero_gp = if let (TermPattern::Variable(fv), TermPattern::Variable(tv)) =
                        (&from_tp, &to_tp)
                    {
                        let sentinel_iri =
                            NamedNode::new_unchecked(format!("{}{}", self.base_iri, "__node"));
                        let from_sentinel = GraphPattern::Bgp {
                            patterns: vec![TriplePattern {
                                subject: TermPattern::Variable(fv.clone()),
                                predicate: NamedNodePattern::NamedNode(sentinel_iri.clone()),
                                object: TermPattern::NamedNode(sentinel_iri),
                            }],
                        };
                        GraphPattern::Extend {
                            inner: Box::new(from_sentinel),
                            variable: tv.clone(),
                            expression: SparExpr::Variable(fv.clone()),
                        }
                    } else {
                        GraphPattern::Bgp { patterns: vec![] }
                    };
                    // Build the 1..n-hop branch by recursing with lower=1.
                    let nonzero_range = crate::lqa::op::PathRange {
                        lower: 1,
                        upper: Some(n),
                    };
                    let nonzero_gp =
                        self.lower_varlen(from_tp, to_tp, rel_types, direction, &nonzero_range)?;
                    return Ok(GraphPattern::Union {
                        left: Box::new(zero_gp),
                        right: Box::new(nonzero_gp),
                    });
                }
            }
        }

        // Build the base PPE from the rel types.
        let base_ppe: PPE = if rel_types.is_empty() {
            // Untyped: exclude internal predicates.
            PPE::NegatedPropertySet(vec![
                NamedNode::new_unchecked(RDF_TYPE),
                NamedNode::new_unchecked(format!("{}{}", self.base_iri, "__node")),
            ])
        } else if rel_types.len() == 1 {
            PPE::NamedNode(NamedNode::new_unchecked(format!(
                "{}{}",
                self.base_iri, &rel_types[0]
            )))
        } else {
            let ppes: Vec<PPE> = rel_types
                .iter()
                .map(|rt| {
                    PPE::NamedNode(NamedNode::new_unchecked(format!("{}{}", self.base_iri, rt)))
                })
                .collect();
            ppes.into_iter()
                .reduce(|a, b| PPE::Alternative(Box::new(a), Box::new(b)))
                .expect("non-empty")
        };

        // Build quantified PPE based on range.
        let quantified_ppe: PPE = match (lower, upper) {
            // Exact single hop — treat as simple triple (use Expand without range).
            // This shouldn't occur since is_lqa_safe no longer guards this, but
            // handle it anyway by using a 1-hop path.
            (1, Some(1)) => base_ppe,
            // *1.. or bare * (one or more hops)
            (1, None) => PPE::OneOrMore(Box::new(base_ppe)),
            // *0.. (zero or more hops)
            (0, None) => PPE::ZeroOrMore(Box::new(base_ppe)),
            // *0..1 (zero or one hop)
            (0, Some(1)) => PPE::ZeroOrOne(Box::new(base_ppe)),
            // *M.. (M or more hops, M > 1): Sequence of M fixed + OneOrMore
            (m, None) if m > 1 => {
                let mut ppe = PPE::OneOrMore(Box::new(base_ppe.clone()));
                for _ in 0..m.saturating_sub(1) {
                    ppe = PPE::Sequence(Box::new(base_ppe.clone()), Box::new(ppe));
                }
                ppe
            }
            // *M..N bounded: unroll as UNION of path lengths M..=N (max 10 hops)
            (m, Some(n)) if n > 1 => {
                let max_n = n.min(m + 10);
                // Build a chain PPE for a given number of hops.
                let chain = |count: u64| -> PPE {
                    let mut p = base_ppe.clone();
                    for _ in 1..count {
                        p = PPE::Sequence(Box::new(base_ppe.clone()), Box::new(p));
                    }
                    p
                };
                let ranges: Vec<PPE> = (m.max(1)..=max_n).map(chain).collect();
                if ranges.is_empty() {
                    base_ppe
                } else {
                    ranges
                        .into_iter()
                        .reduce(|a, b| PPE::Alternative(Box::new(a), Box::new(b)))
                        .expect("non-empty")
                }
            }
            _ => base_ppe,
        };

        // Apply direction.
        let path_ppe = match direction {
            Direction::Outgoing => quantified_ppe,
            Direction::Incoming => PPE::Reverse(Box::new(quantified_ppe)),
            Direction::Undirected => PPE::Alternative(
                Box::new(quantified_ppe.clone()),
                Box::new(PPE::Reverse(Box::new(quantified_ppe))),
            ),
        };

        Ok(GraphPattern::Path {
            subject: from_tp,
            path: path_ppe,
            object: to_tp,
        })
    }

    fn lower_expand_typed(
        &self,
        from: TermPattern,
        pred: NamedNodePattern,
        to: TermPattern,
        direction: &Direction,
    ) -> GraphPattern {
        match direction {
            Direction::Outgoing => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: from,
                    predicate: pred,
                    object: to,
                }],
            },
            Direction::Incoming => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: to,
                    predicate: pred,
                    object: from,
                }],
            },
            Direction::Undirected => {
                let fwd = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: from.clone(),
                        predicate: pred.clone(),
                        object: to.clone(),
                    }],
                };
                let bwd = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: to,
                        predicate: pred,
                        object: from,
                    }],
                };
                GraphPattern::Union {
                    left: Box::new(fwd),
                    right: Box::new(bwd),
                }
            }
        }
    }

    fn lower_expand_any_type(
        &self,
        from: TermPattern,
        pred_var: Variable,
        to: TermPattern,
        direction: &Direction,
    ) -> GraphPattern {
        let edge_pat = match direction {
            Direction::Outgoing => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: from,
                    predicate: NamedNodePattern::Variable(pred_var),
                    object: to.clone(),
                }],
            },
            Direction::Incoming => GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: to.clone(),
                    predicate: NamedNodePattern::Variable(pred_var),
                    object: from,
                }],
            },
            Direction::Undirected => {
                let fwd = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: from.clone(),
                        predicate: NamedNodePattern::Variable(pred_var.clone()),
                        object: to.clone(),
                    }],
                };
                // Backward branch: subject=to, object=from.
                // Add FILTER(?to != ?from) to prevent self-loop duplication:
                // when `from == to` (same SPARQL variable), both branches would match
                // identically. The FILTER suppresses the backward branch for self-loops
                // so they are counted exactly once (from the forward branch only).
                let bwd_bgp = GraphPattern::Bgp {
                    patterns: vec![TriplePattern {
                        subject: to.clone(),
                        predicate: NamedNodePattern::Variable(pred_var),
                        object: from.clone(),
                    }],
                };
                let bwd = if from == to {
                    // Self-loop case: suppress backward branch entirely.
                    GraphPattern::Filter {
                        expr: SparExpr::Literal(SparLit::new_typed_literal(
                            "false",
                            NamedNode::new_unchecked(XSD_BOOLEAN),
                        )),
                        inner: Box::new(bwd_bgp),
                    }
                } else if let (TermPattern::Variable(to_v), TermPattern::Variable(from_v)) =
                    (&to, &from)
                {
                    // Distinct variables: add FILTER(?to_v != ?from_v) to avoid
                    // duplicate matches when the two endpoints happen to bind to the
                    // same node in a concrete graph.
                    let filter_expr = SparExpr::Not(Box::new(SparExpr::Equal(
                        Box::new(SparExpr::Variable(to_v.clone())),
                        Box::new(SparExpr::Variable(from_v.clone())),
                    )));
                    GraphPattern::Filter {
                        expr: filter_expr,
                        inner: Box::new(bwd_bgp),
                    }
                } else {
                    bwd_bgp
                };
                GraphPattern::Union {
                    left: Box::new(fwd),
                    right: Box::new(bwd),
                }
            }
        };

        // Add endpoint sentinel: ensure `to` is an actual PG node.
        // Without this, untyped expand matches rdf:type and property triples as well
        // (since those share the same predicate namespace in our RDF encoding).
        if let TermPattern::Variable(to_var) = &to {
            let sentinel_iri = NamedNode::new_unchecked(format!("{}{}", self.base_iri, "__node"));
            let sentinel_bgp = GraphPattern::Bgp {
                patterns: vec![TriplePattern {
                    subject: TermPattern::Variable(to_var.clone()),
                    predicate: NamedNodePattern::NamedNode(sentinel_iri.clone()),
                    object: TermPattern::NamedNode(sentinel_iri),
                }],
            };
            join(edge_pat, sentinel_bgp)
        } else {
            edge_pat
        }
    }

    // ── Expression lowering ───────────────────────────────────────────────────

    fn lower_expr(&mut self, expr: &Expr) -> Result<SparExpr, PolygraphError> {
        match expr {
            Expr::Variable { name, .. } => Ok(SparExpr::Variable(Self::var(name))),

            Expr::Literal(lit) => match lit {
                Literal::Integer(n) => Ok(Self::lit_integer(*n)),
                Literal::Float(f) => Ok(Self::lit_double(*f)),
                Literal::String(s) => Ok(Self::lit_str(s)),
                Literal::Boolean(b) => Ok(Self::lit_bool(*b)),
                Literal::Null => {
                    let null_var = self.fresh("null");
                    Ok(SparExpr::Variable(null_var))
                }
                Literal::TypedLiteral(value, datatype) => {
                    if datatype.is_empty() {
                        // Duration and similar are stored as plain string literals
                        Ok(Self::lit_str(value))
                    } else {
                        Ok(SparExpr::Literal(SparLit::new_typed_literal(
                            value.clone(),
                            NamedNode::new_unchecked(datatype.clone()),
                        )))
                    }
                }
            },

            Expr::Property(base, key) => {
                // If the base is an edge (relationship) variable, use RDF-star reification
                // to access the edge property.  Two triples must stay together in one
                // OPTIONAL block: the rdf:reifies triple and the property triple.
                if let Expr::Variable { name, .. } = base.as_ref() {
                    if let Some(edge_info) = self.edge_vars.get(name.as_str()).cloned() {
                        let subj_var = Self::var(&edge_info.subj);
                        let obj_var = Self::var(&edge_info.obj);
                        let pred_pat = match &edge_info.pred {
                            EdgePred::Static(iri) => NamedNodePattern::NamedNode(iri.clone()),
                            EdgePred::Dynamic(v) => NamedNodePattern::Variable(v.clone()),
                        };
                        let prop_var = self.fresh(key);
                        let reif_var = self.fresh(&format!("reif_{}", key));
                        let rdf_reifies = NamedNode::new_unchecked(
                            "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies",
                        );

                        if edge_info.undirected {
                            // For undirected edges the actual stored triple might be in
                            // either direction.  Use a UNION inside the OPTIONAL so that
                            // the reification is checked for BOTH <<(subj pred obj)>> and
                            // <<(obj pred subj)>>, covering the forward AND backward UNION
                            // arms of the original undirected MATCH.
                            let fwd_triple = TermPattern::Triple(Box::new(TriplePattern {
                                subject: TermPattern::Variable(subj_var.clone()),
                                predicate: pred_pat.clone(),
                                object: TermPattern::Variable(obj_var.clone()),
                            }));
                            let bwd_triple = TermPattern::Triple(Box::new(TriplePattern {
                                subject: TermPattern::Variable(obj_var.clone()),
                                predicate: pred_pat,
                                object: TermPattern::Variable(subj_var),
                            }));
                            let prop_pred = NamedNodePattern::NamedNode(self.prop_iri(key));
                            let fwd_arm = GraphPattern::Bgp {
                                patterns: vec![
                                    TriplePattern {
                                        subject: TermPattern::Variable(reif_var.clone()),
                                        predicate: NamedNodePattern::NamedNode(rdf_reifies.clone()),
                                        object: fwd_triple,
                                    },
                                    TriplePattern {
                                        subject: TermPattern::Variable(reif_var.clone()),
                                        predicate: prop_pred.clone(),
                                        object: TermPattern::Variable(prop_var.clone()),
                                    },
                                ],
                            };
                            let bwd_arm = GraphPattern::Bgp {
                                patterns: vec![
                                    TriplePattern {
                                        subject: TermPattern::Variable(reif_var.clone()),
                                        predicate: NamedNodePattern::NamedNode(rdf_reifies),
                                        object: bwd_triple,
                                    },
                                    TriplePattern {
                                        subject: TermPattern::Variable(reif_var),
                                        predicate: prop_pred,
                                        object: TermPattern::Variable(prop_var.clone()),
                                    },
                                ],
                            };
                            let union_pat = GraphPattern::Union {
                                left: Box::new(fwd_arm),
                                right: Box::new(bwd_arm),
                            };
                            self.pending_optional_patterns.push(union_pat);
                        } else {
                            // Directed/outgoing/incoming: single reification direction.
                            let edge_triple_term = TermPattern::Triple(Box::new(TriplePattern {
                                subject: TermPattern::Variable(subj_var),
                                predicate: pred_pat,
                                object: TermPattern::Variable(obj_var),
                            }));
                            // Both reification triples must be in the same OPTIONAL block.
                            self.pending_optional_groups.push(vec![
                                TriplePattern {
                                    subject: TermPattern::Variable(reif_var.clone()),
                                    predicate: NamedNodePattern::NamedNode(rdf_reifies),
                                    object: edge_triple_term,
                                },
                                TriplePattern {
                                    subject: TermPattern::Variable(reif_var),
                                    predicate: NamedNodePattern::NamedNode(self.prop_iri(key)),
                                    object: TermPattern::Variable(prop_var.clone()),
                                },
                            ]);
                        }
                        return Ok(SparExpr::Variable(prop_var));
                    }
                }

                // If the base is a scalar variable (bound via BIND/Extend, not Scan),
                // it holds an RDF literal and cannot be the subject of a triple.
                // Special case: if the scalar is a temporal or duration literal with a
                // known compile-time value, extract the property at compile time.
                if let Expr::Variable { name, .. } = base.as_ref() {
                    if self.scalar_vars.contains(name) {
                        if let Some(lit_val) = self.scalar_lit_vals.get(name.as_str()).cloned() {
                            let extracted = lqa_scalar_temporal_prop(&lit_val, key);
                            if let Some(spar) = extracted {
                                return Ok(spar);
                            }
                        }
                        return Err(PolygraphError::Unsupported {
                            construct: "property access on scalar variable".into(),
                            spec_ref: "openCypher 9 §6.1".into(),
                            reason: format!(
                                "Variable `{name}` is bound to a scalar value (not a node); \
                                 triple-based property access is not applicable"
                            ),
                        });
                    }
                }
                let base_expr = self.lower_expr(base)?;
                let base_var = match &base_expr {
                    SparExpr::Variable(v) => v.clone(),
                    _ => {
                        return Err(PolygraphError::Unsupported {
                            construct: "property access on non-variable expression".into(),
                            spec_ref: "openCypher 9 §6.1".into(),
                            reason: "LQA path only supports property access on variables".into(),
                        })
                    }
                };
                let prop_var = self.fresh(key);
                // In openCypher, accessing an absent property returns null rather
                // than excluding the row.  Use OPTIONAL so a missing property
                // leaves the variable unbound (≡ null) rather than dropping the
                // solution — matching openCypher null-propagation semantics.
                self.pending_optional_triples.push(TriplePattern {
                    subject: TermPattern::Variable(base_var),
                    predicate: NamedNodePattern::NamedNode(self.prop_iri(key)),
                    object: TermPattern::Variable(prop_var.clone()),
                });
                Ok(SparExpr::Variable(prop_var))
            }

            Expr::Add(a, b) => {
                // In openCypher, `+` is overloaded: arithmetic for numbers,
                // string concatenation when either operand is a string, and
                // list concatenation when both operands are list-typed.
                // SPARQL `+` is arithmetic-only; strings must use CONCAT().

                // Temporal arithmetic: date/time + duration needs special SPARQL.
                if let Expr::Variable { name, .. } = a.as_ref() {
                    if let Some(xsd_type) = self.temporal_type_vars.get(name.as_str()).cloned() {
                        let la = self.lower_expr(a)?;
                        let lb = self.lower_expr(b)?;
                        let is_date = xsd_type.as_str() == XSD_DATE;
                        return Ok(
                            crate::translator::cypher::temporal_add_sparql(la, lb, is_date)
                        );
                    }
                }

                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                if lqa_expr_is_string(a) || lqa_expr_is_string(b) {
                    Ok(SparExpr::FunctionCall(Function::Concat, vec![la, lb]))
                } else if matches!(a.as_ref(), Expr::Property(..))
                    && matches!(b.as_ref(), Expr::Property(..))
                {
                    // Runtime list-concatenation heuristic: if STR(?a) starts with "["
                    // the property holds a serialised Cypher list — join the two by
                    // trimming the trailing "]" of a and the leading "[" of b.
                    let one = SparExpr::Literal(SparLit::new_typed_literal(
                        "1",
                        NamedNode::new_unchecked(XSD_INTEGER),
                    ));
                    let two = SparExpr::Literal(SparLit::new_typed_literal(
                        "2",
                        NamedNode::new_unchecked(XSD_INTEGER),
                    ));
                    let strlen_a = SparExpr::FunctionCall(Function::StrLen, vec![la.clone()]);
                    let len_minus_1 = SparExpr::Subtract(Box::new(strlen_a), Box::new(one.clone()));
                    let head = SparExpr::FunctionCall(
                        Function::SubStr,
                        vec![la.clone(), one, len_minus_1],
                    );
                    let tail = SparExpr::FunctionCall(Function::SubStr, vec![lb.clone(), two]);
                    let sep = SparExpr::Literal(SparLit::new_simple_literal(", "));
                    let concat = SparExpr::FunctionCall(Function::Concat, vec![head, sep, tail]);
                    let str_a = SparExpr::FunctionCall(Function::Str, vec![la.clone()]);
                    let bracket = SparExpr::Literal(SparLit::new_simple_literal("["));
                    let is_list = SparExpr::FunctionCall(Function::StrStarts, vec![str_a, bracket]);
                    let numeric_add = SparExpr::Add(Box::new(la), Box::new(lb));
                    Ok(SparExpr::If(
                        Box::new(is_list),
                        Box::new(concat),
                        Box::new(numeric_add),
                    ))
                } else {
                    if let Some(folded) = fold_numeric_binop('+', &la, &lb) {
                        return Ok(folded);
                    }
                    Ok(SparExpr::Add(Box::new(la), Box::new(lb)))
                }
            }
            Expr::Sub(a, b) => {
                // Temporal arithmetic: date/time - duration needs special SPARQL.
                if let Expr::Variable { name, .. } = a.as_ref() {
                    if let Some(xsd_type) = self.temporal_type_vars.get(name.as_str()).cloned() {
                        let la = self.lower_expr(a)?;
                        let rb = self.lower_expr(b)?;
                        let is_date = xsd_type.as_str() == XSD_DATE;
                        return Ok(
                            crate::translator::cypher::temporal_subtract_sparql(la, rb, is_date)
                        );
                    }
                }
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                if let Some(folded) = fold_numeric_binop('-', &la, &rb) {
                    return Ok(folded);
                }
                Ok(SparExpr::Subtract(Box::new(la), Box::new(rb)))
            }
            Expr::Mul(a, b) => {
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                if let Some(folded) = fold_numeric_binop('*', &la, &rb) {
                    return Ok(folded);
                }
                Ok(SparExpr::Multiply(Box::new(la), Box::new(rb)))
            }
            Expr::Div(a, b) => {
                // Constant-fold when both operands are numeric literals.  This
                // avoids the SPARQL integer/integer → decimal problem: `12 / 4`
                // folds to `3^^xsd:integer` at compile time rather than emitting
                // SPARQL arithmetic that returns `3.0^^xsd:decimal`.
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                if let Some(folded) = fold_numeric_binop('/', &la, &rb) {
                    return Ok(folded);
                }
                // Non-constant division: apply FLOOR when the RHS is a literal
                // integer to keep Cypher truncation semantics at runtime too.
                let rb_is_int_lit = matches!(
                    &rb,
                    SparExpr::Literal(l) if l.datatype().as_str() == XSD_INTEGER
                );
                let div = SparExpr::Divide(Box::new(la), Box::new(rb));
                if rb_is_int_lit {
                    Ok(SparExpr::FunctionCall(
                        spargebra::algebra::Function::Floor,
                        vec![div],
                    ))
                } else {
                    Ok(div)
                }
            }
            Expr::Mod(a, b) => {
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                if let Some(folded) = fold_numeric_binop('%', &la, &rb) {
                    return Ok(folded);
                }
                // a % b = a - FLOOR(a / b) * b
                // Matches the formula used by the legacy translator.
                let div = SparExpr::Divide(Box::new(la.clone()), Box::new(rb.clone()));
                let floor_div =
                    SparExpr::FunctionCall(spargebra::algebra::Function::Floor, vec![div]);
                let floor_times_b = SparExpr::Multiply(Box::new(floor_div), Box::new(rb));
                Ok(SparExpr::Subtract(Box::new(la), Box::new(floor_times_b)))
            }
            Expr::Pow(base, exp) => {
                if let (Expr::Literal(Literal::Integer(b)), Expr::Literal(Literal::Integer(e))) =
                    (base.as_ref(), exp.as_ref())
                {
                    let result = (*b as f64).powi(*e as i32);
                    if result.is_finite() {
                        return Ok(Self::lit_double(result));
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: "^ exponentiation with runtime operands".into(),
                    spec_ref: "openCypher 9 §6.3.1".into(),
                    reason: "SPARQL has no POW; legacy path handles this".into(),
                })
            }
            Expr::Unary(UnaryOp::Neg, e) => {
                let inner = self.lower_expr(e)?;
                // Constant-fold unary minus on numeric literals.
                if let SparExpr::Literal(ref l) = inner {
                    if l.datatype().as_str() == XSD_INTEGER {
                        if let Ok(n) = l.value().parse::<i64>() {
                            if let Some(neg) = n.checked_neg() {
                                return Ok(Self::lit_integer(neg));
                            }
                        }
                    } else if l.datatype().as_str() == XSD_DOUBLE {
                        if let Ok(f) = l.value().parse::<f64>() {
                            return Ok(Self::lit_double(-f));
                        }
                    }
                }
                Ok(SparExpr::UnaryMinus(Box::new(inner)))
            }
            Expr::Unary(UnaryOp::Not, e) => Ok(SparExpr::Not(Box::new(self.lower_expr(e)?))),
            Expr::Unary(UnaryOp::Pos, e) => self.lower_expr(e),

            Expr::Comparison(op, a, b) => {
                // Special case: `CmpOp::In` with a list literal RHS.
                // Expand to SparExpr::In(lhs, [item1, item2, ...]) before lowering
                // the RHS to avoid hitting the Unsupported path for Expr::List.
                if let (CmpOp::In, Expr::List(items)) = (op, b.as_ref()) {
                    let la = self.lower_expr(a)?;
                    let sparql_items = items
                        .iter()
                        .map(|item| self.lower_expr(item))
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(SparExpr::In(Box::new(la), sparql_items));
                }
                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                Ok(match op {
                    CmpOp::Eq => SparExpr::Equal(Box::new(la), Box::new(lb)),
                    CmpOp::Ne => {
                        SparExpr::Not(Box::new(SparExpr::Equal(Box::new(la), Box::new(lb))))
                    }
                    // For ordered comparisons (<, <=, >, >=), wrap each operand in
                    // bool_to_int_for_order so that boolean values (false=0, true=1)
                    // compare correctly even when one side is a NOT-expression.
                    // Without this, SPARQL serializes `!x >= y` without parens and the
                    // parser re-interprets it as `!(x >= y)` due to operator precedence.
                    CmpOp::Lt => SparExpr::Less(
                        Box::new(bool_to_int_for_order(la)),
                        Box::new(bool_to_int_for_order(lb)),
                    ),
                    CmpOp::Le => SparExpr::LessOrEqual(
                        Box::new(bool_to_int_for_order(la)),
                        Box::new(bool_to_int_for_order(lb)),
                    ),
                    CmpOp::Gt => SparExpr::Greater(
                        Box::new(bool_to_int_for_order(la)),
                        Box::new(bool_to_int_for_order(lb)),
                    ),
                    CmpOp::Ge => SparExpr::GreaterOrEqual(
                        Box::new(bool_to_int_for_order(la)),
                        Box::new(bool_to_int_for_order(lb)),
                    ),
                    CmpOp::In => SparExpr::In(Box::new(la), vec![lb]),
                    CmpOp::StartsWith | CmpOp::EndsWith | CmpOp::Contains | CmpOp::RegexMatch => {
                        return Err(PolygraphError::Unsupported {
                            construct: format!("string comparison op {op:?}"),
                            spec_ref: "openCypher 9 §6.2".into(),
                            reason: "use FunctionCall form".into(),
                        })
                    }
                })
            }

            Expr::IsNull(e) => {
                // For property access: `n.prop IS NULL` → NOT EXISTS { ?n <prop> ?_val }
                // This avoids adding a required BGP triple that would filter out
                // rows where the property is absent.
                if let Expr::Property(base, key) = e.as_ref() {
                    let base_expr = self.lower_expr(base)?;
                    if let SparExpr::Variable(base_var) = base_expr {
                        let val_var = self.fresh(key);
                        let exists_pat = GraphPattern::Bgp {
                            patterns: vec![TriplePattern {
                                subject: TermPattern::Variable(base_var),
                                predicate: NamedNodePattern::NamedNode(self.prop_iri(key)),
                                object: TermPattern::Variable(val_var),
                            }],
                        };
                        return Ok(SparExpr::Not(Box::new(SparExpr::Exists(Box::new(
                            exists_pat,
                        )))));
                    }
                }
                let inner = self.lower_expr(e)?;
                if let SparExpr::Variable(v) = &inner {
                    Ok(SparExpr::Not(Box::new(SparExpr::Bound(v.clone()))))
                } else {
                    // Complex expression: bind it to a fresh probe variable so that
                    // BOUND(?probe) correctly detects null/error propagation.
                    // When the expression errors (e.g. due to an UNDEF operand),
                    // BIND leaves the probe unbound, making !BOUND = true (IS NULL).
                    let probe = self.fresh("isnull_probe");
                    self.pending_binds.push((probe.clone(), inner));
                    Ok(SparExpr::Not(Box::new(SparExpr::Bound(probe))))
                }
            }
            Expr::IsNotNull(e) => {
                // For property access: `n.prop IS NOT NULL` → EXISTS { ?n <prop> ?_val }
                if let Expr::Property(base, key) = e.as_ref() {
                    let base_expr = self.lower_expr(base)?;
                    if let SparExpr::Variable(base_var) = base_expr {
                        let val_var = self.fresh(key);
                        let exists_pat = GraphPattern::Bgp {
                            patterns: vec![TriplePattern {
                                subject: TermPattern::Variable(base_var),
                                predicate: NamedNodePattern::NamedNode(self.prop_iri(key)),
                                object: TermPattern::Variable(val_var),
                            }],
                        };
                        return Ok(SparExpr::Exists(Box::new(exists_pat)));
                    }
                }
                let inner = self.lower_expr(e)?;
                if let SparExpr::Variable(v) = &inner {
                    Ok(SparExpr::Bound(v.clone()))
                } else {
                    // Complex expression: bind it to a fresh probe variable so that
                    // BOUND(?probe) correctly detects null/error propagation.
                    let probe = self.fresh("isnotnull_probe");
                    self.pending_binds.push((probe.clone(), inner));
                    Ok(SparExpr::Bound(probe))
                }
            }

            Expr::And(a, b) => Ok(SparExpr::And(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            Expr::Or(a, b) => Ok(SparExpr::Or(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            // XOR(a,b) = (a OR b) AND NOT (a AND b)
            Expr::Xor(a, b) => {
                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                Ok(SparExpr::And(
                    Box::new(SparExpr::Or(Box::new(la.clone()), Box::new(lb.clone()))),
                    Box::new(SparExpr::Not(Box::new(SparExpr::And(
                        Box::new(la),
                        Box::new(lb),
                    )))),
                ))
            }
            Expr::Not(e) => Ok(SparExpr::Not(Box::new(self.lower_expr(e)?))),

            Expr::LabelCheck { expr, labels } => {
                let base_inner = self.lower_expr(expr)?;
                let base_var = match base_inner {
                    SparExpr::Variable(v) => v,
                    _ => {
                        return Err(PolygraphError::Unsupported {
                            construct: "label check on non-variable".into(),
                            spec_ref: "openCypher 9 §6.3".into(),
                            reason: "LQA path only supports label check on variables".into(),
                        })
                    }
                };
                let var_name = base_var.as_str().to_owned();

                let mut result: Option<SparExpr> = None;
                for label in labels {
                    let label_tp = GraphPattern::Bgp {
                        patterns: vec![TriplePattern {
                            subject: TermPattern::Variable(base_var.clone()),
                            predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(
                                RDF_TYPE,
                            )),
                            object: TermPattern::NamedNode(self.label_iri(label)),
                        }],
                    };
                    let check = SparExpr::Exists(Box::new(label_tp));
                    result = Some(match result {
                        None => check,
                        Some(acc) => SparExpr::And(Box::new(acc), Box::new(check)),
                    });
                }
                let check_expr = result.unwrap_or(Self::lit_bool(true));

                // If the base variable is nullable (from OPTIONAL MATCH), wrap in
                // IF(BOUND(?var), check, ?_null) so that when the variable is null,
                // the label predicate returns null instead of an incorrect boolean.
                if self.nullable.contains(var_name.as_str()) {
                    let null_var = self.fresh("label_null");
                    Ok(SparExpr::If(
                        Box::new(SparExpr::Bound(base_var)),
                        Box::new(check_expr),
                        Box::new(SparExpr::Variable(null_var)),
                    ))
                } else {
                    Ok(check_expr)
                }
            }

            Expr::FunctionCall { name, args, .. } => self.lower_function_call(name, args),

            Expr::CaseSearched {
                branches,
                else_expr,
            } => {
                let else_sparql = match else_expr {
                    Some(e) => self.lower_expr(e)?,
                    None => {
                        let null_v = self.fresh("case_null");
                        SparExpr::Variable(null_v)
                    }
                };

                branches
                    .iter()
                    .rev()
                    .try_fold(else_sparql, |acc, (cond, then_)| {
                        let c = self.lower_expr(cond)?;
                        let t = self.lower_expr(then_)?;
                        Ok::<_, PolygraphError>(SparExpr::If(
                            Box::new(c),
                            Box::new(t),
                            Box::new(acc),
                        ))
                    })
            }

            Expr::Quantifier {
                kind,
                variable,
                list,
                predicate,
            } => {
                // Expand quantifiers over compile-time constant lists.
                // For runtime lists (properties, variables) fall back to legacy.
                let items = match list.as_ref() {
                    Expr::List(items) => items,
                    _ => {
                        return Err(PolygraphError::Unsupported {
                            construct: "Quantifier over non-constant list".into(),
                            spec_ref: "openCypher 9 §6.3.4".into(),
                            reason: "list must be a compile-time constant for LQA expansion; legacy fallback applies".into(),
                        })
                    }
                };

                if items.is_empty() {
                    // ALL(empty)  = true,  NONE(empty)   = true
                    // ANY(empty)  = false, SINGLE(empty) = false
                    return Ok(match kind {
                        QuantKind::All | QuantKind::None => Self::lit_bool(true),
                        QuantKind::Any | QuantKind::Single => Self::lit_bool(false),
                    });
                }

                // Type-check: detect non-numeric list elements used with arithmetic
                // predicates (e.g. `none(x IN ['str'] WHERE x % 2 = 0)`).
                // openCypher requires a compile-time SyntaxError (InvalidArgumentType)
                // for these patterns.
                let any_non_numeric = items.iter().any(|item| {
                    matches!(
                        item,
                        Expr::Literal(Literal::String(_)) | Expr::Literal(Literal::Boolean(_))
                    )
                });
                if any_non_numeric && quant_pred_uses_arithmetic(predicate, variable.as_str()) {
                    return Err(PolygraphError::Translation {
                        message: format!(
                            "Type mismatch: arithmetic predicate applied to non-numeric elements in {kind:?} quantifier over '{variable}'"
                        ),
                    });
                }

                // Evaluate pred[x←item] for each item.
                let preds = items
                    .iter()
                    .map(|item| {
                        let subst = subst_var(predicate, variable.as_str(), item);
                        self.lower_expr(&subst)
                    })
                    .collect::<Result<Vec<_>, _>>()?;

                match kind {
                    QuantKind::All => Ok(preds
                        .into_iter()
                        .reduce(|a, b| SparExpr::And(Box::new(a), Box::new(b)))
                        .unwrap()),
                    QuantKind::Any => Ok(preds
                        .into_iter()
                        .reduce(|a, b| SparExpr::Or(Box::new(a), Box::new(b)))
                        .unwrap()),
                    QuantKind::None => Ok(preds
                        .into_iter()
                        .map(|p| SparExpr::Not(Box::new(p)))
                        .reduce(|a, b| SparExpr::And(Box::new(a), Box::new(b)))
                        .unwrap()),
                    QuantKind::Single => {
                        // any_true AND NOT(two_or_more_true)
                        let any_true = preds
                            .iter()
                            .cloned()
                            .reduce(|a, b| SparExpr::Or(Box::new(a), Box::new(b)))
                            .unwrap();
                        if preds.len() == 1 {
                            return Ok(any_true);
                        }
                        let mut pair_exprs = Vec::new();
                        for i in 0..preds.len() {
                            for j in (i + 1)..preds.len() {
                                pair_exprs.push(SparExpr::And(
                                    Box::new(preds[i].clone()),
                                    Box::new(preds[j].clone()),
                                ));
                            }
                        }
                        let two_or_more = pair_exprs
                            .into_iter()
                            .reduce(|a, b| SparExpr::Or(Box::new(a), Box::new(b)))
                            .unwrap();
                        Ok(SparExpr::And(
                            Box::new(any_true),
                            Box::new(SparExpr::Not(Box::new(two_or_more))),
                        ))
                    }
                }
            }

            Expr::Exists(inner_op) => {
                // Do not handle variable-length path predicates in LQA — the VL path
                // semantics inside EXISTS (pattern predicates like `WHERE (n)-[:REL*2]-()`)
                // only work correctly via the legacy translator.
                if op_has_varlen(inner_op) {
                    return Err(PolygraphError::Unsupported {
                        construct: "expression type Exists in LQA SPARQL lowering".into(),
                        spec_ref: "openCypher 9 §6.3.4".into(),
                        reason: "EXISTS with variable-length path requires legacy path".into(),
                    });
                }
                // Evaluate an EXISTS { ... } subquery.  We need to isolate the inner
                // pending state so variable bindings and optional triples do not leak
                // into the outer WHERE clause.
                let saved_scan_vars = self.scan_vars.clone();
                let saved_nullable = self.nullable.clone();
                let saved_edge_vars = self.edge_vars.clone();
                let saved_anon_edge_info = std::mem::take(&mut self.anon_edge_info);
                let saved_pending_triples = std::mem::take(&mut self.pending_triples);
                let saved_opt_triples = std::mem::take(&mut self.pending_optional_triples);
                let saved_opt_groups = std::mem::take(&mut self.pending_optional_groups);
                let saved_opt_patterns = std::mem::take(&mut self.pending_optional_patterns);
                let saved_binds = std::mem::take(&mut self.pending_binds);

                let inner_gp = self.lower_op(inner_op)?;
                let exists_pat = self.flush_pending(inner_gp);

                // Restore outer scope
                self.scan_vars = saved_scan_vars;
                self.nullable = saved_nullable;
                self.edge_vars = saved_edge_vars;
                self.anon_edge_info = saved_anon_edge_info;
                self.pending_triples = saved_pending_triples;
                self.pending_optional_triples = saved_opt_triples;
                self.pending_optional_groups = saved_opt_groups;
                self.pending_optional_patterns = saved_opt_patterns;
                self.pending_binds = saved_binds;

                Ok(SparExpr::Exists(Box::new(exists_pat)))
            }

            Expr::List(_)
            | Expr::Map(_)
            | Expr::Subscript(_, _)
            | Expr::ListSlice { .. }
            | Expr::ListComprehension { .. }
            | Expr::PatternComprehension { .. }
            | Expr::Reduce { .. }
            | Expr::Aggregate { .. } => Err(PolygraphError::Unsupported {
                construct: format!(
                    "expression type {} in LQA SPARQL lowering",
                    expr_type_name(expr)
                ),
                spec_ref: "openCypher 9 §6".into(),
                reason:
                    "complex expression not yet fully handled in LQA path; legacy fallback applies"
                        .into(),
            }),

            Expr::Parameter(name) => Err(PolygraphError::Unsupported {
                construct: format!("parameter ${name}"),
                spec_ref: "openCypher 9 §4.1".into(),
                reason: "parameterized queries not yet supported in LQA path".into(),
            }),
        }
    }

    /// Lower `expr` to a SPARQL expression that produces a plain string value
    /// suitable for use as an element inside a CONCAT-based list/map serialization.
    /// Constant literals are serialized directly; everything else is wrapped in
    /// `COALESCE(STR(?x), "?")` to protect against unbound variables.
    fn lower_expr_as_concat_piece(
        &mut self,
        expr: &Expr,
    ) -> Result<SparExpr, PolygraphError> {
        // Fast path: constant literals.
        if let Some(s) = lqa_lit_elem_str(expr) {
            return Ok(Self::lit_str(&s));
        }
        // For nested list/map, recurse through lower_expr (which will produce
        // a string or CONCAT expression), then wrap in STR() to guarantee a string.
        match expr {
            Expr::List(_) | Expr::Map(_) => {
                let e = self.lower_expr(expr)?;
                return Ok(SparExpr::FunctionCall(
                    spargebra::algebra::Function::Str,
                    vec![e],
                ));
            }
            _ => {}
        }
        // Dynamic: lower to SPARQL and wrap in COALESCE(STR(...), "?").
        let e = self.lower_expr(expr)?;
        Ok(SparExpr::Coalesce(vec![
            SparExpr::FunctionCall(spargebra::algebra::Function::Str, vec![e]),
            Self::lit_str("?"),
        ]))
    }

    fn lower_function_call(
        &mut self,
        name: &str,
        args: &[Expr],
    ) -> Result<SparExpr, PolygraphError> {
        use spargebra::algebra::Function;

        let name_lower = name.to_lowercase();
        match name_lower.as_str() {
            "abs" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Abs, vec![a]))
            }
            "ceil" | "ceiling" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Ceil, vec![a]))
            }
            "floor" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Floor, vec![a]))
            }
            "round" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Round, vec![a]))
            }
            "sign" => {
                let arg = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let zero = Self::lit_integer(0);
                let one = Self::lit_integer(1);
                let m1 = Self::lit_integer(-1);
                Ok(SparExpr::If(
                    Box::new(SparExpr::Greater(
                        Box::new(arg.clone()),
                        Box::new(zero.clone()),
                    )),
                    Box::new(one),
                    Box::new(SparExpr::If(
                        Box::new(SparExpr::Less(Box::new(arg), Box::new(zero.clone()))),
                        Box::new(m1),
                        Box::new(zero),
                    )),
                ))
            }
            "tostring" | "string" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Str, vec![a]))
            }
            "tointeger" | "int" | "integer" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(
                    Function::Custom(NamedNode::new_unchecked(XSD_INTEGER)),
                    vec![a],
                ))
            }
            "todouble" | "tofloat" | "float" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(
                    Function::Custom(NamedNode::new_unchecked(XSD_DOUBLE)),
                    vec![a],
                ))
            }
            "toupper" | "touppercase" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::UCase, vec![a]))
            }
            "tolower" | "tolowercase" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::LCase, vec![a]))
            }
            "ltrim" | "rtrim" | "trim" => Err(PolygraphError::Unsupported {
                construct: format!("{name}()"),
                spec_ref: "openCypher 9 §6.3.2".into(),
                reason: "no direct SPARQL built-in; legacy path applies".into(),
            }),
            "strlen" | "size" => {
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                // Special case: if the arg is a compile-time constant list,
                // return the list length directly as an integer literal.
                match arg {
                    Expr::List(items) => {
                        return Ok(Self::lit_integer(items.len() as i64));
                    }
                    Expr::Map(pairs) => {
                        return Ok(Self::lit_integer(pairs.len() as i64));
                    }
                    _ => {}
                }
                let a = self.lower_expr(arg)?;
                Ok(SparExpr::FunctionCall(Function::StrLen, vec![a]))
            }
            "length" => {
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                // length() on a node or relationship variable must be rejected at
                // compile time (openCypher InvalidArgumentType).
                if let Expr::Variable { name: var_name, .. } = arg {
                    if self.scan_vars.contains(var_name.as_str()) {
                        return Err(PolygraphError::Translation {
                            message: format!(
                                "Type mismatch: expected Path or String, got Node/Relationship for length({})",
                                var_name
                            ),
                        });
                    }
                }
                let a = self.lower_expr(arg)?;
                Ok(SparExpr::FunctionCall(Function::StrLen, vec![a]))
            }
            "substring" | "substr" => {
                let a0 = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let raw_start = self.lower_expr(args.get(1).ok_or_else(|| arg_err(name))?)?;
                // Cypher substring() uses 0-based start index; SPARQL SUBSTR()
                // uses 1-based → add 1 to the start argument.
                let a1 = SparExpr::Add(Box::new(raw_start), Box::new(Self::lit_integer(1)));
                let mut sargs = vec![a0, a1];
                if let Some(a2) = args.get(2) {
                    sargs.push(self.lower_expr(a2)?);
                }
                Ok(SparExpr::FunctionCall(Function::SubStr, sargs))
            }
            "startswith" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let b = self.lower_expr(args.get(1).ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::StrStarts, vec![a, b]))
            }
            "endswith" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let b = self.lower_expr(args.get(1).ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::StrEnds, vec![a, b]))
            }
            "contains" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let b = self.lower_expr(args.get(1).ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Contains, vec![a, b]))
            }
            "regex" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                let b = self.lower_expr(args.get(1).ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Regex, vec![a, b]))
            }
            "type" => {
                if let Some(Expr::Variable { name: rv, .. }) = args.first() {
                    let rv = rv.as_str();
                    // Fast path: single static type known at compile time.
                    if let Some(types) = self.edge_types.get(rv).cloned() {
                        if types.len() == 1 {
                            // If the rel-var is nullable (from OPTIONAL MATCH), return
                            // IF(BOUND(?r_marker), "T", ?_null) so that when the OPTIONAL
                            // didn't match, type(r) returns null instead of the constant name.
                            if self.nullable.contains(rv) {
                                // Find the SPARQL variable that acts as the r-marker (the BIND
                                // inside the OPTIONAL that is bound if and only if r matched).
                                if let Some(edge_info) = self.edge_vars.get(rv).cloned() {
                                    if let EdgePred::Static(pred_iri) = &edge_info.pred {
                                        let _ = pred_iri; // no dynamic pred var to use
                                    }
                                }
                                // Use the sparql var ?rv directly as the NULL check:
                                // the BIND inside OPTIONAL sets ?rv; when OPTIONAL fails ?rv is unbound.
                                let rv_var = Self::var(rv);
                                let null_var = self.fresh("type_null");
                                return Ok(SparExpr::If(
                                    Box::new(SparExpr::Bound(rv_var)),
                                    Box::new(Self::lit_str(&types[0])),
                                    Box::new(SparExpr::Variable(null_var)),
                                ));
                            }
                            return Ok(Self::lit_str(&types[0]));
                        }
                    }
                    // Dynamic path: extract local name from the predicate variable.
                    if let Some(edge_info) = self.edge_vars.get(rv).cloned() {
                        if let EdgePred::Dynamic(pred_var) = &edge_info.pred {
                            // STRAFTER(STR(?pred_var), base_iri) extracts the local name.
                            let base_lit = SparExpr::Literal(SparLit::new_simple_literal(
                                self.base_iri.clone(),
                            ));
                            return Ok(SparExpr::FunctionCall(
                                Function::StrAfter,
                                vec![
                                    SparExpr::FunctionCall(
                                        Function::Str,
                                        vec![SparExpr::Variable(pred_var.clone())],
                                    ),
                                    base_lit,
                                ],
                            ));
                        }
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: "type(r) with unknown/multiple edge types".into(),
                    spec_ref: "openCypher 9 §6.3.2".into(),
                    reason: "multi-type or unbound relationship type requires legacy path".into(),
                })
            }
            "startnode" | "endnode" => {
                if let Some(Expr::Variable { name: rv, .. }) = args.first() {
                    if let Some(edge_info) = self.edge_vars.get(rv.as_str()).cloned() {
                        let node_var = if name_lower == "startnode" {
                            &edge_info.subj
                        } else {
                            &edge_info.obj
                        };
                        return Ok(SparExpr::Variable(Self::var(node_var)));
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: format!("{name}()"),
                    spec_ref: "openCypher 9 §6.3.2".into(),
                    reason: "startNode/endNode requires a known relationship variable".into(),
                })
            }
            "id" | "elementid" => {
                let a = self.lower_expr(args.first().ok_or_else(|| arg_err(name))?)?;
                Ok(SparExpr::FunctionCall(Function::Str, vec![a]))
            }
            "coalesce" => {
                // Property-access triples generated inside coalesce() arguments
                // must be OPTIONAL in SPARQL — the whole point of coalesce is
                // to handle absent/null properties gracefully.
                let largs = args
                    .iter()
                    .map(|a| {
                        let before = self.pending_triples.len();
                        let expr = self.lower_expr(a)?;
                        // Promote any new required triples to optional triples.
                        let new_triples: Vec<_> = self.pending_triples.drain(before..).collect();
                        self.pending_optional_triples.extend(new_triples);
                        Ok(expr)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(SparExpr::Coalesce(largs))
            }
            "head" | "first" => {
                // Constant folding: head over a compile-time constant list.
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                match arg {
                    Expr::List(items) => {
                        if items.is_empty() {
                            Ok(SparExpr::Variable(self.fresh("_null_head")))
                        } else {
                            self.lower_expr(&items[0])
                        }
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "head()".into(),
                        spec_ref: "openCypher 9 §3.4.2".into(),
                        reason: "head() over runtime list requires legacy path".into(),
                    }),
                }
            }
            "last" => {
                // Constant folding: last over a compile-time constant list.
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                match arg {
                    Expr::List(items) => {
                        if items.is_empty() {
                            Ok(SparExpr::Variable(self.fresh("_null_last")))
                        } else {
                            self.lower_expr(items.last().unwrap())
                        }
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "last()".into(),
                        spec_ref: "openCypher 9 §3.4.2".into(),
                        reason: "last() over runtime list requires legacy path".into(),
                    }),
                }
            }
            _ => Err(PolygraphError::Unsupported {
                construct: format!("{name}()"),
                spec_ref: "openCypher 9 §6.3".into(),
                reason: format!("function '{name}' not yet in LQA path; legacy fallback applies"),
            }),
        }
    }

    fn lower_order_key(&mut self, sk: &SortKey) -> Result<OrderExpression, PolygraphError> {
        self.lower_order_key_expr(&sk.expr, sk.dir.clone())
    }

    fn lower_order_key_expr(
        &mut self,
        expr: &Expr,
        dir: SortDir,
    ) -> Result<OrderExpression, PolygraphError> {
        let sparql_expr = self.lower_expr(expr)?;
        Ok(match dir {
            SortDir::Asc => OrderExpression::Asc(sparql_expr),
            SortDir::Desc => OrderExpression::Desc(sparql_expr),
        })
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Returns `true` if `var` appears as a direct operand to an arithmetic
/// operator (`%`, `/`, `*`, `-`, `^`) anywhere in `expr`.
/// Used to detect type-mismatch quantifiers at compile time.
fn quant_pred_uses_arithmetic(expr: &Expr, var: &str) -> bool {
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Mod(a, b) | E::Div(a, b) | E::Mul(a, b) | E::Sub(a, b) | E::Pow(a, b) => {
            expr_contains_var(a, var) || expr_contains_var(b, var)
                || quant_pred_uses_arithmetic(a, var) || quant_pred_uses_arithmetic(b, var)
        }
        E::Unary(UnaryOp::Neg, a) => expr_contains_var(a, var) || quant_pred_uses_arithmetic(a, var),
        E::And(a, b) | E::Or(a, b) | E::Xor(a, b) => {
            quant_pred_uses_arithmetic(a, var) || quant_pred_uses_arithmetic(b, var)
        }
        E::Not(a) | E::IsNull(a) | E::IsNotNull(a) => quant_pred_uses_arithmetic(a, var),
        E::Comparison(_, a, b) => {
            quant_pred_uses_arithmetic(a, var) || quant_pred_uses_arithmetic(b, var)
        }
        _ => false,
    }
}

fn expr_contains_var(expr: &Expr, var: &str) -> bool {
    matches!(expr, Expr::Variable { name, .. } if name.as_str() == var)
}

/// Convert LQA map pairs (with only literal values, at the LQA IR level) into
/// AST `(String, Expression)` pairs so that the pub(crate) temporal helpers in
/// `translator::cypher` can be called from the LQA path.
///
/// Pairs whose value is not a ground literal are omitted (the helper functions
/// gracefully return `None` when required keys are absent).
fn lqa_map_to_ast_pairs(pairs: &[(String, Expr)]) -> Vec<(String, crate::ast::cypher::Expression)> {
    use crate::ast::cypher::{Expression as AE, Literal as AL};
    pairs
        .iter()
        .filter_map(|(k, v)| {
            let ae = match v {
                Expr::Literal(Literal::Integer(n)) => AE::Literal(AL::Integer(*n)),
                Expr::Literal(Literal::Float(f)) => AE::Literal(AL::Float(*f)),
                Expr::Literal(Literal::String(s)) => AE::Literal(AL::String(s.clone())),
                Expr::Literal(Literal::Boolean(b)) => AE::Literal(AL::Boolean(*b)),
                Expr::Literal(Literal::Null) => AE::Literal(AL::Null),
                _ => return None,
            };
            Some((k.clone(), ae))
        })
        .collect()
}

/// Recursively substitute a single variable name with a replacement expression
/// in an LQA `Expr` tree.  Used for quantifier expansion.
fn subst_var(expr: &Expr, var: &str, val: &Expr) -> Expr {
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Variable { name, .. } if name == var => val.clone(),
        E::Add(a, b) => E::Add(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Sub(a, b) => E::Sub(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Mul(a, b) => E::Mul(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Div(a, b) => E::Div(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Mod(a, b) => E::Mod(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Pow(a, b) => E::Pow(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Unary(op, a) => E::Unary(op.clone(), Box::new(subst_var(a, var, val))),
        E::Comparison(op, a, b) => E::Comparison(
            op.clone(),
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::IsNull(a) => E::IsNull(Box::new(subst_var(a, var, val))),
        E::IsNotNull(a) => E::IsNotNull(Box::new(subst_var(a, var, val))),
        E::And(a, b) => E::And(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Or(a, b) => E::Or(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Xor(a, b) => E::Xor(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::Not(a) => E::Not(Box::new(subst_var(a, var, val))),
        E::Property(base, key) => E::Property(Box::new(subst_var(base, var, val)), key.clone()),
        E::Subscript(a, b) => E::Subscript(
            Box::new(subst_var(a, var, val)),
            Box::new(subst_var(b, var, val)),
        ),
        E::ListSlice { list, start, end } => E::ListSlice {
            list: Box::new(subst_var(list, var, val)),
            start: start.as_deref().map(|e| Box::new(subst_var(e, var, val))),
            end: end.as_deref().map(|e| Box::new(subst_var(e, var, val))),
        },
        E::List(items) => E::List(items.iter().map(|e| subst_var(e, var, val)).collect()),
        E::Map(pairs) => E::Map(
            pairs
                .iter()
                .map(|(k, e)| (k.clone(), subst_var(e, var, val)))
                .collect(),
        ),
        E::FunctionCall {
            name,
            distinct,
            args,
        } => E::FunctionCall {
            name: name.clone(),
            distinct: *distinct,
            args: args.iter().map(|a| subst_var(a, var, val)).collect(),
        },
        E::CaseSearched {
            branches,
            else_expr,
        } => E::CaseSearched {
            branches: branches
                .iter()
                .map(|(c, t)| (subst_var(c, var, val), subst_var(t, var, val)))
                .collect(),
            else_expr: else_expr
                .as_deref()
                .map(|e| Box::new(subst_var(e, var, val))),
        },
        E::LabelCheck {
            expr: inner,
            labels,
        } => E::LabelCheck {
            expr: Box::new(subst_var(inner, var, val)),
            labels: labels.clone(),
        },
        // For other complex variants (Aggregate, Quantifier, comprehensions, etc.)
        // that are unlikely to appear inside a quantifier predicate, leave as-is.
        _ => expr.clone(),
    }
}

/// Structural equality check for LQA expressions (ignores type annotations).
/// Used to match ORDER BY sort keys to GROUP BY projection item expressions.
/// Recursively substitute alias references inside a Cypher expression.
/// Used to expand sort-key (ORDER BY) expressions that contain RETURN/WITH alias
/// variable names, replacing each alias with its underlying expression before
/// lowering to SPARQL. This handles cases like `ORDER BY n + 2` where `n` is a
/// RETURN alias for `n.num`.
fn subst_aliases_in_expr(expr: &Expr, alias_map: &[(&str, &Expr)]) -> Expr {
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Variable { name, .. } => {
            if let Some((_, underlying)) = alias_map.iter().find(|(a, _)| *a == name.as_str()) {
                (*underlying).clone()
            } else {
                expr.clone()
            }
        }
        E::Add(a, b) => E::Add(
            Box::new(subst_aliases_in_expr(a, alias_map)),
            Box::new(subst_aliases_in_expr(b, alias_map)),
        ),
        E::Sub(a, b) => E::Sub(
            Box::new(subst_aliases_in_expr(a, alias_map)),
            Box::new(subst_aliases_in_expr(b, alias_map)),
        ),
        E::Mul(a, b) => E::Mul(
            Box::new(subst_aliases_in_expr(a, alias_map)),
            Box::new(subst_aliases_in_expr(b, alias_map)),
        ),
        E::Div(a, b) => E::Div(
            Box::new(subst_aliases_in_expr(a, alias_map)),
            Box::new(subst_aliases_in_expr(b, alias_map)),
        ),
        E::FunctionCall {
            name,
            distinct,
            args,
        } => E::FunctionCall {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|a| subst_aliases_in_expr(a, alias_map))
                .collect(),
        },
        _ => expr.clone(),
    }
}

/// Reverse alias substitution: replace sub-expressions that match an alias's
/// underlying expression with a Variable reference to that alias.
/// Used in mid-pipeline ORDER BY after a GroupBy/DISTINCT scope boundary where
/// the original expressions (like `a.name`) are no longer accessible but the
/// alias variables (like `name`) are. E.g. `a.name + 'C'` → `name + 'C'`.
fn subst_exprs_with_aliases(expr: &Expr, alias_map: &[(&str, &Expr)]) -> Expr {
    use crate::lqa::expr::Expr as E;
    // Check if the whole expression matches any alias's underlying expression.
    if let Some((alias_name, _)) = alias_map.iter().find(|(_, e)| exprs_equivalent(expr, e)) {
        return E::Variable {
            name: (*alias_name).to_string(),
            ty: None,
        };
    }
    // Recursively substitute sub-expressions.
    match expr {
        E::Add(a, b) => E::Add(
            Box::new(subst_exprs_with_aliases(a, alias_map)),
            Box::new(subst_exprs_with_aliases(b, alias_map)),
        ),
        E::Sub(a, b) => E::Sub(
            Box::new(subst_exprs_with_aliases(a, alias_map)),
            Box::new(subst_exprs_with_aliases(b, alias_map)),
        ),
        E::Mul(a, b) => E::Mul(
            Box::new(subst_exprs_with_aliases(a, alias_map)),
            Box::new(subst_exprs_with_aliases(b, alias_map)),
        ),
        E::Div(a, b) => E::Div(
            Box::new(subst_exprs_with_aliases(a, alias_map)),
            Box::new(subst_exprs_with_aliases(b, alias_map)),
        ),
        E::FunctionCall {
            name,
            distinct,
            args,
        } => E::FunctionCall {
            name: name.clone(),
            distinct: *distinct,
            args: args
                .iter()
                .map(|a| subst_exprs_with_aliases(a, alias_map))
                .collect(),
        },
        _ => expr.clone(),
    }
}

fn exprs_equivalent(a: &Expr, b: &Expr) -> bool {
    use crate::lqa::expr::Expr as E;
    match (a, b) {
        (E::Variable { name: na, .. }, E::Variable { name: nb, .. }) => na == nb,
        (E::Property(ba, ka), E::Property(bb, kb)) => ka == kb && exprs_equivalent(ba, bb),
        (E::Add(la, ra), E::Add(lb, rb)) => exprs_equivalent(la, lb) && exprs_equivalent(ra, rb),
        (E::Literal(la), E::Literal(lb)) => la == lb,
        _ => false,
    }
}

fn join(left: GraphPattern, right: GraphPattern) -> GraphPattern {
    match (&left, &right) {
        (GraphPattern::Bgp { patterns: lp }, _) if lp.is_empty() => right,
        (_, GraphPattern::Bgp { patterns: rp }) if rp.is_empty() => left,
        _ => GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        },
    }
}

fn expr_to_usize(expr: &Expr) -> Result<usize, PolygraphError> {
    match expr {
        Expr::Literal(Literal::Integer(n)) if *n >= 0 => Ok(*n as usize),
        // Evaluate simple compile-time arithmetic for constant SKIP/LIMIT expressions
        // like `LIMIT 1 + 1` or `SKIP 3 * 2`.
        Expr::Add(a, b) => Ok(expr_to_usize(a)? + expr_to_usize(b)?),
        Expr::Sub(a, b) => {
            let av = expr_to_usize(a)?;
            let bv = expr_to_usize(b)?;
            Ok(av.saturating_sub(bv))
        }
        Expr::Mul(a, b) => Ok(expr_to_usize(a)? * expr_to_usize(b)?),
        // Handle toInteger(x) where x can be evaluated as a float.
        // Also handles non-evaluatable expressions (e.g. rand()) by returning 0
        // (SKIP/LIMIT 0 = no-op), since SPARQL OFFSET/LIMIT only take integer literals.
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("toInteger") && args.len() == 1 =>
        {
            Ok(expr_to_f64(&args[0])
                .map(|f| if f >= 0.0 { f as usize } else { 0 })
                .unwrap_or(0))
        }
        _ => Err(PolygraphError::Translation {
            message: format!("SKIP/LIMIT requires a non-negative integer literal, got {expr:?}"),
        }),
    }
}

/// Evaluate a Cypher expression as a compile-time floating-point constant.
/// This is used for constant-folding in SKIP/LIMIT positions.
fn expr_to_f64(expr: &Expr) -> Result<f64, PolygraphError> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => Ok(*n as f64),
        Expr::Literal(Literal::Float(f)) => Ok(*f as f64),
        Expr::Add(a, b) => Ok(expr_to_f64(a)? + expr_to_f64(b)?),
        Expr::Sub(a, b) => Ok(expr_to_f64(a)? - expr_to_f64(b)?),
        Expr::Mul(a, b) => Ok(expr_to_f64(a)? * expr_to_f64(b)?),
        Expr::Div(a, b) => {
            let bv = expr_to_f64(b)?;
            if bv == 0.0 {
                return Err(PolygraphError::Translation {
                    message: "Division by zero in SKIP/LIMIT expression".into(),
                });
            }
            Ok(expr_to_f64(a)? / bv)
        }
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("ceil") && args.len() == 1 =>
        {
            Ok(expr_to_f64(&args[0])?.ceil())
        }
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("floor") && args.len() == 1 =>
        {
            Ok(expr_to_f64(&args[0])?.floor())
        }
        Expr::FunctionCall { name, args, .. }
            if name.eq_ignore_ascii_case("round") && args.len() == 1 =>
        {
            Ok(expr_to_f64(&args[0])?.round())
        }
        _ => Err(PolygraphError::Translation {
            message: format!("Cannot evaluate float expression at compile time: {expr:?}"),
        }),
    }
}

fn query_slice_start(pat: &GraphPattern) -> usize {
    if let GraphPattern::Slice { start, .. } = pat {
        *start
    } else {
        0
    }
}

fn literal_to_ground(expr: &Expr) -> Result<Option<spargebra::term::GroundTerm>, PolygraphError> {
    match expr {
        Expr::Literal(Literal::Integer(n)) => Ok(Some(spargebra::term::GroundTerm::Literal(
            SparLit::new_typed_literal(n.to_string(), NamedNode::new_unchecked(XSD_INTEGER)),
        ))),
        Expr::Literal(Literal::Float(f)) => Ok(Some(spargebra::term::GroundTerm::Literal(
            SparLit::new_typed_literal(format!("{f:?}"), NamedNode::new_unchecked(XSD_DOUBLE)),
        ))),
        Expr::Literal(Literal::String(s)) => Ok(Some(spargebra::term::GroundTerm::Literal(
            SparLit::new_simple_literal(s.as_str()),
        ))),
        Expr::Literal(Literal::Boolean(b)) => Ok(Some(spargebra::term::GroundTerm::Literal(
            SparLit::new_typed_literal(b.to_string(), NamedNode::new_unchecked(XSD_BOOLEAN)),
        ))),
        Expr::Literal(Literal::Null) => Ok(None),
        Expr::Literal(Literal::TypedLiteral(v, t)) => {
            if t.is_empty() {
                Ok(Some(spargebra::term::GroundTerm::Literal(
                    SparLit::new_simple_literal(v.as_str()),
                )))
            } else {
                Ok(Some(spargebra::term::GroundTerm::Literal(
                    SparLit::new_typed_literal(
                        v.as_str(),
                        NamedNode::new_unchecked(t.as_str()),
                    ),
                )))
            }
        }
        _ => Err(PolygraphError::Unsupported {
            construct: format!("non-literal value ({}) in UNWIND/VALUES context", expr_type_name(expr)),
            spec_ref: "openCypher 9 §4.5".into(),
            reason: "UNWIND/VALUES only supports scalar literal values in LQA path; nested lists and computed expressions require legacy path".into(),
        }),
    }
}

/// Returns `true` if `op` or any of its sub-ops is an `Expand` with a
/// variable-length path (`range: Some(_)`).  Used to guard the EXISTS handler:
/// VL-path predicates like `WHERE (n)-[:REL*2]-()` need the legacy path.
fn op_has_varlen(op: &Op) -> bool {
    match op {
        Op::Expand { inner, range, .. } => range.is_some() || op_has_varlen(inner),
        Op::Scan { .. } | Op::Unit | Op::Values { .. } => false,
        Op::Selection { inner, .. }
        | Op::Projection { inner, .. }
        | Op::Unwind { inner, .. }
        | Op::Limit { inner, .. }
        | Op::OrderBy { inner, .. }
        | Op::Skip { inner, .. }
        | Op::GroupBy { inner, .. }
        | Op::Distinct { inner }
        | Op::Create { inner, .. }
        | Op::Set { inner, .. }
        | Op::Remove { inner, .. }
        | Op::Delete { inner, .. }
        | Op::Merge { inner, .. }
        | Op::Subquery { inner, .. }
        | Op::Foreach { inner, .. }
        | Op::Call { inner, .. } => op_has_varlen(inner),
        Op::LeftOuterJoin { left, right, .. }
        | Op::CartesianProduct { left, right }
        | Op::Union { left, right }
        | Op::UnionAll { left, right } => op_has_varlen(left) || op_has_varlen(right),
    }
}

fn expr_type_name(expr: &Expr) -> &'static str {
    match expr {
        Expr::Variable { .. } => "Variable",
        Expr::Literal(_) => "Literal",
        Expr::Property(_, _) => "Property",
        Expr::Add(_, _) => "Add",
        Expr::Sub(_, _) => "Sub",
        Expr::Mul(_, _) => "Mul",
        Expr::Div(_, _) => "Div",
        Expr::Mod(_, _) => "Mod",
        Expr::Pow(_, _) => "Pow",
        Expr::Unary(_, _) => "Unary",
        Expr::Comparison(_, _, _) => "Comparison",
        Expr::IsNull(_) => "IsNull",
        Expr::IsNotNull(_) => "IsNotNull",
        Expr::And(_, _) => "And",
        Expr::Or(_, _) => "Or",
        Expr::Not(_) => "Not",
        Expr::LabelCheck { .. } => "LabelCheck",
        Expr::FunctionCall { .. } => "FunctionCall",
        Expr::Aggregate { .. } => "Aggregate",
        Expr::CaseSearched { .. } => "CaseSearched",
        Expr::List(_) => "List",
        Expr::Map(_) => "Map",
        Expr::Subscript(_, _) => "Subscript",
        Expr::ListSlice { .. } => "ListSlice",
        Expr::Quantifier { .. } => "Quantifier",
        Expr::ListComprehension { .. } => "ListComprehension",
        Expr::PatternComprehension { .. } => "PatternComprehension",
        Expr::Reduce { .. } => "Reduce",
        Expr::Exists(_) => "Exists",
        Expr::Parameter(_) => "Parameter",
        Expr::Xor(_, _) => "Xor",
    }
}

/// Returns `true` if `e` is guaranteed to produce a string value.
///
/// Used by the `+` operator handler to decide between SPARQL arithmetic `+`
/// and `CONCAT()`.  A conservative check: only literal strings and
/// string-producing function calls / Add-chains are detected; property
/// accesses are treated as unknown (numeric `+` will be attempted, and callers
/// relying on string concat must include at least one literal string argument).
fn lqa_expr_is_string(e: &Expr) -> bool {
    match e {
        Expr::Literal(lit) => matches!(lit, Literal::String(_)),
        Expr::Add(a, b) => lqa_expr_is_string(a) || lqa_expr_is_string(b),
        Expr::FunctionCall { name, .. } => matches!(
            name.as_str(),
            "toString"
                | "toLower"
                | "toUpper"
                | "trim"
                | "ltrim"
                | "rtrim"
                | "replace"
                | "substring"
                | "left"
                | "right"
                | "reverse"
                | "split"
                | "tostring"
        ),
        _ => false,
    }
}

/// Serialize a constant LQA literal expression to its Cypher string representation
/// (as used inside a list or map serialization). Returns `None` for non-constants.
/// Try to extract a Cypher temporal/duration property from a known scalar literal string.
/// Returns the SPARQL integer literal for the component, or `None` if not recognized.
fn lqa_scalar_temporal_prop(val: &str, component: &str) -> Option<SparExpr> {
    use crate::translator::cypher as tc;

    // Duration property: val starts with 'P' or '-P'
    if val.starts_with('P') || val.starts_with("-P") {
        let n = tc::duration_get_component(val, component)?;
        let parsed: i64 = n.parse().ok()?;
        return Some(SparExpr::Literal(SparLit::new_typed_literal(
            parsed.to_string(),
            NamedNode::new_unchecked(XSD_INTEGER),
        )));
    }

    // Temporal (date / time / datetime) property.
    // Use tc_from_iso_string to parse the temporal value.
    let comps = tc::tc_from_iso_string(val)?;
    let n: i64 = match component {
        "year" => comps.year?,
        "month" => comps.month?,
        "day" => comps.day?,
        "hour" => comps.hour?,
        "minute" => comps.minute?,
        "second" => comps.second?,
        "millisecond" | "millisecondOfSecond" | "millisecondsOfSecond" => {
            (comps.ns? / 1_000_000) as i64
        }
        "microsecond" | "microsecondOfSecond" | "microsecondsOfSecond" => {
            (comps.ns? / 1_000) as i64
        }
        "nanosecond" | "nanosecondOfSecond" | "nanosecondsOfSecond" => comps.ns? as i64,
        "week" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let (_, w, _) = crate::translator::cypher::date_to_iso_week(y, m, d);
            w
        }
        "dayOfWeek" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let (_, _, dow) = crate::translator::cypher::date_to_iso_week(y, m, d);
            dow
        }
        "dayOfYear" | "ordinalDay" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let epoch_d = crate::translator::cypher::temporal_epoch(y, m, d);
            let epoch_y = crate::translator::cypher::temporal_epoch(y, 1, 1);
            (epoch_d - epoch_y + 1) as i64
        }
        "quarter" => {
            let m = comps.month?;
            (m - 1) / 3 + 1
        }
        "dayOfQuarter" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let q_start_m = ((m - 1) / 3) * 3 + 1;
            let mut doq = d;
            for mo in q_start_m..m {
                doq += crate::translator::cypher::temporal_dim(y, mo);
            }
            doq
        }
        "weekYear" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let (iso_year, _, _) = crate::translator::cypher::date_to_iso_week(y, m, d);
            iso_year
        }
        "offset" | "timezone" | "epochMillis" | "epochSeconds" => return None,
        _ => return None,
    };
    Some(SparExpr::Literal(SparLit::new_typed_literal(
        n.to_string(),
        NamedNode::new_unchecked(XSD_INTEGER),
    )))
}

fn lqa_lit_elem_str(e: &Expr) -> Option<String> {
    match e {
        Expr::Literal(lit) => match lit {
            Literal::Integer(n) => Some(n.to_string()),
            Literal::Float(f) => {
                // Mirror Cypher float formatting: avoid trailing zeros but keep decimal.
                let s = format!("{f}");
                Some(s)
            }
            Literal::String(s) => Some(format!("'{s}'")),
            Literal::Boolean(b) => Some(if *b { "true" } else { "false" }.to_owned()),
            Literal::Null => Some("null".to_owned()),
            Literal::TypedLiteral(v, _) => Some(v.clone()),
        },
        Expr::Unary(crate::lqa::expr::UnaryOp::Neg, inner) => match inner.as_ref() {
            Expr::Literal(Literal::Integer(n)) => Some(format!("-{n}")),
            Expr::Literal(Literal::Float(f)) => Some(format!("{}", -f)),
            _ => None,
        },
        _ => None,
    }
}

fn arg_err(name: &str) -> PolygraphError {
    PolygraphError::UnsupportedFeature {
        feature: format!("{name}() requires an argument"),
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Compile an [`Op`] tree to a SPARQL SELECT query string + projection schema.
///
/// Returns [`PolygraphError::Unsupported`] for constructs not yet handled in
/// the LQA path.  The caller should fall back to the legacy translator.
pub fn compile(op: &Op, base_iri: Option<&str>) -> Result<CompiledQuery, PolygraphError> {
    let base = base_iri.unwrap_or(DEFAULT_BASE).to_string();
    let mut c = Compiler::new(base.clone());
    c.compile_inner(op, &base)
}

/// Wrap a SPARQL expression so that `xsd:boolean` values (false=0, true=1) sort
/// correctly with ordered comparison operators (`<`, `<=`, `>`, `>=`).
///
/// Without this wrapper, serializing `(!false) >= false` produces the SPARQL text
/// `! "false"^^<bool> >= "false"^^<bool>`, which SPARQL parsers interpret as
/// `! ("false"^^<bool> >= "false"^^<bool>)` due to operator precedence — the
/// opposite of the intended semantics.  The `IF` wrapper forces the entire
/// operand into a primary-expression context, adding implicit parens:
///
///   IF(isLiteral(e) && datatype(e) = xsd:boolean,
///      xsd:integer(e),          -- false→0, true→1
///      e)
///
/// This is the same technique used by the legacy translator.
fn bool_to_int_for_order(e: SparExpr) -> SparExpr {
    let cond = SparExpr::And(
        Box::new(SparExpr::FunctionCall(
            spargebra::algebra::Function::IsLiteral,
            vec![e.clone()],
        )),
        Box::new(SparExpr::Equal(
            Box::new(SparExpr::FunctionCall(
                spargebra::algebra::Function::Datatype,
                vec![e.clone()],
            )),
            Box::new(SparExpr::NamedNode(NamedNode::new_unchecked(XSD_BOOLEAN))),
        )),
    );
    let cast_to_int = SparExpr::FunctionCall(
        spargebra::algebra::Function::Custom(NamedNode::new_unchecked(XSD_INTEGER)),
        vec![e.clone()],
    );
    SparExpr::If(Box::new(cond), Box::new(cast_to_int), Box::new(e))
}

/// Attempt to constant-fold a binary numeric operation when both operands are
/// SPARQL numeric literals.  Returns `Some(folded)` on success, `None` when
/// the operands are not both numeric literals or when the result is undefined
/// (e.g. division by zero).
///
/// * Integer + integer  → integer (using Rust's overflow-safe arithmetic)
/// * Integer / integer  → integer with truncation toward zero (Cypher semantics),
///   avoids the SPARQL `xsd:decimal` result for integer division
/// * Mixed int/double   → double
/// * `op` is one of `'+'`, `'-'`, `'*'`, `'/'`, `'%'`
fn fold_numeric_binop(op: char, la: &SparExpr, rb: &SparExpr) -> Option<SparExpr> {
    let la_lit = if let SparExpr::Literal(l) = la {
        l
    } else {
        return None;
    };
    let rb_lit = if let SparExpr::Literal(l) = rb {
        l
    } else {
        return None;
    };

    let la_dt = la_lit.datatype().as_str();
    let rb_dt = rb_lit.datatype().as_str();

    // Both integer literals → integer result (avoid xsd:decimal drift).
    if la_dt == XSD_INTEGER && rb_dt == XSD_INTEGER {
        let lv: i64 = la_lit.value().parse().ok()?;
        let rv: i64 = rb_lit.value().parse().ok()?;
        let result = match op {
            '+' => lv.checked_add(rv)?,
            '-' => lv.checked_sub(rv)?,
            '*' => lv.checked_mul(rv)?,
            '/' => {
                if rv == 0 {
                    return None; // division by zero → propagate as SPARQL undef
                }
                // Rust integer division truncates toward zero, matching Cypher semantics.
                lv / rv
            }
            '%' => {
                if rv == 0 {
                    return None;
                }
                lv % rv
            }
            _ => return None,
        };
        return Some(SparExpr::Literal(SparLit::new_typed_literal(
            result.to_string(),
            NamedNode::new_unchecked(XSD_INTEGER),
        )));
    }

    // Mixed or double literals → double result.
    let parse_num = |l: &SparLit| -> Option<f64> {
        let dt = l.datatype().as_str();
        if dt == XSD_INTEGER || dt == XSD_DOUBLE || dt == "http://www.w3.org/2001/XMLSchema#decimal"
        {
            l.value().parse().ok()
        } else {
            None
        }
    };

    let lv = parse_num(la_lit)?;
    let rv = parse_num(rb_lit)?;
    let result = match op {
        '+' => lv + rv,
        '-' => lv - rv,
        '*' => lv * rv,
        '/' => {
            if rv == 0.0 {
                return None;
            }
            lv / rv
        }
        '%' => {
            if rv == 0.0 {
                return None;
            }
            lv % rv
        }
        _ => return None,
    };
    if result.is_finite() {
        Some(SparExpr::Literal(SparLit::new_typed_literal(
            format!("{result:?}"),
            NamedNode::new_unchecked(XSD_DOUBLE),
        )))
    } else {
        None
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lqa::lower::AstLowerer;
    use crate::parser::parse_cypher;

    fn compile_query(src: &str) -> String {
        let ast = parse_cypher(src).expect("parse");
        let mut l = AstLowerer::new();
        let op = l.lower_query(&ast).expect("lower");
        let result = compile(&op, None).expect("compile");
        result.sparql
    }

    #[test]
    fn simple_match_return() {
        let sparql = compile_query("MATCH (n:Person) RETURN n");
        assert!(sparql.contains("SELECT"), "expected SELECT, got: {sparql}");
    }

    #[test]
    fn where_clause() {
        let sparql = compile_query("MATCH (n:Person) WHERE n.age > 30 RETURN n.name");
        let upper = sparql.to_uppercase();
        assert!(upper.contains("FILTER"), "expected FILTER, got: {sparql}");
    }

    #[test]
    fn relationship_match() {
        let sparql = compile_query("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name");
        assert!(sparql.contains("SELECT"));
    }

    #[test]
    fn order_limit() {
        let sparql = compile_query("MATCH (n:Person) RETURN n LIMIT 10");
        assert!(sparql.contains("10"), "expected limit 10, got: {sparql}");
    }
}
