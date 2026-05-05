//! Phase 4.5 — AST → LQA lowering pass.
//!
//! Converts a [`CypherQuery`] AST into an [`Op`] tree that can subsequently
//! be compiled to SPARQL by [`crate::lqa::sparql`].
//!
//! The conversion is structurally straightforward — every AST node maps to
//! one or more LQA nodes — so this module has no SPARQL-specific knowledge.
//!
//! # Entry point
//!
//! ```ignore
//! let mut lowerer = AstLowerer::new();
//! let op = lowerer.lower_query(&cypher_query)?;
//! ```

use std::collections::HashSet;

use crate::ast::cypher::{
    self as ast, AggregateExpr, Clause, CompOp, Direction, PatternElement, QuantifierKind,
};
use crate::error::PolygraphError;
use crate::lqa::expr::{AggKind, CmpOp, Expr, Literal, QuantKind, UnaryOp};
use crate::lqa::op::{
    AggItem, CreateEdge, CreateNode, Direction as LqaDir, MergeClause as LqaMergeClause, Op,
    PathRange, ProjItem, RemoveItem as LqaRemoveItem, SetItem as LqaSetItem, SortKey,
};

// ── Entry point ───────────────────────────────────────────────────────────────

/// Lowers a parsed [`ast::CypherQuery`] into an LQA [`Op`] tree.
pub struct AstLowerer {
    counter: u32,
    /// Variables introduced by earlier MATCH patterns in the same query.
    /// Re-used variables (those seen before) are not re-scanned; they are
    /// already bound in the SPARQL context via shared variable names.
    seen_vars: HashSet<String>,
}

impl AstLowerer {
    pub fn new() -> Self {
        Self {
            counter: 0,
            seen_vars: HashSet::new(),
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let c = self.counter;
        self.counter += 1;
        format!("_lqa_{prefix}_{c}")
    }

    /// Convert the whole query.  Returns the root [`Op`].
    pub fn lower_query(&mut self, query: &ast::CypherQuery) -> Result<Op, PolygraphError> {
        self.lower_clauses(&query.clauses)
    }

    // ── Clause list ───────────────────────────────────────────────────────────

    /// Split on UNION markers, lower each arm, combine.
    fn lower_clauses(&mut self, clauses: &[Clause]) -> Result<Op, PolygraphError> {
        // Find UNION positions
        let mut cut_positions: Vec<(usize, bool)> = Vec::new(); // (index, all)
        for (i, c) in clauses.iter().enumerate() {
            if let Clause::Union { all } = c {
                cut_positions.push((i, *all));
            }
        }

        if cut_positions.is_empty() {
            return self.lower_pipeline(clauses);
        }

        // Split into arms
        let mut arms: Vec<&[Clause]> = Vec::new();
        let mut all_flags: Vec<bool> = Vec::new();
        let mut prev = 0;
        for (pos, all) in &cut_positions {
            arms.push(&clauses[prev..*pos]);
            all_flags.push(*all);
            prev = pos + 1;
        }
        arms.push(&clauses[prev..]);

        let mut result = self.lower_pipeline(arms[0])?;
        for (i, arm) in arms[1..].iter().enumerate() {
            let right = self.lower_pipeline(arm)?;
            result = if all_flags[i] {
                Op::UnionAll {
                    left: Box::new(result),
                    right: Box::new(right),
                }
            } else {
                Op::Union {
                    left: Box::new(result),
                    right: Box::new(right),
                }
            };
        }
        Ok(result)
    }

    /// Lower a single arm (no UNION marker inside).
    fn lower_pipeline(&mut self, clauses: &[Clause]) -> Result<Op, PolygraphError> {
        let mut op = Op::Unit;
        for clause in clauses {
            op = self.lower_clause(op, clause)?;
        }
        Ok(op)
    }

    fn lower_clause(&mut self, current: Op, clause: &Clause) -> Result<Op, PolygraphError> {
        match clause {
            Clause::Match(m) => {
                let match_op = self.lower_match_pattern(m)?;
                if m.optional {
                    // OPTIONAL MATCH wraps the current scope in a LeftOuterJoin so that
                    // if the pattern has no matches, the result row has null bindings.
                    // This is correct even when `current == Op::Unit` (i.e. the very
                    // first clause is OPTIONAL MATCH), producing one null row on empty graphs.
                    Ok(Op::LeftOuterJoin {
                        left: Box::new(current),
                        right: Box::new(match_op),
                        condition: None,
                    })
                } else if matches!(current, Op::Unit) {
                    Ok(match_op)
                } else {
                    // Join: two independent MATCH clauses share variables via natural join.
                    // Use CartesianProduct here; the SPARQL lowerer joins via shared vars.
                    Ok(Op::CartesianProduct {
                        left: Box::new(current),
                        right: Box::new(match_op),
                    })
                }
            }

            Clause::With(w) => self.lower_with(current, w),

            Clause::Return(r) => self.lower_return(current, r),

            Clause::Unwind(u) => {
                let list = self.lower_expr(&u.expression)?;
                Ok(Op::Unwind {
                    inner: Box::new(current),
                    list,
                    variable: u.variable.clone(),
                })
            }

            Clause::Create(c) => {
                let (nodes, edges) = self.lower_create_pattern(&c.pattern)?;
                Ok(Op::Create {
                    inner: Box::new(current),
                    nodes,
                    edges,
                })
            }

            Clause::Set(s) => {
                let items = self.lower_set_items(&s.items)?;
                Ok(Op::Set {
                    inner: Box::new(current),
                    items,
                })
            }

            Clause::Delete(d) => {
                let exprs = d
                    .expressions
                    .iter()
                    .map(|e| self.lower_expr(e))
                    .collect::<Result<_, _>>()?;
                Ok(Op::Delete {
                    inner: Box::new(current),
                    detach: d.detach,
                    exprs,
                })
            }

            Clause::Remove(r) => {
                let items = self.lower_remove_items(&r.items)?;
                Ok(Op::Remove {
                    inner: Box::new(current),
                    items,
                })
            }

            Clause::Merge(m) => {
                let clause = self.lower_merge_clause(m)?;
                Ok(Op::Merge {
                    inner: Box::new(current),
                    clause,
                })
            }

            Clause::Call(c) => {
                let args = c
                    .args
                    .iter()
                    .map(|e| self.lower_expr(e))
                    .collect::<Result<_, _>>()?;
                Ok(Op::Call {
                    inner: Box::new(current),
                    procedure: c.procedure.clone(),
                    args,
                    yields: c.yields.clone(),
                })
            }

            Clause::Union { .. } => Err(PolygraphError::Translation {
                message: "unexpected UNION inside lower_clause".into(),
            }),
        }
    }

    // ── MATCH ────────────────────────────────────────────────────────────────

    fn lower_match_pattern(&mut self, m: &ast::MatchClause) -> Result<Op, PolygraphError> {
        let mut op = self.lower_pattern_list(&m.pattern)?;
        if let Some(where_) = &m.where_ {
            let pred = self.lower_expr(&where_.expression)?;
            op = Op::Selection {
                inner: Box::new(op),
                predicate: pred,
            };
        }
        Ok(op)
    }

    fn lower_pattern_list(&mut self, pl: &ast::PatternList) -> Result<Op, PolygraphError> {
        let mut iter = pl.0.iter();
        let first =
            self.lower_pattern(iter.next().ok_or_else(|| PolygraphError::Translation {
                message: "empty pattern list".into(),
            })?)?;
        let mut result = first;
        for pat in iter {
            let right = self.lower_pattern(pat)?;
            result = Op::CartesianProduct {
                left: Box::new(result),
                right: Box::new(right),
            };
        }
        Ok(result)
    }

    /// Lower one path pattern: `(n:Label {prop: val})-[r:T]->(m)`.
    ///
    /// The elements are `[Node, Rel, Node, Rel, Node, …]` alternating.
    ///
    /// Rules:
    /// - A node AFTER a relationship is the `to` endpoint — it is already
    ///   bound by the relationship triple.  We add label/property constraints
    ///   via `Selection` wrappers but do NOT create an additional `Scan`.
    /// - A node that was already introduced by a previous MATCH pattern
    ///   (tracked in `self.seen_vars`) is not re-scanned either — SPARQL joins
    ///   on shared variable names automatically.
    /// - A node that is new AND is the first element of a pattern AND has a
    ///   label gets a normal `Scan`.
    fn lower_pattern(&mut self, p: &ast::Pattern) -> Result<Op, PolygraphError> {
        let elements = &p.elements;

        // Pre-assign names to every anonymous node position.
        let mut node_names: Vec<String> = Vec::new();
        for elem in elements {
            if let PatternElement::Node(n) = elem {
                node_names.push(n.variable.clone().unwrap_or_else(|| self.fresh("anon")));
            }
        }

        let mut op: Option<Op> = None;
        let mut node_idx = 0usize;
        let mut last_src: Option<String> = None;
        // Set to true just after we process a Relationship element.
        let mut after_rel = false;

        for elem in elements {
            match elem {
                PatternElement::Node(n) => {
                    let var = node_names[node_idx].clone();
                    node_idx += 1;

                    let is_seen = self.seen_vars.contains(&var);
                    self.seen_vars.insert(var.clone());

                    if after_rel || is_seen {
                        // The variable is already bound — don't create a Scan.
                        // Apply label and property constraints via Selection.
                        let mut acc = op.take().unwrap_or(Op::Unit);
                        for label in &n.labels {
                            acc = Op::Selection {
                                inner: Box::new(acc),
                                predicate: Expr::LabelCheck {
                                    expr: Box::new(Expr::var(&var)),
                                    labels: vec![label.clone()],
                                },
                            };
                        }
                        if let Some(props) = n.properties.as_deref() {
                            for (key, val) in props {
                                acc = Op::Selection {
                                    inner: Box::new(acc),
                                    predicate: Expr::Comparison(
                                        CmpOp::Eq,
                                        Box::new(Expr::Property(
                                            Box::new(Expr::var(&var)),
                                            key.clone(),
                                        )),
                                        Box::new(self.lower_expr(val)?),
                                    ),
                                };
                            }
                        }
                        op = Some(acc);
                    } else {
                        // Fresh variable with no prior binding — emit a Scan.
                        let scan = Op::Scan {
                            variable: var.clone(),
                            label: n.labels.first().cloned(),
                            extra_labels: if n.labels.len() > 1 {
                                n.labels[1..].to_vec()
                            } else {
                                vec![]
                            },
                        };
                        let scan_op =
                            self.apply_prop_predicates(scan, &var, n.properties.as_deref())?;
                        op = Some(match op.take() {
                            None => scan_op,
                            Some(prev) => Op::CartesianProduct {
                                left: Box::new(prev),
                                right: Box::new(scan_op),
                            },
                        });
                    }

                    after_rel = false;
                    last_src = Some(var);
                }

                PatternElement::Relationship(r) => {
                    let from = last_src
                        .clone()
                        .ok_or_else(|| PolygraphError::Translation {
                            message: "relationship pattern without preceding node variable".into(),
                        })?;
                    // The 'to' node is the next node element (not yet incremented).
                    let to = node_names[node_idx].clone();

                    let direction = match r.direction {
                        Direction::Right => LqaDir::Outgoing,
                        Direction::Left => LqaDir::Incoming,
                        Direction::Both => LqaDir::Undirected,
                    };

                    let range = r.range.as_ref().map(|rq| PathRange {
                        lower: rq.lower.unwrap_or(1),
                        upper: rq.upper,
                    });

                    let expand = Op::Expand {
                        inner: Box::new(op.take().unwrap_or(Op::Unit)),
                        from: from.clone(),
                        rel_var: r.variable.clone(),
                        to: to.clone(),
                        rel_types: r.rel_types.clone(),
                        direction,
                        range,
                        path_var: p.variable.clone(),
                    };

                    // Inline relationship property predicates: -[r {w: 1}]->
                    let expand_op = if let (Some(props), Some(rv)) = (&r.properties, &r.variable) {
                        self.apply_prop_predicates(expand, rv, Some(props))?
                    } else {
                        expand
                    };

                    op = Some(expand_op);
                    after_rel = true;
                    // last_src is NOT updated here; the next Node element will update it.
                }
            }
        }

        Ok(op.unwrap_or(Op::Unit))
    }

    /// Wrap `inner_op` with a Selection for each `(key, val)` property predicate.
    fn apply_prop_predicates(
        &mut self,
        inner_op: Op,
        var: &str,
        props: Option<&[(String, ast::Expression)]>,
    ) -> Result<Op, PolygraphError> {
        let Some(props) = props else {
            return Ok(inner_op);
        };
        let mut acc = inner_op;
        for (key, val) in props {
            let pred = Expr::Comparison(
                CmpOp::Eq,
                Box::new(Expr::Property(Box::new(Expr::var(var)), key.clone())),
                Box::new(self.lower_expr(val)?),
            );
            acc = Op::Selection {
                inner: Box::new(acc),
                predicate: pred,
            };
        }
        Ok(acc)
    }

    // ── WITH ─────────────────────────────────────────────────────────────────

    fn lower_with(&mut self, inner: Op, w: &ast::WithClause) -> Result<Op, PolygraphError> {
        let (proj_items, agg_items, post_group_aliases) = self.lower_return_items(&w.items)?;

        // Keep a copy for ORDER BY aggregate-to-alias rewriting.
        let agg_items_for_order = agg_items.clone();

        let projected = if !agg_items.is_empty() {
            let agg_aliases: Vec<String> = agg_items.iter().map(|a| a.alias.clone()).collect();
            let group_keys = proj_cols_keys(&proj_items, &agg_aliases, &post_group_aliases);
            let grouped = Op::GroupBy {
                inner: Box::new(inner),
                group_keys,
                agg_items,
            };
            Op::Projection {
                inner: Box::new(grouped),
                items: proj_items,
                distinct: w.distinct,
            }
        } else {
            Op::Projection {
                inner: Box::new(inner),
                items: proj_items,
                distinct: w.distinct,
            }
        };

        // WHERE on WITH applies after projection.
        let filtered = if let Some(wh) = &w.where_ {
            let pred = self.lower_expr(&wh.expression)?;
            Op::Selection {
                inner: Box::new(projected),
                predicate: pred,
            }
        } else {
            projected
        };

        let ordered = self.maybe_order_by_with_aggs(filtered, w.order_by.as_ref(), &agg_items_for_order)?;
        let skipped = self.maybe_skip(ordered, w.skip.as_ref())?;
        self.maybe_limit(skipped, w.limit.as_ref())
    }

    // ── RETURN ───────────────────────────────────────────────────────────────

    fn lower_return(&mut self, inner: Op, r: &ast::ReturnClause) -> Result<Op, PolygraphError> {
        let (proj_items, agg_items, post_group_aliases) = self.lower_return_items(&r.items)?;

        // Keep a copy for ORDER BY aggregate-to-alias rewriting (see below).
        let agg_items_for_order = agg_items.clone();

        let projected = if !agg_items.is_empty() {
            let agg_aliases: Vec<String> = agg_items.iter().map(|a| a.alias.clone()).collect();
            let group_keys = proj_cols_keys(&proj_items, &agg_aliases, &post_group_aliases);
            let grouped = Op::GroupBy {
                inner: Box::new(inner),
                group_keys,
                agg_items,
            };
            Op::Projection {
                inner: Box::new(grouped),
                items: proj_items,
                distinct: r.distinct,
            }
        } else {
            Op::Projection {
                inner: Box::new(inner),
                items: proj_items,
                distinct: r.distinct,
            }
        };

        // Use agg-aware ORDER BY so that `ORDER BY max(n.age)` resolves to
        // the alias variable bound by the GROUP BY rather than a bare aggregate.
        let ordered = self.maybe_order_by_with_aggs(projected, r.order_by.as_ref(), &agg_items_for_order)?;
        let skipped = self.maybe_skip(ordered, r.skip.as_ref())?;
        let limited = self.maybe_limit(skipped, r.limit.as_ref())?;

        if r.distinct {
            Ok(Op::Distinct {
                inner: Box::new(limited),
            })
        } else {
            Ok(limited)
        }
    }

    // ── Return / WITH items ───────────────────────────────────────────────────

    /// Split the item list into (projection items, aggregate items).
    fn lower_return_items(
        &mut self,
        items: &ast::ReturnItems,
    ) -> Result<(Vec<ProjItem>, Vec<AggItem>, Vec<String>), PolygraphError> {
        let mut proj: Vec<ProjItem> = Vec::new();
        let mut aggs: Vec<AggItem> = Vec::new();
        let mut gen_counter = 0u32;
        // Aliases of projection items that are computed POST-GROUP (not group keys):
        // compound expressions that contained extracted aggregate sub-expressions.
        let mut post_group_aliases: Vec<String> = Vec::new();

        match items {
            ast::ReturnItems::All => {
                // RETURN * — represented as a single catch-all; lowered to "project all vars"
                // The SPARQL lowerer handles this by not wrapping in Project.
                proj.push(ProjItem {
                    expr: Expr::var("*"),
                    alias: "*".into(),
                    display_name: None,
                });
            }
            ast::ReturnItems::Explicit(list) => {
                for item in list {
                    // Compute the Cypher output column name (the "natural alias").
                    // For an explicit AS, this is the alias text.
                    // For a bare variable, it's the variable name.
                    // For computed expressions like `max(x)`, use expr_natural_alias.
                    let explicit_alias = item.alias.as_deref();
                    let natural_cypher_name: Option<String> = explicit_alias
                        .map(|s| s.to_owned())
                        .or_else(|| expr_natural_alias(&item.expression));

                    let alias = explicit_alias.map(|s| s.to_owned()).unwrap_or_else(|| {
                        // If the expression is a bare variable reference, use its name as the
                        // implicit alias (e.g. `WITH i` or `RETURN x` → alias = "i"/"x"),
                        // matching openCypher semantics. Only generate a fresh name for
                        // computed expressions that have no natural identifier.
                        if let ast::Expression::Variable(v) = &item.expression {
                            v.clone()
                        } else {
                            let a = format!("_gen_{gen_counter}");
                            gen_counter += 1;
                            a
                        }
                    });
                    // The display_name is set when the Cypher column name differs from the
                    // SPARQL variable name (alias), e.g. `max(x)` gets alias `_gen_0` but
                    // display_name `max(x)` to match TCK column headers.
                    let display_name = match &natural_cypher_name {
                        Some(name) if name != &alias => Some(name.clone()),
                        _ => None,
                    };
                    let expr = self.lower_expr(&item.expression)?;
                    // Check if this expression is/wraps an aggregate (directly or nested).
                    if matches!(expr, Expr::Aggregate { .. }) {
                        // Emit as an aggregate: bind the agg expr to the alias.
                        aggs.push(AggItem {
                            expr: expr.clone(),
                            alias: alias.clone(),
                        });
                        // Project the output alias variable.
                        proj.push(ProjItem {
                            expr: Expr::var(&alias),
                            alias: alias.clone(),
                            display_name: display_name.clone(),
                        });
                    } else if expr_contains_aggregate(&expr) {
                        // Compound expression containing aggregate(s): extract each
                        // aggregate sub-expression, replacing it with a fresh variable.
                        // E.g. `count(a) + 3` → agg[_agg_0=count(a)], proj[_agg_0 + 3].
                        let extracted = extract_nested_aggregates(expr, &mut aggs, &mut gen_counter);
                        // Mark this alias as a post-group computation (not a group key).
                        post_group_aliases.push(alias.clone());
                        proj.push(ProjItem {
                            expr: extracted,
                            alias,
                            display_name,
                        });
                    } else {
                        proj.push(ProjItem {
                            expr,
                            alias,
                            display_name,
                        });
                    }
                }
            }
        }
        Ok((proj, aggs, post_group_aliases))
    }

    // ── ORDER BY / SKIP / LIMIT helpers ──────────────────────────────────────

    fn maybe_order_by(
        &mut self,
        op: Op,
        order_by: Option<&ast::OrderByClause>,
    ) -> Result<Op, PolygraphError> {
        self.maybe_order_by_with_aggs(op, order_by, &[])
    }

    /// Like `maybe_order_by`, but when ORDER BY expressions are aggregates that
    /// already appear in `agg_items` (e.g. `ORDER BY max(n.age)` when the RETURN
    /// clause also has `max(n.age)`), they are replaced by the corresponding alias
    /// variable rather than re-emitting the aggregate — which would be invalid
    /// in SPARQL since aggregates may only appear inside `GROUP BY` / aggregate
    /// expressions.
    fn maybe_order_by_with_aggs(
        &mut self,
        op: Op,
        order_by: Option<&ast::OrderByClause>,
        agg_items: &[AggItem],
    ) -> Result<Op, PolygraphError> {
        let Some(ob) = order_by else { return Ok(op) };
        let keys = ob
            .items
            .iter()
            .map(|si| {
                let lowered = self.lower_expr(&si.expression)?;
                // Replace any aggregate sub-expression with its alias variable
                // so the sort key references the GROUP BY output, not the aggregate.
                let resolved = rewrite_aggs_to_vars(lowered, agg_items);
                Ok(SortKey {
                    expr: resolved,
                    dir: if si.descending {
                        crate::lqa::expr::SortDir::Desc
                    } else {
                        crate::lqa::expr::SortDir::Asc
                    },
                })
            })
            .collect::<Result<Vec<_>, PolygraphError>>()?;
        Ok(Op::OrderBy {
            inner: Box::new(op),
            keys,
        })
    }

    fn maybe_skip(&mut self, op: Op, skip: Option<&ast::Expression>) -> Result<Op, PolygraphError> {
        let Some(s) = skip else { return Ok(op) };
        Ok(Op::Skip {
            inner: Box::new(op),
            count: self.lower_expr(s)?,
        })
    }

    fn maybe_limit(
        &mut self,
        op: Op,
        limit: Option<&ast::Expression>,
    ) -> Result<Op, PolygraphError> {
        let Some(l) = limit else { return Ok(op) };
        Ok(Op::Limit {
            inner: Box::new(op),
            count: self.lower_expr(l)?,
        })
    }

    // ── Expressions ───────────────────────────────────────────────────────────

    pub fn lower_expr(&mut self, e: &ast::Expression) -> Result<Expr, PolygraphError> {
        use ast::Expression as AE;
        match e {
            AE::Variable(v) => Ok(Expr::var(v)),
            AE::Literal(l) => Ok(Expr::Literal(lower_literal(l))),
            AE::Property(base, key) => Ok(Expr::Property(
                Box::new(self.lower_expr(base)?),
                key.clone(),
            )),
            AE::Add(a, b) => Ok(Expr::Add(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Subtract(a, b) => Ok(Expr::Sub(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Multiply(a, b) => Ok(Expr::Mul(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Divide(a, b) => Ok(Expr::Div(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Modulo(a, b) => Ok(Expr::Mod(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Power(a, b) => Ok(Expr::Pow(
                Box::new(self.lower_expr(a)?),
                Box::new(self.lower_expr(b)?),
            )),
            AE::Negate(a) => Ok(Expr::Unary(UnaryOp::Neg, Box::new(self.lower_expr(a)?))),
            AE::Not(a) => {
                if is_definitely_non_boolean(a) {
                    return Err(PolygraphError::Unsupported {
                        construct: "NOT with non-boolean literal operand".into(),
                        spec_ref: "openCypher 9 §6.2.3".into(),
                        reason: "type error: NOT requires a boolean operand; fall back to legacy for SyntaxError".into(),
                    });
                }
                Ok(Expr::Not(Box::new(self.lower_expr(a)?)))
            }
            AE::IsNull(a) => Ok(Expr::IsNull(Box::new(self.lower_expr(a)?))),
            AE::IsNotNull(a) => Ok(Expr::IsNotNull(Box::new(self.lower_expr(a)?))),
            AE::Or(a, b) => {
                if is_definitely_non_boolean(a) || is_definitely_non_boolean(b) {
                    return Err(PolygraphError::Unsupported {
                        construct: "OR with non-boolean literal operand".into(),
                        spec_ref: "openCypher 9 §6.2.3".into(),
                        reason: "type error: OR requires boolean operands; fall back to legacy for SyntaxError".into(),
                    });
                }
                Ok(Expr::Or(
                    Box::new(self.lower_expr(a)?),
                    Box::new(self.lower_expr(b)?),
                ))
            }
            AE::And(a, b) => {
                if is_definitely_non_boolean(a) || is_definitely_non_boolean(b) {
                    return Err(PolygraphError::Unsupported {
                        construct: "AND with non-boolean literal operand".into(),
                        spec_ref: "openCypher 9 §6.2.3".into(),
                        reason: "type error: AND requires boolean operands; fall back to legacy for SyntaxError".into(),
                    });
                }
                Ok(Expr::And(
                    Box::new(self.lower_expr(a)?),
                    Box::new(self.lower_expr(b)?),
                ))
            }
            AE::Xor(a, b) => {
                if is_definitely_non_boolean(a) || is_definitely_non_boolean(b) {
                    return Err(PolygraphError::Unsupported {
                        construct: "XOR with non-boolean literal operand".into(),
                        spec_ref: "openCypher 9 §6.2.3".into(),
                        reason: "type error: XOR requires boolean operands; fall back to legacy for SyntaxError".into(),
                    });
                }
                // Xor(a, b) = (a OR b) AND NOT (a AND b)
                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                Ok(Expr::And(
                    Box::new(Expr::Or(Box::new(la.clone()), Box::new(lb.clone()))),
                    Box::new(Expr::Not(Box::new(Expr::And(Box::new(la), Box::new(lb))))),
                ))
            }
            AE::Comparison(a, op, b) => {
                // Type-check `IN` before lowering: if the RHS is definitely not a list,
                // fall back to legacy which raises the proper SyntaxError.
                if matches!(op, CompOp::In) && is_definitely_non_list(b) {
                    return Err(PolygraphError::Unsupported {
                        construct: "IN with non-list literal RHS".into(),
                        spec_ref: "openCypher 9 §6.3.4".into(),
                        reason: "type error: IN requires a list on the RHS; fall back to legacy for SyntaxError".into(),
                    });
                }
                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                match op {
                    CompOp::Eq => Ok(Expr::Comparison(CmpOp::Eq, Box::new(la), Box::new(lb))),
                    CompOp::Ne => Ok(Expr::Comparison(CmpOp::Ne, Box::new(la), Box::new(lb))),
                    CompOp::Lt => Ok(Expr::Comparison(CmpOp::Lt, Box::new(la), Box::new(lb))),
                    CompOp::Le => Ok(Expr::Comparison(CmpOp::Le, Box::new(la), Box::new(lb))),
                    CompOp::Gt => Ok(Expr::Comparison(CmpOp::Gt, Box::new(la), Box::new(lb))),
                    CompOp::Ge => Ok(Expr::Comparison(CmpOp::Ge, Box::new(la), Box::new(lb))),
                    CompOp::In => Ok(Expr::Comparison(CmpOp::In, Box::new(la), Box::new(lb))),
                    CompOp::StartsWith => Ok(Expr::FunctionCall {
                        name: "startsWith".into(),
                        distinct: false,
                        args: vec![la, lb],
                    }),
                    CompOp::EndsWith => Ok(Expr::FunctionCall {
                        name: "endsWith".into(),
                        distinct: false,
                        args: vec![la, lb],
                    }),
                    CompOp::Contains => Ok(Expr::FunctionCall {
                        name: "contains".into(),
                        distinct: false,
                        args: vec![la, lb],
                    }),
                    CompOp::RegexMatch => Ok(Expr::FunctionCall {
                        name: "regex".into(),
                        distinct: false,
                        args: vec![la, lb],
                    }),
                }
            }
            AE::FunctionCall {
                name,
                distinct,
                args,
            } => {
                // range(start, end [, step]) with constant integer arguments:
                // Pre-evaluate to Expr::List so the UNWIND handler and IN operator
                // can use it as a literal list without falling back to legacy.
                if name.eq_ignore_ascii_case("range") && (args.len() == 2 || args.len() == 3) {
                    let start = if let AE::Literal(ast::Literal::Integer(n)) = &args[0] {
                        Some(*n)
                    } else {
                        None
                    };
                    let end_val = if let AE::Literal(ast::Literal::Integer(n)) = &args[1] {
                        Some(*n)
                    } else {
                        None
                    };
                    let step = if let Some(step_arg) = args.get(2) {
                        if let AE::Literal(ast::Literal::Integer(n)) = step_arg {
                            if *n == 0 {
                                None // step=0 is a runtime error; leave to legacy
                            } else {
                                Some(*n)
                            }
                        } else {
                            None // non-literal step; fall through to FunctionCall
                        }
                    } else {
                        Some(1i64)
                    };
                    if let (Some(s), Some(e), Some(st)) = (start, end_val, step) {
                        let mut items = Vec::new();
                        let mut i = s;
                        while (st > 0 && i <= e) || (st < 0 && i >= e) {
                            items.push(Expr::Literal(crate::lqa::expr::Literal::Integer(i)));
                            i += st;
                        }
                        return Ok(Expr::List(items));
                    }
                }
                let largs: Vec<Expr> = args
                    .iter()
                    .map(|a| self.lower_expr(a))
                    .collect::<Result<_, _>>()?;

                // ── Temporal constructor constant folding ───────────────────
                // If this is a temporal constructor and all of the LQA args are
                // ground literals (potentially wrapped in a function call that
                // was itself folded), call the pub(crate) temporal helpers from
                // the original AST-level args to produce a typed literal.
                // We do this AFTER lowering the args so that non-literal args
                // (e.g. temporal values from WITH bindings) have already been
                // lowered; but we pass the ORIGINAL `args` (AST) to the temporal
                // helpers because they expect `ast::cypher::Expression`.
                {
                    use crate::lqa::expr::Literal as LLit;
                    use crate::translator::cypher as tc;

                    const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";
                    const XSD_TIME: &str = "http://www.w3.org/2001/XMLSchema#time";
                    const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";

                    let name_lc = name.to_lowercase();
                    let base = name_lc
                        .strip_suffix(".transaction")
                        .or_else(|| name_lc.strip_suffix(".statement"))
                        .or_else(|| name_lc.strip_suffix(".realtime"))
                        .unwrap_or(name_lc.as_str());

                    let temporal_lit: Option<Expr> = match base {
                        "date" | "localtime" | "localdatetime" | "time" | "datetime"
                        | "duration" => {
                            // Zero-arg: placeholder literal
                            if args.is_empty() {
                                let (val, xsd) = match base {
                                    "date" => ("2000-01-01", XSD_DATE),
                                    "localtime" => ("00:00:00", XSD_TIME),
                                    "time" => ("00:00:00Z", XSD_TIME),
                                    "localdatetime" => ("2000-01-01T00:00:00", XSD_DATETIME),
                                    "datetime" => ("2000-01-01T00:00:00Z", XSD_DATETIME),
                                    "duration" => ("PT0S", ""),
                                    _ => unreachable!(),
                                };
                                let lit = if xsd.is_empty() {
                                    LLit::String(val.into())
                                } else {
                                    LLit::TypedLiteral(val.into(), xsd.into())
                                };
                                Some(Expr::Literal(lit))
                            }
                            // Null propagation
                            else if matches!(args.first(), Some(AE::Literal(ast::Literal::Null))) {
                                Some(Expr::Literal(LLit::Null))
                            }
                            // Map arg: call temporal helpers with ORIGINAL AST Map pairs.
                            // Guard: if any map value is a Variable reference, the
                            // caller may have WITH-bound substitutions we don't track
                            // here — fall through to legacy (return None).
                            else if let Some(AE::Map(pairs)) = args.first() {
                                let has_var = pairs.iter().any(|(_, v)| {
                                    matches!(v, AE::Variable(_))
                                });
                                if has_var {
                                    None
                                } else {
                                let result = match base {
                                    "date" => tc::temporal_date_from_map(pairs)
                                        .map(|s| LLit::TypedLiteral(s, XSD_DATE.into())),
                                    "localtime" => tc::temporal_localtime_from_map(pairs)
                                        .map(|s| LLit::TypedLiteral(s, XSD_TIME.into())),
                                    "time" => tc::temporal_time_from_map(pairs)
                                        .map(|s| LLit::TypedLiteral(s, XSD_TIME.into())),
                                    "localdatetime" => tc::temporal_localdatetime_from_map(pairs)
                                        .map(|s| LLit::TypedLiteral(s, XSD_DATETIME.into())),
                                    "datetime" => tc::temporal_datetime_from_map(pairs)
                                        .map(|s| LLit::TypedLiteral(s, XSD_DATETIME.into())),
                                    "duration" => tc::temporal_duration_from_map(pairs)
                                        .map(LLit::String),
                                    _ => None,
                                };
                                result.map(|lit| Expr::Literal(lit))
                                }
                            }
                            // String arg: call temporal string parse helper
                            else if let Some(AE::Literal(ast::Literal::String(s))) = args.first() {
                                let result = match base {
                                    "date" => tc::temporal_parse_date(s)
                                        .map(|v| LLit::TypedLiteral(v, XSD_DATE.into())),
                                    "localtime" => tc::temporal_parse_localtime(s)
                                        .map(|v| LLit::TypedLiteral(v, XSD_TIME.into())),
                                    "time" => tc::temporal_parse_time(s)
                                        .map(|v| LLit::TypedLiteral(v, XSD_TIME.into())),
                                    "localdatetime" => tc::temporal_parse_localdatetime(s)
                                        .map(|v| LLit::TypedLiteral(v, XSD_DATETIME.into())),
                                    "datetime" => tc::temporal_parse_datetime(s)
                                        .map(|v| LLit::TypedLiteral(v, XSD_DATETIME.into())),
                                    "duration" => tc::temporal_parse_duration(s)
                                        .map(LLit::String),
                                    _ => None,
                                };
                                result.map(|lit| Expr::Literal(lit))
                            } else {
                                None
                            }
                        }
                        // date.truncate / datetime.truncate / localdatetime.truncate / etc.
                        "date.truncate"
                        | "datetime.truncate"
                        | "localdatetime.truncate"
                        | "localtime.truncate"
                        | "time.truncate" => {
                            if args.len() >= 3 {
                                // Unit must be a string literal
                                let unit = if let AE::Literal(ast::Literal::String(u)) = &args[0] {
                                    Some(u.clone())
                                } else {
                                    None
                                };
                                // Overrides map
                                let overrides_pairs: Option<&Vec<(String, ast::Expression)>> =
                                    if let AE::Map(pairs) = &args[2] {
                                        Some(pairs)
                                    } else {
                                        None
                                    };
                                if let (Some(unit), Some(overrides)) = (unit, overrides_pairs) {
                                    // Build TcComponents from the "other" arg (args[1])
                                    let mut comps =
                                        tc::tc_from_expr(&args[1])
                                            .or_else(|| {
                                                // Fallback: try tc_from_iso_string for
                                                // string-literal "other" arg
                                                if let AE::Literal(ast::Literal::String(s)) =
                                                    &args[1]
                                                {
                                                    tc::tc_from_iso_string(s)
                                                } else {
                                                    None
                                                }
                                            });
                                    if let Some(ref mut c) = comps {
                                        tc::tc_apply_truncation(&unit, c);
                                        tc::tc_apply_overrides(overrides, c);

                                        const XSD_DATE_T: &str =
                                            "http://www.w3.org/2001/XMLSchema#date";
                                        const XSD_TIME_T: &str =
                                            "http://www.w3.org/2001/XMLSchema#time";
                                        const XSD_DT_T: &str =
                                            "http://www.w3.org/2001/XMLSchema#dateTime";

                                        let lit = match base {
                                            "date.truncate" => {
                                                let y = c.year.unwrap_or(0);
                                                let m = c.month.unwrap_or(1);
                                                let d = c.day.unwrap_or(1);
                                                LLit::TypedLiteral(
                                                    format!("{y:04}-{m:02}-{d:02}"),
                                                    XSD_DATE_T.into(),
                                                )
                                            }
                                            "datetime.truncate" => {
                                                let y = c.year.unwrap_or(0);
                                                let m = c.month.unwrap_or(1);
                                                let d = c.day.unwrap_or(1);
                                                let h = c.hour.unwrap_or(0);
                                                let mn = c.minute.unwrap_or(0);
                                                let s = c.second.unwrap_or(0);
                                                let ns = c.ns.unwrap_or(0);
                                                let t = tc::tc_fmt_time(h, mn, s, ns);
                                                let tz = c.tz.as_deref().unwrap_or("Z");
                                                LLit::TypedLiteral(
                                                    format!("{y:04}-{m:02}-{d:02}T{t}{tz}"),
                                                    XSD_DT_T.into(),
                                                )
                                            }
                                            "localdatetime.truncate" => {
                                                let y = c.year.unwrap_or(0);
                                                let m = c.month.unwrap_or(1);
                                                let d = c.day.unwrap_or(1);
                                                let h = c.hour.unwrap_or(0);
                                                let mn = c.minute.unwrap_or(0);
                                                let s = c.second.unwrap_or(0);
                                                let ns = c.ns.unwrap_or(0);
                                                let t = tc::tc_fmt_time(h, mn, s, ns);
                                                LLit::TypedLiteral(
                                                    format!("{y:04}-{m:02}-{d:02}T{t}"),
                                                    XSD_DT_T.into(),
                                                )
                                            }
                                            "localtime.truncate" => {
                                                let h = c.hour.unwrap_or(0);
                                                let mn = c.minute.unwrap_or(0);
                                                let s = c.second.unwrap_or(0);
                                                let ns = c.ns.unwrap_or(0);
                                                LLit::TypedLiteral(
                                                    tc::tc_fmt_time(h, mn, s, ns),
                                                    XSD_TIME_T.into(),
                                                )
                                            }
                                            "time.truncate" => {
                                                let h = c.hour.unwrap_or(0);
                                                let mn = c.minute.unwrap_or(0);
                                                let s = c.second.unwrap_or(0);
                                                let ns = c.ns.unwrap_or(0);
                                                let t = tc::tc_fmt_time(h, mn, s, ns);
                                                let tz = c.tz.as_deref().unwrap_or("Z");
                                                LLit::TypedLiteral(
                                                    format!("{t}{tz}"),
                                                    XSD_TIME_T.into(),
                                                )
                                            }
                                            _ => unreachable!(),
                                        };
                                        Some(Expr::Literal(lit))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    if let Some(folded) = temporal_lit {
                        return Ok(folded);
                    }

                    // ── Duration-between constant folding ─────────────────
                    // If this is a duration.between / duration.inMonths / etc.
                    // and both args are already-folded typed temporal literals,
                    // compute the duration at compile time.
                    let dur_lit = match base {
                        "duration.between" | "duration.inmonths" | "duration.indays"
                        | "duration.inseconds" => {
                            if largs.len() >= 2 {
                                // Null propagation: if either arg is null, result is null.
                                if matches!(largs[0], Expr::Literal(LLit::Null))
                                    || matches!(largs[1], Expr::Literal(LLit::Null))
                                {
                                    Some(Expr::Literal(LLit::Null))
                                } else {
                                    let v1 = match &largs[0] {
                                        Expr::Literal(LLit::TypedLiteral(v, _)) => Some(v.clone()),
                                        Expr::Literal(LLit::String(v))
                                            if v.starts_with('P') || v.starts_with("-P") =>
                                        {
                                            Some(v.clone())
                                        }
                                        _ => None,
                                    };
                                    let v2 = match &largs[1] {
                                        Expr::Literal(LLit::TypedLiteral(v, _)) => Some(v.clone()),
                                        Expr::Literal(LLit::String(v))
                                            if v.starts_with('P') || v.starts_with("-P") =>
                                        {
                                            Some(v.clone())
                                        }
                                        _ => None,
                                    };
                                    if let (Some(s1), Some(s2)) = (v1, v2) {
                                        let result = match base {
                                            "duration.between" => {
                                                tc::temporal_duration_between(&s1, &s2)
                                            }
                                            "duration.inmonths" => {
                                                tc::temporal_duration_in_months(&s1, &s2)
                                            }
                                            "duration.indays" => {
                                                tc::temporal_duration_in_days(&s1, &s2)
                                            }
                                            "duration.inseconds" => {
                                                tc::temporal_duration_in_seconds(&s1, &s2)
                                            }
                                            _ => None,
                                        };
                                        result.map(|s| Expr::Literal(LLit::String(s)))
                                    } else {
                                        None
                                    }
                                }
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    if let Some(folded) = dur_lit {
                        return Ok(folded);
                    }
                }
                // ─────────────────────────────────────────────────────────────
                Ok(Expr::FunctionCall {
                    name: name.clone(),
                    distinct: *distinct,
                    args: largs,
                })
            }
            AE::Aggregate(agg) => self.lower_agg(agg),
            AE::LabelCheck { variable, labels } => Ok(Expr::LabelCheck {
                expr: Box::new(Expr::var(variable)),
                labels: labels.clone(),
            }),
            AE::List(items) => {
                let litems = items
                    .iter()
                    .map(|i| self.lower_expr(i))
                    .collect::<Result<_, _>>()?;
                Ok(Expr::List(litems))
            }
            AE::Map(pairs) => {
                let lpairs = pairs
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), self.lower_expr(v)?)))
                    .collect::<Result<Vec<_>, PolygraphError>>()?;
                Ok(Expr::Map(lpairs))
            }
            AE::Subscript(a, b) => {
                // Constant-fold: if the list is a literal list and the index is
                // a literal integer, extract the element directly.
                if let AE::List(items) = a.as_ref() {
                    if let AE::Literal(ast::Literal::Integer(idx)) = b.as_ref() {
                        let len = items.len() as i64;
                        let i = if *idx < 0 { len + idx } else { *idx };
                        if i >= 0 && (i as usize) < items.len() {
                            return self.lower_expr(&items[i as usize]);
                        }
                        // Out-of-bounds → null.
                        return Ok(Expr::Literal(crate::lqa::expr::Literal::Null));
                    }
                }
                Ok(Expr::Subscript(
                    Box::new(self.lower_expr(a)?),
                    Box::new(self.lower_expr(b)?),
                ))
            }
            AE::ListSlice { list, start, end } => Ok(Expr::ListSlice {
                list: Box::new(self.lower_expr(list)?),
                start: start
                    .as_ref()
                    .map(|e| self.lower_expr(e))
                    .transpose()?
                    .map(Box::new),
                end: end
                    .as_ref()
                    .map(|e| self.lower_expr(e))
                    .transpose()?
                    .map(Box::new),
            }),
            AE::QuantifierExpr {
                kind,
                variable,
                list,
                predicate,
            } => {
                let qkind = match kind {
                    QuantifierKind::All => QuantKind::All,
                    QuantifierKind::Any => QuantKind::Any,
                    QuantifierKind::None => QuantKind::None,
                    QuantifierKind::Single => QuantKind::Single,
                };
                let pred = match predicate {
                    Some(p) => self.lower_expr(p)?,
                    None => Expr::Literal(Literal::Boolean(true)),
                };
                Ok(Expr::Quantifier {
                    kind: qkind,
                    variable: variable.clone(),
                    list: Box::new(self.lower_expr(list)?),
                    predicate: Box::new(pred),
                })
            }
            AE::ListComprehension {
                variable,
                list,
                predicate,
                projection,
            } => Ok(Expr::ListComprehension {
                variable: variable.clone(),
                list: Box::new(self.lower_expr(list)?),
                predicate: predicate
                    .as_ref()
                    .map(|p| self.lower_expr(p))
                    .transpose()?
                    .map(Box::new),
                projection: projection
                    .as_ref()
                    .map(|p| self.lower_expr(p))
                    .transpose()?
                    .map(Box::new),
            }),
            AE::PatternComprehension {
                alias: _alias,
                pattern,
                predicate,
                projection,
            } => {
                let pattern_op = self.lower_pattern(pattern)?;
                let subq_op = if let Some(p) = predicate {
                    let pred = self.lower_expr(p)?;
                    Op::Selection {
                        inner: Box::new(pattern_op),
                        predicate: pred,
                    }
                } else {
                    pattern_op
                };
                Ok(Expr::PatternComprehension {
                    alias: None,
                    pattern_op: Box::new(subq_op),
                    predicate: None,
                    projection: Box::new(self.lower_expr(projection)?),
                })
            }
            AE::CaseExpression {
                operand,
                whens,
                else_expr,
            } => {
                let branches = if let Some(subj) = operand {
                    // Simple CASE → normalise to searched CASE
                    let lsubj = self.lower_expr(subj)?;
                    whens
                        .iter()
                        .map(|(w, t)| {
                            Ok((
                                Expr::Comparison(
                                    CmpOp::Eq,
                                    Box::new(lsubj.clone()),
                                    Box::new(self.lower_expr(w)?),
                                ),
                                self.lower_expr(t)?,
                            ))
                        })
                        .collect::<Result<Vec<_>, PolygraphError>>()?
                } else {
                    whens
                        .iter()
                        .map(|(w, t)| Ok((self.lower_expr(w)?, self.lower_expr(t)?)))
                        .collect::<Result<Vec<_>, PolygraphError>>()?
                };
                Ok(Expr::CaseSearched {
                    branches,
                    else_expr: else_expr
                        .as_ref()
                        .map(|e| self.lower_expr(e))
                        .transpose()?
                        .map(Box::new),
                })
            }
            AE::ExistsSubquery { patterns, where_ } => {
                let pat_op = self.lower_pattern_list(patterns)?;
                let subq = if let Some(w) = where_ {
                    let pred = self.lower_expr(w)?;
                    Op::Selection {
                        inner: Box::new(pat_op),
                        predicate: pred,
                    }
                } else {
                    pat_op
                };
                Ok(Expr::Exists(Box::new(subq)))
            }
            AE::PatternPredicate(pat) => {
                let pat_op = self.lower_pattern(pat)?;
                Ok(Expr::Exists(Box::new(pat_op)))
            }
        }
    }

    fn lower_agg(&mut self, agg: &AggregateExpr) -> Result<Expr, PolygraphError> {
        match agg {
            AggregateExpr::Count { distinct, expr } => {
                let e = expr.as_ref().map(|e| self.lower_expr(e)).transpose()?;
                Ok(Expr::Aggregate {
                    kind: AggKind::Count,
                    distinct: *distinct,
                    arg: e.map(Box::new),
                })
            }
            AggregateExpr::Sum { distinct, expr } => Ok(Expr::Aggregate {
                kind: AggKind::Sum,
                distinct: *distinct,
                arg: Some(Box::new(self.lower_expr(expr)?)),
            }),
            AggregateExpr::Avg { distinct, expr } => Ok(Expr::Aggregate {
                kind: AggKind::Avg,
                distinct: *distinct,
                arg: Some(Box::new(self.lower_expr(expr)?)),
            }),
            AggregateExpr::Min { distinct, expr } => Ok(Expr::Aggregate {
                kind: AggKind::Min,
                distinct: *distinct,
                arg: Some(Box::new(self.lower_expr(expr)?)),
            }),
            AggregateExpr::Max { distinct, expr } => Ok(Expr::Aggregate {
                kind: AggKind::Max,
                distinct: *distinct,
                arg: Some(Box::new(self.lower_expr(expr)?)),
            }),
            AggregateExpr::Collect { distinct, expr } => Ok(Expr::Aggregate {
                kind: AggKind::Collect,
                distinct: *distinct,
                arg: Some(Box::new(self.lower_expr(expr)?)),
            }),
        }
    }

    // ── Write clause helpers ──────────────────────────────────────────────────

    fn lower_create_pattern(
        &mut self,
        pl: &ast::PatternList,
    ) -> Result<(Vec<CreateNode>, Vec<CreateEdge>), PolygraphError> {
        use crate::lqa::op::CreateNode;
        let mut nodes = Vec::new();
        let mut edges = Vec::new();

        for pat in &pl.0 {
            let mut node_names: Vec<String> = Vec::new();
            for elem in &pat.elements {
                if let PatternElement::Node(n) = elem {
                    node_names.push(n.variable.clone().unwrap_or_else(|| self.fresh("anon")));
                }
            }

            let mut node_idx = 0usize;
            let mut last_src: Option<String> = None;
            for elem in &pat.elements {
                match elem {
                    PatternElement::Node(n) => {
                        let var = node_names[node_idx].clone();
                        node_idx += 1;
                        let props = n
                            .properties
                            .as_deref()
                            .map(|ps| {
                                ps.iter()
                                    .map(|(k, v)| Ok((k.clone(), self.lower_expr(v)?)))
                                    .collect::<Result<Vec<_>, PolygraphError>>()
                            })
                            .transpose()?
                            .unwrap_or_default();
                        nodes.push(CreateNode {
                            variable: Some(var.clone()),
                            labels: n.labels.clone(),
                            properties: props,
                        });
                        last_src = Some(var);
                    }
                    PatternElement::Relationship(r) => {
                        let from = last_src.clone().unwrap_or_default();
                        let to = node_names[node_idx].clone();
                        let props = r
                            .properties
                            .as_deref()
                            .map(|ps| {
                                ps.iter()
                                    .map(|(k, v)| Ok((k.clone(), self.lower_expr(v)?)))
                                    .collect::<Result<Vec<_>, PolygraphError>>()
                            })
                            .transpose()?
                            .unwrap_or_default();
                        edges.push(CreateEdge {
                            variable: r.variable.clone(),
                            from,
                            to,
                            rel_type: r.rel_types.first().cloned().unwrap_or_default(),
                            direction: match r.direction {
                                Direction::Right => LqaDir::Outgoing,
                                Direction::Left => LqaDir::Incoming,
                                Direction::Both => LqaDir::Undirected,
                            },
                            properties: props,
                        });
                    }
                }
            }
        }
        Ok((nodes, edges))
    }

    fn lower_set_items(
        &mut self,
        items: &[ast::SetItem],
    ) -> Result<Vec<LqaSetItem>, PolygraphError> {
        items
            .iter()
            .map(|item| match item {
                ast::SetItem::Property {
                    variable,
                    key,
                    value,
                } => Ok(LqaSetItem::Property {
                    variable: variable.clone(),
                    key: key.clone(),
                    value: self.lower_expr(value)?,
                }),
                ast::SetItem::MergeMap { variable, map } => {
                    let props = map
                        .iter()
                        .map(|(k, v)| Ok((k.clone(), self.lower_expr(v)?)))
                        .collect::<Result<Vec<_>, PolygraphError>>()?;
                    Ok(LqaSetItem::MergeMap {
                        variable: variable.clone(),
                        map: Expr::Map(props),
                    })
                }
                ast::SetItem::NodeReplace { variable, value } => Ok(LqaSetItem::Replace {
                    variable: variable.clone(),
                    value: self.lower_expr(value)?,
                }),
                ast::SetItem::SetLabel { variable, labels } => Ok(LqaSetItem::Label {
                    variable: variable.clone(),
                    labels: labels.clone(),
                }),
            })
            .collect()
    }

    fn lower_remove_items(
        &mut self,
        items: &[ast::RemoveItem],
    ) -> Result<Vec<LqaRemoveItem>, PolygraphError> {
        items
            .iter()
            .map(|item| match item {
                ast::RemoveItem::Property { variable, key } => Ok(LqaRemoveItem::Property {
                    variable: variable.clone(),
                    key: key.clone(),
                }),
                ast::RemoveItem::Label { variable, labels } => Ok(LqaRemoveItem::Label {
                    variable: variable.clone(),
                    labels: labels.clone(),
                }),
            })
            .collect()
    }

    fn lower_merge_clause(
        &mut self,
        m: &ast::MergeClause,
    ) -> Result<LqaMergeClause, PolygraphError> {
        let pattern_op = self.lower_pattern(&m.pattern)?;
        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        for action in &m.actions {
            let items = self.lower_set_items(&action.items)?;
            if action.on_create {
                on_create.extend(items);
            } else {
                on_match.extend(items);
            }
        }
        Ok(LqaMergeClause {
            pattern: Box::new(pattern_op),
            on_match,
            on_create,
        })
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

fn lower_literal(l: &ast::Literal) -> Literal {
    match l {
        ast::Literal::Integer(n) => Literal::Integer(*n),
        ast::Literal::Float(f) => Literal::Float(*f),
        ast::Literal::String(s) => Literal::String(s.clone()),
        ast::Literal::Boolean(b) => Literal::Boolean(*b),
        ast::Literal::Null => Literal::Null,
    }
}

/// Extract GROUP BY keys from a projection list: variables that are NOT
/// themselves aggregate-output aliases.
///
/// After `lower_return_items`, every aggregate `AGG(x) AS alias` produces:
///   - an `AggItem { alias: "alias", … }` in the agg list
///   - a `ProjItem { expr: Var("alias"), alias: "alias" }` in the proj list
///
/// Those proj-list entries must NOT become GROUP BY keys — the alias is the
/// aggregate output, not an input column.
fn proj_cols_keys(
    items: &[ProjItem],
    agg_aliases: &[String],
    post_group_aliases: &[String],
) -> Vec<String> {
    let agg_set: std::collections::HashSet<&str> = agg_aliases.iter().map(|s| s.as_str()).collect();
    let post_set: std::collections::HashSet<&str> =
        post_group_aliases.iter().map(|s| s.as_str()).collect();
    items
        .iter()
        .filter_map(|pi| {
            // Every non-aggregate, non-wildcard projection item is a GROUP BY key,
            // EXCEPT for compound expressions that contain extracted aggregates
            // (tracked in post_group_aliases) — those are computed AFTER GROUP BY
            // as Extend steps.
            if pi.alias != "*"
                && !agg_set.contains(pi.alias.as_str())
                && !post_set.contains(pi.alias.as_str())
            {
                Some(pi.alias.clone())
            } else {
                None
            }
        })
        .collect()
}

/// Returns `true` when `expr` is definitely not a boolean value (e.g. an
/// integer literal, a string, a list).  Used to detect static type errors for
/// AND / OR / XOR / NOT operands so that LQA can fall back to the legacy
/// translator, which surfaces the proper `SyntaxError`.
fn is_definitely_non_boolean(expr: &ast::Expression) -> bool {
    use ast::{Expression as E, Literal};
    matches!(
        expr,
        E::Literal(Literal::Integer(_))
            | E::Literal(Literal::Float(_))
            | E::Literal(Literal::String(_))
            | E::List(_)
            | E::Map(_)
            | E::Negate(_)
    )
}

/// Returns `true` when `expr` is definitely not a list value (e.g. a boolean,
/// integer, float, string, or map literal).  Used to detect static type errors
/// for `IN` expressions.
fn is_definitely_non_list(expr: &ast::Expression) -> bool {
    use ast::{Expression as E, Literal};
    matches!(
        expr,
        E::Literal(Literal::Boolean(_))
            | E::Literal(Literal::Integer(_))
            | E::Literal(Literal::Float(_))
            | E::Literal(Literal::String(_))
            | E::Map(_)
    )
}

/// Replace every `Expr::Aggregate` sub-expression that exactly matches an
/// entry in `agg_items` with the corresponding alias variable.  This is used
/// to rewrite ORDER BY sort-key expressions when the same aggregate already
/// appears in the RETURN/WITH clause: in SPARQL the sort key must reference
/// the Group-bound variable, not repeat the aggregate.
///
/// Compound expressions like `count(a) + 1` are also rewritten recursively.
fn rewrite_aggs_to_vars(expr: Expr, agg_items: &[AggItem]) -> Expr {
    // First check if the whole expression is a known aggregate → alias var.
    if let Expr::Aggregate { .. } = &expr {
        for ai in agg_items {
            if ai.expr == expr {
                return Expr::var(&ai.alias);
            }
        }
        // Aggregate not found in the map — return as-is (will fail later).
        return expr;
    }
    // Recursively rewrite compound expressions.
    match expr {
        Expr::Add(a, b) => Expr::Add(
            Box::new(rewrite_aggs_to_vars(*a, agg_items)),
            Box::new(rewrite_aggs_to_vars(*b, agg_items)),
        ),
        Expr::Sub(a, b) => Expr::Sub(
            Box::new(rewrite_aggs_to_vars(*a, agg_items)),
            Box::new(rewrite_aggs_to_vars(*b, agg_items)),
        ),
        Expr::Mul(a, b) => Expr::Mul(
            Box::new(rewrite_aggs_to_vars(*a, agg_items)),
            Box::new(rewrite_aggs_to_vars(*b, agg_items)),
        ),
        Expr::Div(a, b) => Expr::Div(
            Box::new(rewrite_aggs_to_vars(*a, agg_items)),
            Box::new(rewrite_aggs_to_vars(*b, agg_items)),
        ),
        Expr::Mod(a, b) => Expr::Mod(
            Box::new(rewrite_aggs_to_vars(*a, agg_items)),
            Box::new(rewrite_aggs_to_vars(*b, agg_items)),
        ),
        other => other,
    }
}

/// Returns `true` if the LQA expression tree contains any `Expr::Aggregate` node.
/// Used to detect compound aggregate expressions like `count(a) + 3`.
fn expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate { .. } => true,
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b)
        | Expr::Mod(a, b) | Expr::Pow(a, b) | Expr::And(a, b) | Expr::Or(a, b)
        | Expr::Xor(a, b) | Expr::Comparison(_, a, b) => {
            expr_contains_aggregate(a) || expr_contains_aggregate(b)
        }
        Expr::Unary(_, e) | Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::Property(e, _) => expr_contains_aggregate(e),
        Expr::FunctionCall { args, .. } => args.iter().any(expr_contains_aggregate),
        Expr::List(items) => items.iter().any(expr_contains_aggregate),
        Expr::CaseSearched { branches, else_expr } => {
            branches.iter().any(|(w, t)| expr_contains_aggregate(w) || expr_contains_aggregate(t))
                || else_expr.as_ref().map_or(false, |e| expr_contains_aggregate(e))
        }
        _ => false,
    }
}

/// Recursively replace every `Expr::Aggregate` sub-expression in `expr` with a
/// fresh variable reference, pushing the extracted aggregate into `aggs`.
/// Returns the transformed expression.
fn extract_nested_aggregates(expr: Expr, aggs: &mut Vec<AggItem>, counter: &mut u32) -> Expr {
    match expr {
        Expr::Aggregate { .. } => {
            let alias = format!("_agg_ex_{}", *counter);
            *counter += 1;
            aggs.push(AggItem { expr, alias: alias.clone() });
            Expr::var(&alias)
        }
        Expr::Add(a, b) => Expr::Add(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Sub(a, b) => Expr::Sub(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Mul(a, b) => Expr::Mul(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Div(a, b) => Expr::Div(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Mod(a, b) => Expr::Mod(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Pow(a, b) => Expr::Pow(
            Box::new(extract_nested_aggregates(*a, aggs, counter)),
            Box::new(extract_nested_aggregates(*b, aggs, counter)),
        ),
        Expr::Unary(op, e) => Expr::Unary(op, Box::new(extract_nested_aggregates(*e, aggs, counter))),
        Expr::Not(e) => Expr::Not(Box::new(extract_nested_aggregates(*e, aggs, counter))),
        Expr::FunctionCall { name, distinct, args } => Expr::FunctionCall {
            name,
            distinct,
            args: args.into_iter().map(|a| extract_nested_aggregates(a, aggs, counter)).collect(),
        },
        _ => expr,
    }
}

/// Derive the "natural" implicit Cypher alias for a return expression.
///
/// openCypher specifies that, when no `AS alias` is given, the column name is
/// the original expression text.  This function recreates that text for the
/// common cases so that TCK column headers match.
///
/// Returns `None` when no natural alias exists (caller generates `_gen_N`).
fn expr_natural_alias(expr: &ast::Expression) -> Option<String> {
    use ast::Expression as E;
    match expr {
        E::Variable(v) => Some(v.clone()),
        E::Property(base, key) => {
            let base_alias = expr_natural_alias(base)?;
            Some(format!("{}.{}", base_alias, key))
        }
        E::FunctionCall { name, args, .. } => {
            // e.g. `count(n)` → `count(n)`, `type(r)` → `type(r)`
            let arg_strs: Option<Vec<String>> = args.iter().map(expr_natural_alias).collect();
            // Only produce a natural alias if all args have natural aliases.
            arg_strs.map(|args| format!("{}({})", name, args.join(", ")))
        }
        _ => None,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_cypher;

    fn lower(src: &str) -> Op {
        let ast = parse_cypher(src).expect("parse");
        let mut l = AstLowerer::new();
        l.lower_query(&ast).expect("lower")
    }

    #[test]
    fn scan_with_label() {
        let op = lower("MATCH (n:Person) RETURN n");
        // Must have a Scan or Projection somewhere above it
        assert!(format!("{op:?}").contains("Scan"));
    }

    #[test]
    fn selection_from_where() {
        let op = lower("MATCH (n:Person) WHERE n.age > 30 RETURN n.name");
        let s = format!("{op:?}");
        assert!(s.contains("Selection"));
        assert!(s.contains("Projection"));
    }

    #[test]
    fn relationship_pattern() {
        let op = lower("MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name");
        let s = format!("{op:?}");
        assert!(s.contains("Expand"));
    }

    #[test]
    fn union_all() {
        let op = lower("MATCH (n:A) RETURN n UNION ALL MATCH (n:B) RETURN n");
        assert!(matches!(op, Op::UnionAll { .. }));
    }

    #[test]
    fn with_clause() {
        let op = lower("MATCH (n:Person) WITH n RETURN n");
        let s = format!("{op:?}");
        assert!(s.contains("Projection"));
    }

    #[test]
    fn optional_match() {
        let op = lower("MATCH (n) OPTIONAL MATCH (n)-[r]->(m) RETURN n, m");
        assert!(
            matches!(&op, Op::Projection { inner, .. } if matches!(inner.as_ref(), Op::LeftOuterJoin { .. }))
        );
    }

    #[test]
    fn order_by_limit() {
        let op = lower("MATCH (n:Person) RETURN n.name ORDER BY n.name LIMIT 10");
        let s = format!("{op:?}");
        assert!(s.contains("OrderBy"));
        assert!(s.contains("Limit"));
    }

    #[test]
    fn aggregate() {
        let op = lower("MATCH (n:Person) RETURN count(n) AS cnt");
        let s = format!("{op:?}");
        assert!(s.contains("GroupBy"));
    }
}
