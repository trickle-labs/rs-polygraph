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
    /// Named path variables and their static hop count for fixed-length patterns.
    /// Set when a non-varlen Op::Expand carries a `path_var`; each expand
    /// in a chain increments the counter so multi-hop paths record the total length.
    /// Varlen paths are excluded (their lengths are dynamic and cannot be
    /// resolved at compile time).
    path_lengths: HashMap<String, usize>,
    /// Ordered list of node variables for each fixed-length named path.
    /// Used to implement `nodes(p)` → the sequence of node IRIs along the path.
    /// Populated in the Op::Expand handler alongside `path_lengths`.
    path_node_vars: HashMap<String, Vec<Variable>>,
    /// Variables in the current scope that are known at compile time to hold
    /// a specific integer value.  Populated by Op::Projection when a projection
    /// item is a bare integer literal (e.g. `WITH 0 AS start`), a pass-through
    /// of an already-known const-int var, or `size(known_list)`.
    /// Consumed by `eval_range_args` to enable `range(start, end)` where start/end
    /// are bound to compile-time constants.
    const_int_vars: HashMap<String, i64>,
    /// Known literal list lengths for `size(var)` const-folding.
    /// Maps variable name → list element count (populated when `WITH list AS v`
    /// assigns a plain list literal, or `UNWIND` of a sized literal list).
    list_size_vars: HashMap<String, usize>,
    /// Variables bound to map literals via WITH, e.g. `WITH {k: v} AS m`.
    /// Stores the key-value pairs so that `m.k` can be constant-folded at
    /// compile time without emitting a runtime SPARQL lookup.
    scalar_map_exprs: HashMap<String, Vec<(String, Expr)>>,
    /// For `collect()` aggregates: maps output alias → raw GROUP_CONCAT variable.
    /// After the GROUP pattern, each entry is used to emit
    /// `BIND(CONCAT("[", COALESCE(?raw, ""), "]") AS ?alias)`.
    collect_post_wraps: Vec<(String, Variable)>,
    /// BINDs that must be emitted inside a GROUP body for collect() args.
    /// Each entry is `(proj_var, arg_expr)` → `BIND(arg_expr AS proj_var)`
    /// inserted into `group_inner` before the GROUP is assembled.
    collect_group_binds: Vec<(Variable, SparExpr)>,
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
            path_lengths: HashMap::new(),
            path_node_vars: HashMap::new(),
            const_int_vars: HashMap::new(),
            list_size_vars: HashMap::new(),
            scalar_map_exprs: HashMap::new(),
            collect_post_wraps: Vec::new(),
            collect_group_binds: Vec::new(),
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

    /// Serialize an LQA map literal to the Cypher string form `{key: value, ...}`.
    /// Used for `properties({...})` compile-time serialization.
    fn serialize_map_literal(pairs: &[(String, crate::lqa::expr::Expr)]) -> String {
        use crate::lqa::expr::{Expr as E, Literal as ELit};
        fn ser(e: &E) -> String {
            match e {
                E::Literal(ELit::Integer(n)) => n.to_string(),
                E::Literal(ELit::Float(f)) => {
                    // Match legacy cypher_float_str output (no trailing .0 stripping here;
                    // use the same format as Display for f64).
                    format!("{f}")
                }
                E::Literal(ELit::String(s)) => format!("'{s}'"),
                E::Literal(ELit::Boolean(b)) => b.to_string(),
                E::Literal(ELit::Null) => "null".to_string(),
                E::List(items) => {
                    let inner: Vec<String> = items.iter().map(ser).collect();
                    format!("[{}]", inner.join(", "))
                }
                E::Map(inner_pairs) => {
                    let entries: Vec<String> = inner_pairs
                        .iter()
                        .map(|(k, v)| format!("{k}: {}", ser(v)))
                        .collect();
                    format!("{{{}}}", entries.join(", "))
                }
                E::Unary(crate::lqa::expr::UnaryOp::Neg, inner) => match inner.as_ref() {
                    E::Literal(ELit::Integer(n)) => format!("-{n}"),
                    E::Literal(ELit::Float(f)) => format!("{}", -f),
                    _ => "?".to_string(),
                },
                _ => "?".to_string(),
            }
        }
        let entries: Vec<String> = pairs
            .iter()
            .map(|(k, v)| format!("{k}: {}", ser(v)))
            .collect();
        format!("{{{}}}", entries.join(", "))
    }

    // ── properties() helpers ──────────────────────────────────────────────────

    /// Build a SPARQL expression for `properties(node_var)`.
    /// Emits a GROUP BY subquery in `pending_optional_patterns` that collects
    /// all base-namespace property key-value pairs for the node and formats them
    /// as the Cypher map string `{key1: val1, key2: val2, ...}`.
    fn build_node_properties_expr(&mut self, vname: &str) -> SparExpr {
        let node_var = Self::var(vname);
        let pred_var = self.fresh("_pp_pred");
        let val_var = self.fresh("_pp_val");
        let pair_var = self.fresh("_pp_pair");
        let raw_var = self.fresh("_pp_raw");
        let base = self.base_iri.clone();
        let base_len = base.len();
        let sentinel_str = format!("{base}__node");

        // BGP: ?node ?pred ?val
        let bgp = GraphPattern::Bgp {
            patterns: vec![TriplePattern {
                subject: TermPattern::Variable(node_var.clone()),
                predicate: NamedNodePattern::Variable(pred_var.clone()),
                object: TermPattern::Variable(val_var.clone()),
            }],
        };

        // FILTER: STRSTARTS(STR(?pred), base) && ?pred != sentinel && ?pred != rdf:type
        let str_pred = SparExpr::FunctionCall(
            Function::Str,
            vec![SparExpr::Variable(pred_var.clone())],
        );
        let strstarts = SparExpr::FunctionCall(
            Function::StrStarts,
            vec![
                str_pred.clone(),
                SparExpr::Literal(SparLit::new_simple_literal(base.clone())),
            ],
        );
        let not_sentinel = SparExpr::Not(Box::new(SparExpr::Equal(
            Box::new(str_pred.clone()),
            Box::new(SparExpr::Literal(SparLit::new_simple_literal(sentinel_str))),
        )));
        let not_rdf_type = SparExpr::Not(Box::new(SparExpr::Equal(
            Box::new(str_pred),
            Box::new(SparExpr::Literal(SparLit::new_simple_literal(RDF_TYPE.to_string()))),
        )));
        let filter_expr = SparExpr::And(
            Box::new(strstarts),
            Box::new(SparExpr::And(Box::new(not_sentinel), Box::new(not_rdf_type))),
        );
        let filtered = GraphPattern::Filter {
            expr: filter_expr,
            inner: Box::new(bgp),
        };

        // key string: SUBSTR(STR(?pred), base_len + 1)
        let key_expr = SparExpr::FunctionCall(
            Function::SubStr,
            vec![
                SparExpr::FunctionCall(Function::Str, vec![SparExpr::Variable(pred_var)]),
                SparExpr::Literal(SparLit::new_typed_literal(
                    (base_len + 1).to_string(),
                    NamedNode::new_unchecked(XSD_INTEGER),
                )),
            ],
        );
        // value format: IF(numeric_or_boolean, STR(?val), CONCAT("'", STR(?val), "'"))
        let pair_expr = SparExpr::FunctionCall(
            Function::Concat,
            vec![
                key_expr,
                SparExpr::Literal(SparLit::new_simple_literal(": ")),
                Self::sparql_cypher_format_value(val_var),
            ],
        );
        let extended = GraphPattern::Extend {
            inner: Box::new(filtered),
            variable: pair_var.clone(),
            expression: pair_expr,
        };
        let gc_agg = AggregateExpression::FunctionCall {
            name: AggregateFunction::GroupConcat {
                separator: Some(", ".to_string()),
            },
            expr: SparExpr::Variable(pair_var),
            distinct: false,
        };
        let group_pat = GraphPattern::Group {
            inner: Box::new(extended),
            variables: vec![node_var],
            aggregates: vec![(raw_var.clone(), gc_agg)],
        };
        // Push Group directly (no Extend wrapper) to mirror the labels() pattern.
        self.pending_optional_patterns.push(group_pat);

        // Return: IF(BOUND(?raw), CONCAT("{", ?raw, "}"), "{}")
        SparExpr::If(
            Box::new(SparExpr::Bound(raw_var.clone())),
            Box::new(SparExpr::FunctionCall(
                Function::Concat,
                vec![
                    SparExpr::Literal(SparLit::new_simple_literal("{")),
                    SparExpr::Variable(raw_var),
                    SparExpr::Literal(SparLit::new_simple_literal("}")),
                ],
            )),
            Box::new(SparExpr::Literal(SparLit::new_simple_literal("{}"))),
        )
    }

    /// Build a SPARQL expression for `properties(edge_var)`.
    /// Uses RDF-star reification to enumerate edge properties.
    fn build_edge_properties_expr(&mut self, vname: &str) -> SparExpr {
        let edge_info = match self.edge_vars.get(vname).cloned() {
            Some(info) => info,
            None => {
                // Fallback: return unbound (shouldn't happen since we check contains_key)
                return SparExpr::Variable(self.fresh("_null"));
            }
        };
        let subj_var = Self::var(&edge_info.subj);
        let obj_var = Self::var(&edge_info.obj);
        let reif_var = self.fresh("_ep_reif");
        let pred_var = self.fresh("_ep_pred");
        let val_var = self.fresh("_ep_val");
        let pair_var = self.fresh("_ep_pair");
        let raw_var = self.fresh("_ep_raw");
        let base = self.base_iri.clone();
        let base_len = base.len();
        let rdf_reifies = NamedNode::new_unchecked(
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies",
        );

        // Build the edge triple term: <<subj pred obj>>
        let (inner_bgp, group_by_vars) = match &edge_info.pred {
            EdgePred::Static(iri) => {
                let edge_triple_tp = TermPattern::Triple(Box::new(TriplePattern {
                    subject: TermPattern::Variable(subj_var.clone()),
                    predicate: NamedNodePattern::NamedNode(iri.clone()),
                    object: TermPattern::Variable(obj_var.clone()),
                }));
                let bgp = GraphPattern::Bgp {
                    patterns: vec![
                        TriplePattern {
                            subject: TermPattern::Variable(reif_var.clone()),
                            predicate: NamedNodePattern::NamedNode(rdf_reifies),
                            object: edge_triple_tp,
                        },
                        TriplePattern {
                            subject: TermPattern::Variable(reif_var.clone()),
                            predicate: NamedNodePattern::Variable(pred_var.clone()),
                            object: TermPattern::Variable(val_var.clone()),
                        },
                    ],
                };
                (bgp, vec![subj_var, obj_var])
            }
            EdgePred::Dynamic(dyn_pred_var) => {
                let edge_triple_tp = TermPattern::Triple(Box::new(TriplePattern {
                    subject: TermPattern::Variable(subj_var.clone()),
                    predicate: NamedNodePattern::Variable(dyn_pred_var.clone()),
                    object: TermPattern::Variable(obj_var.clone()),
                }));
                let bgp = GraphPattern::Bgp {
                    patterns: vec![
                        TriplePattern {
                            subject: TermPattern::Variable(reif_var.clone()),
                            predicate: NamedNodePattern::NamedNode(rdf_reifies),
                            object: edge_triple_tp,
                        },
                        TriplePattern {
                            subject: TermPattern::Variable(reif_var.clone()),
                            predicate: NamedNodePattern::Variable(pred_var.clone()),
                            object: TermPattern::Variable(val_var.clone()),
                        },
                    ],
                };
                (bgp, vec![subj_var, dyn_pred_var.clone(), obj_var])
            }
        };

        // FILTER: STRSTARTS(STR(?pred), base)
        let str_pred = SparExpr::FunctionCall(
            Function::Str,
            vec![SparExpr::Variable(pred_var.clone())],
        );
        let filter_expr = SparExpr::FunctionCall(
            Function::StrStarts,
            vec![
                str_pred,
                SparExpr::Literal(SparLit::new_simple_literal(base.clone())),
            ],
        );
        let filtered = GraphPattern::Filter {
            expr: filter_expr,
            inner: Box::new(inner_bgp),
        };

        // key: SUBSTR(STR(?pred), base_len + 1)
        let key_expr = SparExpr::FunctionCall(
            Function::SubStr,
            vec![
                SparExpr::FunctionCall(Function::Str, vec![SparExpr::Variable(pred_var)]),
                SparExpr::Literal(SparLit::new_typed_literal(
                    (base_len + 1).to_string(),
                    NamedNode::new_unchecked(XSD_INTEGER),
                )),
            ],
        );
        let pair_expr = SparExpr::FunctionCall(
            Function::Concat,
            vec![
                key_expr,
                SparExpr::Literal(SparLit::new_simple_literal(": ")),
                Self::sparql_cypher_format_value(val_var),
            ],
        );
        let extended = GraphPattern::Extend {
            inner: Box::new(filtered),
            variable: pair_var.clone(),
            expression: pair_expr,
        };

        let gc_agg = AggregateExpression::FunctionCall {
            name: AggregateFunction::GroupConcat {
                separator: Some(", ".to_string()),
            },
            expr: SparExpr::Variable(pair_var),
            distinct: false,
        };
        let group_pat = GraphPattern::Group {
            inner: Box::new(extended),
            variables: group_by_vars,
            aggregates: vec![(raw_var.clone(), gc_agg)],
        };
        // Push Group directly (no Extend wrapper) to mirror the labels() pattern.
        self.pending_optional_patterns.push(group_pat);

        // Return: IF(BOUND(?raw), CONCAT("{", ?raw, "}"), "{}")
        SparExpr::If(
            Box::new(SparExpr::Bound(raw_var.clone())),
            Box::new(SparExpr::FunctionCall(
                Function::Concat,
                vec![
                    SparExpr::Literal(SparLit::new_simple_literal("{")),
                    SparExpr::Variable(raw_var),
                    SparExpr::Literal(SparLit::new_simple_literal("}")),
                ],
            )),
            Box::new(SparExpr::Literal(SparLit::new_simple_literal("{}"))),
        )
    }

    /// Format a SPARQL value as a Cypher literal element:
    /// numeric/boolean → plain STR, strings → `'...'` wrapped.
    fn sparql_cypher_format_value(val_var: Variable) -> SparExpr {
        let val_expr = SparExpr::Variable(val_var.clone());
        let dt_expr = SparExpr::FunctionCall(Function::Datatype, vec![val_expr]);
        let str_val = SparExpr::FunctionCall(Function::Str, vec![SparExpr::Variable(val_var.clone())]);

        // Build: numeric_or_bool = (DATATYPE = xsd:integer || xsd:long || xsd:decimal || xsd:double || xsd:float || xsd:boolean)
        let mk_dt_eq = |iri: &str| {
            SparExpr::Equal(
                Box::new(dt_expr.clone()),
                Box::new(SparExpr::NamedNode(NamedNode::new_unchecked(iri))),
            )
        };
        let is_numeric_or_bool = SparExpr::Or(
            Box::new(mk_dt_eq(XSD_INTEGER)),
            Box::new(SparExpr::Or(
                Box::new(mk_dt_eq("http://www.w3.org/2001/XMLSchema#long")),
                Box::new(SparExpr::Or(
                    Box::new(mk_dt_eq("http://www.w3.org/2001/XMLSchema#decimal")),
                    Box::new(SparExpr::Or(
                        Box::new(mk_dt_eq(XSD_DOUBLE)),
                        Box::new(SparExpr::Or(
                            Box::new(mk_dt_eq("http://www.w3.org/2001/XMLSchema#float")),
                            Box::new(mk_dt_eq(XSD_BOOLEAN)),
                        )),
                    )),
                )),
            )),
        );

        // IF numeric_or_bool: STR(?val)
        // ELSE: CONCAT("'", STR(?val), "'")
        SparExpr::If(
            Box::new(is_numeric_or_bool),
            Box::new(str_val.clone()),
            Box::new(SparExpr::FunctionCall(
                Function::Concat,
                vec![
                    SparExpr::Literal(SparLit::new_simple_literal("'")),
                    str_val,
                    SparExpr::Literal(SparLit::new_simple_literal("'")),
                ],
            )),
        )
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
                                    // Find the underlying expression this alias refers to.
                                    let underlying = items
                                        .iter()
                                        .find(|pi| {
                                            pi.alias == *name
                                                && !matches!(pi.expr, Expr::Variable { .. })
                                        })
                                        .map(|pi| &pi.expr);
                                    // If the underlying expression is a Property access, do NOT
                                    // expand it here.  `lower_projection_inner` generates a
                                    // BIND(prop_var AS alias) for every projection item, so the
                                    // alias variable is already bound in the WHERE clause by the
                                    // time ORDER BY is evaluated.  Expanding it here would cause
                                    // a second (and potentially incorrect) property-triple to be
                                    // emitted — in particular, relationship properties need
                                    // RDF-star reification which requires `edge_vars` context
                                    // that is only populated by `lower_projection_inner` (called
                                    // after this block).
                                    match underlying {
                                        Some(Expr::Property(..)) => &sk.expr,
                                        Some(other) => other,
                                        None => &sk.expr,
                                    }
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
            // Apply collect() arg BINDs inside the group body.
            // These must come before complex_group_binds so the collect proj
            // variables are in scope for any group-key expression that might
            // reference them (edge case, but safe to order first).
            for (var, expr) in std::mem::take(&mut self.collect_group_binds) {
                group_inner = GraphPattern::Extend {
                    inner: Box::new(group_inner),
                    variable: var,
                    expression: expr,
                };
            }
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
            // NOTE: do NOT add FILTER for variables used exclusively in collect()
            // aggregates: collect() ignores nulls naturally via GROUP_CONCAT's UNDEF
            // skip, and adding FILTER would cause GROUP BY () to return 0 rows on
            // all-null input (instead of 1 row with empty collect result).
            let collect_agg_vars: std::collections::HashSet<&str> = agg_items
                .iter()
                .filter(|ai| {
                    matches!(
                        &ai.expr,
                        Expr::Aggregate {
                            kind: AggKind::Collect,
                            ..
                        }
                    )
                })
                .filter_map(|ai| {
                    if let Expr::Aggregate { arg: Some(a), .. } = &ai.expr {
                        if let Expr::Variable { name, .. } = a.as_ref() {
                            return Some(name.as_str());
                        }
                    }
                    None
                })
                .collect();
            for null_var_name in &self.unwind_null_vars {
                // Only filter if this variable actually appears in the aggregation.
                let var_appears_in_non_collect_agg = agg_items.iter().any(|ai| {
                    if collect_agg_vars.contains(null_var_name.as_str()) {
                        return false; // this var is only used in collect(), skip filter
                    }
                    if let Expr::Aggregate { arg: Some(a), .. } = &ai.expr {
                        matches!(a.as_ref(), Expr::Variable { name, .. } if name == null_var_name)
                    } else {
                        false
                    }
                });
                if var_appears_in_non_collect_agg {
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
            // collect() aliases also get their projected_columns pushed here
            // (the Extend was already added in the collect_post_wraps loop above).
            let agg_alias_set: std::collections::HashSet<&str> =
                agg_items.iter().map(|a| a.alias.as_str()).collect();
            let mut extended = group_pattern;
            // Wrap collect() outputs in "[…]" and register their projected columns.
            for (alias, raw_gc_var) in std::mem::take(&mut self.collect_post_wraps) {
                let wrapped = SparExpr::FunctionCall(
                    Function::Concat,
                    vec![
                        SparExpr::Literal(SparLit::new_simple_literal("[")),
                        SparExpr::Coalesce(vec![
                            SparExpr::Variable(raw_gc_var),
                            SparExpr::Literal(SparLit::new_simple_literal("")),
                        ]),
                        SparExpr::Literal(SparLit::new_simple_literal("]")),
                    ],
                );
                let alias_var = Self::var(&alias);
                extended = GraphPattern::Extend {
                    inner: Box::new(extended),
                    variable: alias_var,
                    expression: wrapped,
                };
                self.projected_columns.push(scalar_col(alias));
            }
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
                        // Path variables have no SPARQL representation — projecting one
                        // directly (e.g. `RETURN p`) must fall back to legacy.
                        if self.path_lengths.contains_key(name.as_str()) {
                            return Err(PolygraphError::Unsupported {
                                construct: "path value in projection".into(),
                                spec_ref: "openCypher 9 §3.7".into(),
                                reason: format!(
                                    "path variable `{name}` cannot be projected as a SPARQL \
                                     variable; legacy fallback applies"
                                ),
                            });
                        }
                        self.projected_columns.push(scalar_col(name.clone()));
                        continue;
                    }
                }
                let sparql_expr = self.lower_expr(&pi.expr)?;
                // Const-int tracking: propagate compile-time integer bindings so that
                // range(const_var, ...) can be evaluated at compile time (like legacy's
                // const_int_vars mechanism).
                self.update_const_int_vars(&pi.alias, &pi.expr);
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

    /// Populate `const_int_vars` and `list_size_vars` from a projection item.
    /// Called after each non-GroupBy projection item to allow subsequent
    /// `range(const_var, ...)` calls to const-evaluate their arguments.
    fn update_const_int_vars(&mut self, alias: &str, expr: &crate::lqa::expr::Expr) {
        use crate::lqa::expr::{Expr as E, Literal};
        match expr {
            E::Literal(Literal::Integer(n)) => {
                self.const_int_vars.insert(alias.to_string(), *n);
            }
            E::Unary(crate::lqa::expr::UnaryOp::Neg, inner) => {
                if let E::Literal(Literal::Integer(n)) = inner.as_ref() {
                    self.const_int_vars.insert(alias.to_string(), -n);
                }
            }
            E::Variable { name, .. } => {
                // Passthrough alias: var retains const-int status if known.
                if let Some(n) = self.const_int_vars.get(name.as_str()).copied() {
                    self.const_int_vars.insert(alias.to_string(), n);
                }
                // Also propagate list-size status.
                if let Some(sz) = self.list_size_vars.get(name.as_str()).copied() {
                    self.list_size_vars.insert(alias.to_string(), sz);
                }
            }
            E::FunctionCall { name, args, .. } if name.eq_ignore_ascii_case("size") => {
                // size(literal_list_var) → const int
                if let Some(arg) = args.first() {
                    let sz_opt: Option<usize> = match arg {
                        E::List(items) => Some(items.len()),
                        E::Variable { name: v, .. } => self.list_size_vars.get(v.as_str()).copied(),
                        _ => None,
                    };
                    if let Some(sz) = sz_opt {
                        self.const_int_vars.insert(alias.to_string(), sz as i64);
                    }
                }
            }
            E::List(items) => {
                // Track list literals so `size(var)` can be const-evaluated later.
                self.list_size_vars.insert(alias.to_string(), items.len());
            }
            // Arithmetic over known const ints (e.g. `numOfValues - 1`).
            E::Sub(a, b) => {
                if let (Some(va), Some(vb)) = (self.eval_const_int(a), self.eval_const_int(b)) {
                    if let Some(result) = va.checked_sub(vb) {
                        self.const_int_vars.insert(alias.to_string(), result);
                    }
                }
            }
            E::Add(a, b) => {
                if let (Some(va), Some(vb)) = (self.eval_const_int(a), self.eval_const_int(b)) {
                    if let Some(result) = va.checked_add(vb) {
                        self.const_int_vars.insert(alias.to_string(), result);
                    }
                }
            }
            _ => {}
        }
    }

    /// Evaluate an expression to a compile-time integer using the Compiler's
    /// `const_int_vars` map in addition to the pure structural evaluator.
    fn eval_const_int(&self, expr: &crate::lqa::expr::Expr) -> Option<i64> {
        use crate::lqa::expr::{Expr as E, Literal};
        match expr {
            E::Literal(Literal::Integer(n)) => Some(*n),
            E::Unary(crate::lqa::expr::UnaryOp::Neg, e) => self.eval_const_int(e)?.checked_neg(),
            E::Unary(crate::lqa::expr::UnaryOp::Pos, e) => self.eval_const_int(e),
            E::Variable { name, .. } => self.const_int_vars.get(name.as_str()).copied(),
            E::Sub(a, b) => self.eval_const_int(a)?.checked_sub(self.eval_const_int(b)?),
            E::Add(a, b) => self.eval_const_int(a)?.checked_add(self.eval_const_int(b)?),
            E::Mul(a, b) => self.eval_const_int(a)?.checked_mul(self.eval_const_int(b)?),
            E::Div(a, b) => {
                let denom = self.eval_const_int(b)?;
                if denom == 0 {
                    return None;
                }
                Some(self.eval_const_int(a)? / denom)
            }
            E::Mod(a, b) => {
                let denom = self.eval_const_int(b)?;
                if denom == 0 {
                    return None;
                }
                Some(self.eval_const_int(a)? % denom)
            }
            _ => None,
        }
    }

    /// Evaluate range() args against both structural const-eval and the Compiler's
    /// `const_int_vars` map.  Returns `None` if any arg is non-constant.
    fn eval_range_args(&self, args: &[crate::lqa::expr::Expr]) -> Option<Vec<i64>> {
        let start = self.eval_const_int(args.first()?)?;
        let end_val = self.eval_const_int(args.get(1)?)?;
        let step: i64 = if let Some(step_arg) = args.get(2) {
            let s = self.eval_const_int(step_arg)?;
            if s == 0 {
                return None; // step=0 is invalid; let legacy raise the error
            }
            s
        } else {
            1
        };
        let mut items = Vec::new();
        let mut i = start;
        while (step > 0 && i <= end_val) || (step < 0 && i >= end_val) {
            items.push(i);
            i += step;
            if items.len() > 100_000 {
                return None; // too large; let legacy handle
            }
        }
        Some(items)
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
                        // Path variable: count(p) / count(distinct p) → COUNT(*).
                        // A path value has no SPARQL representation; counting paths
                        // is equivalent to counting distinct solution rows.
                        if let Expr::Variable { name: pv, .. } = arg_expr.as_ref() {
                            if self.path_lengths.contains_key(pv.as_str()) {
                                AggregateExpression::CountSolutions {
                                    distinct: *distinct,
                                }
                            } else {
                                AggregateExpression::FunctionCall {
                                    name: AggregateFunction::Count,
                                    expr: self.lower_expr(arg_expr)?,
                                    distinct: *distinct,
                                }
                            }
                        } else {
                            AggregateExpression::FunctionCall {
                                name: AggregateFunction::Count,
                                expr: self.lower_expr(arg_expr)?,
                                distinct: *distinct,
                            }
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
                    // collect() → SPARQL GROUP_CONCAT with Cypher list serialisation.
                    // Values are encoded as: number/boolean → STR(?v),
                    // string → CONCAT("'", STR(?v), "'"), null/UNDEF → "null".
                    // The raw GROUP_CONCAT output is wrapped in "[" … "]" via a
                    // post-GROUP Extend (stored in self.collect_post_wraps).
                    let arg_expr = match arg {
                        Some(a) => a.as_ref(),
                        None => {
                            return Err(PolygraphError::Translation {
                                message: "collect() requires an argument".into(),
                            })
                        }
                    };
                    let opt_before_inner = self.pending_optional_triples.len();
                    let arg_spar = self.lower_expr(arg_expr)?;
                    // If lower_expr added optional triples, the arg involves a nullable
                    // variable (e.g. collect(n.prop) where n comes from OPTIONAL MATCH).
                    // This creates incorrect cross-join semantics in a single GROUP BY;
                    // fall back to legacy for correct handling.
                    if self.pending_optional_triples.len() > opt_before_inner {
                        self.pending_optional_triples.drain(opt_before_inner..);
                        return Err(PolygraphError::Unsupported {
                            construct: "collect() aggregate".into(),
                            spec_ref: "openCypher 9 §3.4.6".into(),
                            reason: "collect() on nullable (OPTIONAL MATCH) property requires legacy path".into(),
                        });
                    }
                    // For node/relationship variables, fall back to legacy: we cannot
                    // serialise graph elements to the "[...]" list string format here.
                    if let SparExpr::Variable(ref v) = arg_spar {
                        if self.scan_vars.contains(v.as_str()) {
                            return Err(PolygraphError::Unsupported {
                                construct: "collect() aggregate".into(),
                                spec_ref: "openCypher 9 §3.4.6".into(),
                                reason:
                                    "collect() on node/relationship variable requires legacy path"
                                        .into(),
                            });
                        }
                    }
                    // Bind the arg expression to a fresh variable inside the GROUP body.
                    let proj_var = self.fresh("gc_proj");
                    self.collect_group_binds.push((proj_var.clone(), arg_spar));
                    let v = SparExpr::Variable(proj_var.clone());
                    // Value encoding: numbers/booleans → STR, strings → quoted.
                    // No BOUND check: openCypher's collect() ignores null values, and
                    // GROUP_CONCAT naturally skips UNDEF rows, so null values are
                    // excluded from the collected list automatically.
                    let dt = SparExpr::FunctionCall(Function::Datatype, vec![v.clone()]);
                    let mk_nn = |iri: &str| SparExpr::NamedNode(NamedNode::new_unchecked(iri));
                    let is_num_or_bool = SparExpr::And(
                        Box::new(SparExpr::FunctionCall(Function::IsLiteral, vec![v.clone()])),
                        Box::new(SparExpr::Or(
                            Box::new(SparExpr::Or(
                                Box::new(SparExpr::Equal(
                                    Box::new(dt.clone()),
                                    Box::new(mk_nn(XSD_INTEGER)),
                                )),
                                Box::new(SparExpr::Equal(
                                    Box::new(dt.clone()),
                                    Box::new(mk_nn(XSD_DOUBLE)),
                                )),
                            )),
                            Box::new(SparExpr::Or(
                                Box::new(SparExpr::Equal(
                                    Box::new(dt.clone()),
                                    Box::new(mk_nn(XSD_BOOLEAN)),
                                )),
                                Box::new(SparExpr::Or(
                                    Box::new(SparExpr::Equal(
                                        Box::new(dt.clone()),
                                        Box::new(mk_nn("http://www.w3.org/2001/XMLSchema#long")),
                                    )),
                                    Box::new(SparExpr::Or(
                                        Box::new(SparExpr::Equal(
                                            Box::new(dt.clone()),
                                            Box::new(mk_nn(
                                                "http://www.w3.org/2001/XMLSchema#decimal",
                                            )),
                                        )),
                                        Box::new(SparExpr::Equal(
                                            Box::new(dt),
                                            Box::new(mk_nn(
                                                "http://www.w3.org/2001/XMLSchema#float",
                                            )),
                                        )),
                                    )),
                                )),
                            )),
                        )),
                    );
                    let enc = SparExpr::If(
                        Box::new(is_num_or_bool),
                        Box::new(SparExpr::FunctionCall(Function::Str, vec![v.clone()])),
                        Box::new(SparExpr::FunctionCall(
                            Function::Concat,
                            vec![
                                SparExpr::Literal(SparLit::new_simple_literal("'")),
                                SparExpr::FunctionCall(Function::Str, vec![v]),
                                SparExpr::Literal(SparLit::new_simple_literal("'")),
                            ],
                        )),
                    );
                    let raw_gc_var = self.fresh("gc_raw");
                    self.collect_post_wraps
                        .push((ai.alias.clone(), raw_gc_var.clone()));
                    let gc_agg = AggregateExpression::FunctionCall {
                        name: AggregateFunction::GroupConcat {
                            separator: Some(", ".to_string()),
                        },
                        expr: enc,
                        distinct: *distinct,
                    };
                    // Return raw_gc_var (not out_var/alias) so the GROUP pattern binds
                    // the raw separated string.  The alias gets the "[…]" wrapper via
                    // collect_post_wraps applied after the GROUP pattern is built.
                    return Ok((raw_gc_var, gc_agg));
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
                path_var,
            } => {
                self.scan_vars.insert(from.clone());
                self.scan_vars.insert(to.clone());

                // For fixed-hop (non-varlen) named paths, record the static hop count.
                // Each Expand in a chained multi-hop pattern increments the counter, so
                // after processing the whole chain, path_lengths[pvar] == total hops.
                // For varlen named paths, record usize::MAX as a sentinel so the path
                // variable is still tracked (and will fail correctly if used as a value).
                // Use saturating_add to guard against edge cases where a mixed
                // fixed+varlen path would otherwise overflow the MAX sentinel.
                if let Some(pvar) = path_var.as_deref() {
                    if range.is_none() {
                        let counter = self.path_lengths.entry(pvar.to_string()).or_insert(0);
                        *counter = counter.saturating_add(1);
                    } else {
                        // Varlen: mark as dynamic (usize::MAX sentinel).
                        self.path_lengths
                            .entry(pvar.to_string())
                            .or_insert(usize::MAX);
                    }
                }

                let inner_pat = self.lower_op(inner)?;

                // For fixed-length named paths, maintain an ordered list of node variables
                // so that `nodes(p)` can emit them at query-compile time.
                // The inner op is processed first (recursive), so when we arrive here the
                // inner Expand (if any) has already prepended its `from` variable.
                // Strategy: innermost Expand seeds the vec with [from, to]; each outer
                // Expand then prepends its own `from` (its `to` is already the first
                // element contributed by the inner step).
                if let Some(pvar) = path_var.as_deref() {
                    if range.is_none() {
                        let from_var = Self::var(from.as_str());
                        let to_var = Self::var(to.as_str());
                        match self.path_node_vars.get_mut(pvar) {
                            Some(nodes) => {
                                // Outer expand: prepend our `from` (inner already owns `to`).
                                nodes.insert(0, from_var);
                            }
                            None => {
                                // Innermost expand: seed with [from, to].
                                self.path_node_vars
                                    .insert(pvar.to_string(), vec![from_var, to_var]);
                            }
                        }
                    }
                }

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

                    // Cross-WITH re-use: if this rel-var was already registered by a
                    // prior Op::Expand (from an earlier MATCH before a WITH clause),
                    // ?rv is already in scope in the flat WHERE block.  Attempting to
                    // BIND it again would raise a SPARQL "cannot bind an already-bound
                    // variable" error.  Instead, emit only the constraint triple and
                    // skip the BIND and uniqueness filter (same edge ≡ same constraint).
                    if self.edge_vars.contains_key(rv.as_str()) {
                        let edge_bgp =
                            self.lower_expand_relvar_reuse(from, to, rel_types, direction, rv);
                        return Ok(join(inner_pat, edge_bgp));
                    }

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
                    let (gp, group_vars) = self.lower_projection_inner(inner, items)?;
                    let flushed = self.flush_pending(gp);
                    // Compute passthrough node aliases AFTER lower_projection_inner so
                    // that scan_vars has been populated by the inner MATCH processing.
                    // (Before the inner runs, scan_vars may be empty for the first
                    // WITH-aggregate in a query chain, causing node passthrough vars to
                    // be incorrectly inserted into scalar_vars.)
                    let passthrough_node_aliases: std::collections::HashSet<&str> = items
                        .iter()
                        .filter_map(|pi| {
                            if let Expr::Variable { name, .. } = &pi.expr {
                                if *name == pi.alias
                                    && (self.scan_vars.contains(name.as_str())
                                        || group_vars.iter().any(|v| v.as_str() == name.as_str()))
                                {
                                    return Some(pi.alias.as_str());
                                }
                            }
                            None
                        })
                        .collect();
                    for pi in items {
                        if pi.alias != "*" && !passthrough_node_aliases.contains(pi.alias.as_str())
                        {
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
                            // Capture the literal value BEFORE e is moved into Extend.
                            // This covers temporal constructors (date/datetime/duration/…)
                            // that lower_expr computes into a typed SPARQL literal at
                            // compile time but whose AST expression type is FunctionCall,
                            // not Literal — so the `pi.expr` match below would miss them.
                            let computed_lit_val: Option<String> =
                                if let SparExpr::Literal(lit) = &e {
                                    Some(lit.value().to_string())
                                } else {
                                    None
                                };
                            gp = self.flush_pending(gp);
                            gp = GraphPattern::Extend {
                                inner: Box::new(gp),
                                variable: Self::var(&pi.alias),
                                expression: e,
                            };
                            // Mark as scalar only when the source is itself scalar (or the
                            // expression is not a simple variable rename). Node variables
                            // passed through WITH as a rename (e.g. `WITH n AS a`) must NOT
                            // be added to scalar_vars — they remain node variables and need
                            // triple-based property access.
                            let is_node_rename = matches!(
                                &pi.expr,
                                Expr::Variable { name, .. } if !self.scalar_vars.contains(name.as_str())
                            );
                            if !is_node_rename {
                                self.scalar_vars.insert(pi.alias.clone());
                            }
                            // Store map literal pairs for compile-time property access
                            // (e.g. `WITH {k: v} AS m RETURN m.k`).
                            // Also handle null scalar: `WITH null AS m` — any property
                            // access on null returns null, so register empty pairs.
                            match &pi.expr {
                                Expr::Map(pairs) => {
                                    self.scalar_map_exprs
                                        .insert(pi.alias.clone(), pairs.clone());
                                }
                                Expr::Literal(Literal::Null) => {
                                    self.scalar_map_exprs.insert(pi.alias.clone(), vec![]);
                                    // null scalar is always nullable
                                    self.nullable.insert(pi.alias.clone());
                                }
                                _ => {}
                            }
                            // Track temporal-typed variables for date/time arithmetic.
                            if let Expr::Literal(Literal::TypedLiteral(_, xsd_type)) = &pi.expr {
                                if !xsd_type.is_empty() {
                                    self.temporal_type_vars
                                        .insert(pi.alias.clone(), xsd_type.clone());
                                }
                            }
                            // Track scalar literal values for temporal/duration property access.
                            // Primary source: direct AST literals.
                            match &pi.expr {
                                Expr::Literal(Literal::TypedLiteral(val, _)) => {
                                    self.scalar_lit_vals.insert(pi.alias.clone(), val.clone());
                                }
                                Expr::Literal(Literal::String(val)) => {
                                    self.scalar_lit_vals.insert(pi.alias.clone(), val.clone());
                                }
                                _ => {}
                            }
                            // Secondary source: value computed by lower_expr (e.g. temporal
                            // constructors that produce a typed literal at compile time).
                            if let Some(val) = computed_lit_val {
                                self.scalar_lit_vals.entry(pi.alias.clone()).or_insert(val);
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
                    // Track the UNWIND variable so projection collision detection
                    // (RETURN expr AS same_alias) can issue a subquery-isolation path
                    // instead of the Oxigraph "SELECT overrides existing variable" error.
                    self.scan_vars.insert(variable.clone());
                    let values = GraphPattern::Values {
                        variables: vec![var],
                        bindings,
                    };
                    Ok(join(inner_pat, values))
                } else if let Expr::FunctionCall { name, args, .. } = list {
                    // UNWIND range(start, end [, step]) AS var — expand at compile time.
                    if name.eq_ignore_ascii_case("range") {
                        if let Some(items) = eval_range_to_integers(args) {
                            let inner_pat = self.lower_op(inner)?;
                            let var = Self::var(variable);
                            if items.is_empty() {
                                return Ok(inner_pat);
                            }
                            let bindings: Vec<Vec<Option<spargebra::term::GroundTerm>>> = items
                                .iter()
                                .map(|n| {
                                    vec![Some(spargebra::term::GroundTerm::Literal(
                                        SparLit::new_typed_literal(
                                            n.to_string(),
                                            NamedNode::new_unchecked(XSD_INTEGER),
                                        ),
                                    ))]
                                })
                                .collect();
                            self.scan_vars.insert(variable.clone());
                            let values = GraphPattern::Values {
                                variables: vec![var],
                                bindings,
                            };
                            return Ok(join(inner_pat, values));
                        }
                    }
                    // UNWIND keys(n) AS x  /  UNWIND keys(r) AS x
                    // Expand one row per property key by querying the RDF graph
                    // for node predicates or edge-reifier predicates in the base
                    // namespace, then stripping the base prefix with SUBSTR.
                    if name.eq_ignore_ascii_case("keys") && args.len() == 1 {
                        if let Some(Expr::Variable { name: var_name, .. }) = args.first() {
                            let inner_pat = self.lower_op(inner)?;
                            let keys_var = Self::var(variable);
                            let pred_v = self.fresh("_keys_pred");
                            let val_v = self.fresh("_keys_val");
                            let base = self.base_iri.clone();
                            let base_len = base.len();
                            let is_nullable = self.nullable.contains(var_name.as_str());

                            let make_key_expr = |pv: Variable| -> SparExpr {
                                SparExpr::FunctionCall(
                                    Function::SubStr,
                                    vec![
                                        SparExpr::FunctionCall(
                                            Function::Str,
                                            vec![SparExpr::Variable(pv)],
                                        ),
                                        SparExpr::Literal(SparLit::new_typed_literal(
                                            (base_len + 1).to_string(),
                                            NamedNode::new_unchecked(XSD_INTEGER),
                                        )),
                                    ],
                                )
                            };

                            if let Some(edge_info) = self.edge_vars.get(var_name.as_str()).cloned()
                            {
                                // Edge variable: expand via RDF-star reification.
                                // ?_keys_reif rdf:reifies <<subj pred obj>> .
                                // ?_keys_reif ?_keys_pred ?_keys_val .
                                // FILTER(STRSTARTS(STR(?_keys_pred), base) [&& BOUND(?r)])
                                let reif_var = self.fresh("_keys_reif");
                                let rdf_reifies = NamedNode::new_unchecked(
                                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#reifies",
                                );
                                let subj_tp = TermPattern::Variable(Self::var(&edge_info.subj));
                                let obj_tp = TermPattern::Variable(Self::var(&edge_info.obj));
                                let pred_pat = match &edge_info.pred {
                                    EdgePred::Static(iri) => {
                                        NamedNodePattern::NamedNode(iri.clone())
                                    }
                                    EdgePred::Dynamic(v) => NamedNodePattern::Variable(v.clone()),
                                };
                                let edge_triple_tp = TermPattern::Triple(Box::new(TriplePattern {
                                    subject: subj_tp,
                                    predicate: pred_pat,
                                    object: obj_tp,
                                }));
                                let bgp = GraphPattern::Bgp {
                                    patterns: vec![
                                        TriplePattern {
                                            subject: TermPattern::Variable(reif_var.clone()),
                                            predicate: NamedNodePattern::NamedNode(rdf_reifies),
                                            object: edge_triple_tp,
                                        },
                                        TriplePattern {
                                            subject: TermPattern::Variable(reif_var),
                                            predicate: NamedNodePattern::Variable(pred_v.clone()),
                                            object: TermPattern::Variable(val_v),
                                        },
                                    ],
                                };
                                let base_lit = SparExpr::Literal(SparLit::new_simple_literal(base));
                                let str_pred = SparExpr::FunctionCall(
                                    Function::Str,
                                    vec![SparExpr::Variable(pred_v.clone())],
                                );
                                let strstarts = SparExpr::FunctionCall(
                                    Function::StrStarts,
                                    vec![str_pred, base_lit],
                                );
                                let filter_expr = if is_nullable {
                                    SparExpr::And(
                                        Box::new(SparExpr::Bound(Self::var(var_name))),
                                        Box::new(strstarts),
                                    )
                                } else {
                                    strstarts
                                };
                                let filtered = GraphPattern::Filter {
                                    expr: filter_expr,
                                    inner: Box::new(bgp),
                                };
                                let extended = GraphPattern::Extend {
                                    inner: Box::new(filtered),
                                    variable: keys_var,
                                    expression: make_key_expr(pred_v),
                                };
                                self.scan_vars.insert(variable.clone());
                                return Ok(join(inner_pat, extended));
                            }

                            if self.scan_vars.contains(var_name.as_str()) {
                                // Node variable: expand via subject-wildcard BGP.
                                // ?n ?_keys_pred ?_keys_val
                                // FILTER(STRSTARTS(STR(?pred),base)
                                //        && STR(?pred)!=base+"__node"
                                //        && STR(?pred)!=rdf:type
                                //        [&& BOUND(?n)])
                                let node_v = Self::var(var_name);
                                let sentinel_str = format!("{base}__node");
                                let bgp = GraphPattern::Bgp {
                                    patterns: vec![TriplePattern {
                                        subject: TermPattern::Variable(node_v.clone()),
                                        predicate: NamedNodePattern::Variable(pred_v.clone()),
                                        object: TermPattern::Variable(val_v),
                                    }],
                                };
                                let base_lit = SparExpr::Literal(SparLit::new_simple_literal(base));
                                let rdf_type_lit = SparExpr::Literal(SparLit::new_simple_literal(
                                    RDF_TYPE.to_string(),
                                ));
                                let sentinel_lit =
                                    SparExpr::Literal(SparLit::new_simple_literal(sentinel_str));
                                let str_pred = SparExpr::FunctionCall(
                                    Function::Str,
                                    vec![SparExpr::Variable(pred_v.clone())],
                                );
                                let strstarts = SparExpr::FunctionCall(
                                    Function::StrStarts,
                                    vec![str_pred.clone(), base_lit],
                                );
                                let not_sentinel = SparExpr::Not(Box::new(SparExpr::Equal(
                                    Box::new(str_pred.clone()),
                                    Box::new(sentinel_lit),
                                )));
                                let not_type = SparExpr::Not(Box::new(SparExpr::Equal(
                                    Box::new(str_pred),
                                    Box::new(rdf_type_lit),
                                )));
                                let props_filter = SparExpr::And(
                                    Box::new(strstarts),
                                    Box::new(SparExpr::And(
                                        Box::new(not_sentinel),
                                        Box::new(not_type),
                                    )),
                                );
                                let filter_expr = if is_nullable {
                                    SparExpr::And(
                                        Box::new(SparExpr::Bound(node_v)),
                                        Box::new(props_filter),
                                    )
                                } else {
                                    props_filter
                                };
                                let filtered = GraphPattern::Filter {
                                    expr: filter_expr,
                                    inner: Box::new(bgp),
                                };
                                let extended = GraphPattern::Extend {
                                    inner: Box::new(filtered),
                                    variable: keys_var,
                                    expression: make_key_expr(pred_v),
                                };
                                self.scan_vars.insert(variable.clone());
                                return Ok(join(inner_pat, extended));
                            }
                        }
                    }
                    Err(PolygraphError::Unsupported {
                        construct: "UNWIND with variable/expression list in LQA path".into(),
                        spec_ref: "openCypher 9 §4.5".into(),
                        reason: "runtime list UNWIND requires legacy path".into(),
                    })
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

    /// Emit a constraint-only triple for a relationship variable that was already
    /// bound in a prior MATCH clause and is being reused after a WITH boundary.
    ///
    /// In this case `?rv` is already bound in the SPARQL scope and must NOT be
    /// re-BIND'd (SPARQL engines reject re-binding a variable already in scope).
    /// We emit only the triple that constrains the match, relying on SPARQL's
    /// natural-join semantics to enforce the existing binding.
    ///
    /// - Typed edges:  emit `?from <base:T> ?to` with the static IRI.  `?rv` was
    ///   bound to `"base:T"^^xsd:anyURI` (a literal marker), which is consistent.
    /// - Untyped edges: `?rv` was bound to the *actual predicate IRI* (not a
    ///   literal) by the first MATCH, so it can be used directly as the predicate
    ///   variable in the triple `?from ?rv ?to`.
    /// - Multi-type:   emit a UNION of one triple per type without any BIND.
    ///
    /// `edge_vars` is NOT updated — the original registration is preserved.
    fn lower_expand_relvar_reuse(
        &mut self,
        from: &str,
        to: &str,
        rel_types: &[String],
        direction: &Direction,
        rv: &str,
    ) -> GraphPattern {
        let from_tp = TermPattern::Variable(Self::var(from));
        let to_tp = TermPattern::Variable(Self::var(to));

        if rel_types.is_empty() {
            // Untyped: ?rv IS a predicate IRI (bound from the first MATCH's triple).
            // Use it directly in predicate position — SPARQL join will constrain.
            let rv_var = Self::var(rv);
            self.lower_expand_any_type(from_tp, rv_var, to_tp, direction)
        } else if rel_types.len() == 1 {
            let iri = NamedNode::new_unchecked(format!("{}{}", self.base_iri, &rel_types[0]));
            let pred = NamedNodePattern::NamedNode(iri);
            self.lower_expand_typed(from_tp, pred, to_tp, direction)
        } else {
            let pats: Vec<GraphPattern> = rel_types
                .iter()
                .map(|rt| {
                    let iri = NamedNode::new_unchecked(format!("{}{}", self.base_iri, rt));
                    let pred = NamedNodePattern::NamedNode(iri);
                    self.lower_expand_typed(from_tp.clone(), pred, to_tp.clone(), direction)
                })
                .collect();
            pats.into_iter()
                .reduce(|a, b| GraphPattern::Union {
                    left: Box::new(a),
                    right: Box::new(b),
                })
                .unwrap_or(GraphPattern::Bgp { patterns: vec![] })
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
            Expr::Variable { name, .. } => {
                // Path variables exist only as hop-count metadata; they have no
                // SPARQL value representation. Direct use outside of length()/size()
                // must fall back to the legacy translator.
                if self.path_lengths.contains_key(name.as_str()) {
                    return Err(PolygraphError::Unsupported {
                        construct: "path value".into(),
                        spec_ref: "openCypher 9 §3.7".into(),
                        reason: format!(
                            "path variable `{name}` cannot be used as a direct value; \
                             only length()/ size() is supported in the LQA path"
                        ),
                    });
                }
                Ok(SparExpr::Variable(Self::var(name)))
            }

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

            // ── Subscript access: expr[key] ───────────────────────────────────────
            // When the key is a compile-time string, treat as property access.
            // When both base and key are literals (constant-folding), evaluate directly.
            Expr::Subscript(base, key) => {
                // Resolve scalar-literal variable keys (e.g. `WITH 'x' AS idx; map[idx]`).
                let resolved_key;
                let key = if let Expr::Variable { name, .. } = key.as_ref() {
                    if let Some(s) = self.scalar_lit_vals.get(name.as_str()).cloned() {
                        resolved_key = Expr::Literal(Literal::String(s));
                        &resolved_key
                    } else {
                        key.as_ref()
                    }
                } else {
                    key.as_ref()
                };
                // Constant-fold: Map[string_key] → look up key in the map.
                if let Expr::Map(pairs) = base.as_ref() {
                    if let Some(key_str) = lqa_eval_string_expr(key) {
                        if let Some((_, val_expr)) =
                            pairs.iter().find(|(k, _)| k.as_str() == key_str)
                        {
                            return self.lower_expr(val_expr);
                        }
                        // Key not found in literal map → null.
                        let null_var = self.fresh("null");
                        return Ok(SparExpr::Variable(null_var));
                    }
                }
                // Dynamic string key on a variable → treat as property access.
                // Also handles scalar_map_exprs variables via try_get_map_pairs.
                if let Some(key_str) = lqa_eval_string_expr(key) {
                    // Delegate to the Property handler by synthesising the expression.
                    return self.lower_expr(&Expr::Property(base.clone(), key_str));
                }
                // Constant integer index on a literal list → already folded in lower.rs.
                // Any remaining Subscript is runtime-dynamic → legacy.
                Err(PolygraphError::Unsupported {
                    construct: "expression type Subscript in LQA SPARQL lowering".into(),
                    spec_ref: "openCypher 9 §6".into(),
                    reason: "dynamic subscript with non-constant key requires legacy path".into(),
                })
            }

            Expr::Property(base, key) => {
                // Constant-fold: literal temporal value accessed via property key.
                // This handles e.g. `date({year: 1984}).year` where the date constructor
                // was folded to a TypedLiteral at lower time.
                if let Expr::Literal(lit) = base.as_ref() {
                    if let Some(val_str) = lqa_literal_str_value(lit) {
                        if let Some(spar) = lqa_scalar_temporal_prop(&val_str, key) {
                            return Ok(spar);
                        }
                    }
                }
                // Constant-fold: Map literal property access {k: v}.key → v
                if let Expr::Map(pairs) = base.as_ref() {
                    if let Some((_, val_expr)) =
                        pairs.iter().find(|(k, _)| k.as_str() == key.as_str())
                    {
                        return self.lower_expr(val_expr);
                    }
                    // Key not in map → null.
                    let null_var = self.fresh("null");
                    return Ok(SparExpr::Variable(null_var));
                }
                // Constant-fold: property access through a variable bound to a map literal,
                // or through a chain of such accesses (e.g. `WITH {a:{b:1}} AS m RETURN m.a.b`).
                // `try_get_map_pairs` resolves Variable / Property chains stored in
                // `scalar_map_exprs`.
                if let Some(pairs) = self.try_get_map_pairs(base) {
                    return match pairs.into_iter().find(|(k, _)| k.as_str() == key.as_str()) {
                        Some((_, val)) => self.lower_expr(&val),
                        None => {
                            let v = self.fresh("null");
                            Ok(SparExpr::Variable(v))
                        }
                    };
                }
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
                // Special case 1: temporal or duration literal with a known compile-time
                // value — extract the property at compile time.
                // Special case 2: temporal variable with unknown runtime value — emit a
                // SPARQL temporal function call (YEAR(?d), MONTH(?d), etc.).
                if let Expr::Variable { name, .. } = base.as_ref() {
                    if self.scalar_vars.contains(name) {
                        // Try compile-time extraction first.
                        if let Some(lit_val) = self.scalar_lit_vals.get(name.as_str()).cloned() {
                            let extracted = lqa_scalar_temporal_prop(&lit_val, key);
                            if let Some(spar) = extracted {
                                return Ok(spar);
                            }
                        }
                        // Try runtime SPARQL temporal function (YEAR(?d), MONTH(?d), etc.).
                        let var_expr = SparExpr::Variable(Self::var(name));
                        if let Some(spar) = lqa_temporal_component_fn(key, var_expr) {
                            return Ok(spar);
                        }
                        // Try JDN-based runtime computation (week, weekYear, weekDay,
                        // ordinalDay, dayOfQuarter) via BIND chain.
                        if let Some(spar) = self.lower_temporal_jdn_property(name, key) {
                            return Ok(spar);
                        }
                        return Err(PolygraphError::Unsupported {
                            construct: format!(
                                "property access on scalar variable .{key} (var={name})"
                            ),
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
                    // When the base expression folded to a literal (e.g., a temporal
                    // constructor call that wasn't folded earlier), try to extract the
                    // temporal component directly from the literal value.
                    SparExpr::Literal(lit) => {
                        if let Some(val_str) = Some(lit.value()) {
                            if let Some(spar) = lqa_scalar_temporal_prop(val_str, key) {
                                return Ok(spar);
                            }
                        }
                        return Err(PolygraphError::Unsupported {
                            construct: "property access on non-variable expression".into(),
                            spec_ref: "openCypher 9 §6.1".into(),
                            reason: "LQA path only supports property access on variables".into(),
                        });
                    }
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

                // List concatenation: [a, b] + [c] → [a, b, c], [a] + scalar → [a, scalar].
                // Merge element vectors and try to constant-fold.  Fall back to legacy
                // for dynamic operands (runtime list values cannot be folded here).
                let a_is_list = matches!(a.as_ref(), Expr::List(_));
                let b_is_list = matches!(b.as_ref(), Expr::List(_));
                if a_is_list || b_is_list {
                    let mut combined: Vec<Expr> = match a.as_ref() {
                        Expr::List(items) => items.clone(),
                        other => vec![other.clone()],
                    };
                    match b.as_ref() {
                        Expr::List(items) => combined.extend(items.iter().cloned()),
                        other => combined.push(other.clone()),
                    }
                    let merged = Expr::List(combined);
                    if let Some(s) = lqa_serialize_literal(&merged) {
                        return Ok(Self::lit_str(&s));
                    }
                    // Dynamic operands — fall back to legacy for correct semantics.
                    return Err(PolygraphError::Unsupported {
                        construct: "list concatenation with dynamic operands in LQA SPARQL lowering".into(),
                        spec_ref: "openCypher 9 §6.5".into(),
                        reason: "list + operator with non-constant operands not yet handled; legacy fallback applies".into(),
                    });
                }

                // Temporal arithmetic: date/time + duration needs special SPARQL.
                if let Expr::Variable { name, .. } = a.as_ref() {
                    if let Some(xsd_type) = self.temporal_type_vars.get(name.as_str()).cloned() {
                        let la = self.lower_expr(a)?;
                        let lb = self.lower_expr(b)?;
                        let is_date = xsd_type.as_str() == XSD_DATE;
                        return Ok(crate::translator::cypher::temporal_add_sparql(
                            la, lb, is_date,
                        ));
                    }
                }

                let la = self.lower_expr(a)?;
                let lb = self.lower_expr(b)?;
                if lqa_expr_is_string(a) || lqa_expr_is_string(b) {
                    Ok(SparExpr::FunctionCall(Function::Concat, vec![la, lb]))
                } else if matches!(a.as_ref(), Expr::Property(..))
                    && matches!(b.as_ref(), Expr::Property(..))
                {
                    // Runtime dispatch: list OR duration OR numeric addition.
                    let str_a = SparExpr::FunctionCall(Function::Str, vec![la.clone()]);
                    let is_list = SparExpr::FunctionCall(
                        Function::StrStarts,
                        vec![
                            str_a.clone(),
                            SparExpr::Literal(SparLit::new_simple_literal("[")),
                        ],
                    );
                    let is_dur = SparExpr::FunctionCall(
                        Function::StrStarts,
                        vec![
                            str_a.clone(),
                            SparExpr::Literal(SparLit::new_simple_literal("P")),
                        ],
                    );
                    // List concat: trim trailing ']' of a, leading '[' of b, join with ", ".
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
                    let list_concat =
                        SparExpr::FunctionCall(Function::Concat, vec![head, sep, tail]);
                    // Duration add: urn:polygraph:duration-add(STR(?a), STR(?b))
                    let str_b = SparExpr::FunctionCall(Function::Str, vec![lb.clone()]);
                    let dur_add = SparExpr::FunctionCall(
                        Function::Custom(NamedNode::new_unchecked("urn:polygraph:duration-add")),
                        vec![str_a, str_b],
                    );
                    let numeric_add = SparExpr::Add(Box::new(la), Box::new(lb));
                    Ok(SparExpr::If(
                        Box::new(is_list),
                        Box::new(list_concat),
                        Box::new(SparExpr::If(
                            Box::new(is_dur),
                            Box::new(dur_add),
                            Box::new(numeric_add),
                        )),
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
                        return Ok(crate::translator::cypher::temporal_subtract_sparql(
                            la, rb, is_date,
                        ));
                    }
                }
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                // Duration-dispatch for property - property subtraction.
                if matches!(a.as_ref(), Expr::Property(..))
                    && matches!(b.as_ref(), Expr::Property(..))
                {
                    let str_a = SparExpr::FunctionCall(Function::Str, vec![la.clone()]);
                    let is_dur = SparExpr::FunctionCall(
                        Function::StrStarts,
                        vec![
                            str_a.clone(),
                            SparExpr::Literal(SparLit::new_simple_literal("P")),
                        ],
                    );
                    let str_b = SparExpr::FunctionCall(Function::Str, vec![rb.clone()]);
                    let dur_sub = SparExpr::FunctionCall(
                        Function::Custom(NamedNode::new_unchecked("urn:polygraph:duration-sub")),
                        vec![str_a, str_b],
                    );
                    let numeric_sub = SparExpr::Subtract(Box::new(la), Box::new(rb));
                    return Ok(SparExpr::If(
                        Box::new(is_dur),
                        Box::new(dur_sub),
                        Box::new(numeric_sub),
                    ));
                }
                if let Some(folded) = fold_numeric_binop('-', &la, &rb) {
                    return Ok(folded);
                }
                Ok(SparExpr::Subtract(Box::new(la), Box::new(rb)))
            }
            Expr::Mul(a, b) => {
                let la = self.lower_expr(a)?;
                let rb = self.lower_expr(b)?;
                // Duration * number dispatch when the LHS is a property access.
                if matches!(a.as_ref(), Expr::Property(..)) {
                    let str_a = SparExpr::FunctionCall(Function::Str, vec![la.clone()]);
                    let is_dur = SparExpr::FunctionCall(
                        Function::StrStarts,
                        vec![
                            str_a.clone(),
                            SparExpr::Literal(SparLit::new_simple_literal("P")),
                        ],
                    );
                    let str_b = SparExpr::FunctionCall(Function::Str, vec![rb.clone()]);
                    let dur_mul = SparExpr::FunctionCall(
                        Function::Custom(NamedNode::new_unchecked(
                            "urn:polygraph:duration-mul-num",
                        )),
                        vec![str_a, str_b],
                    );
                    let numeric_mul = SparExpr::Multiply(Box::new(la), Box::new(rb));
                    return Ok(SparExpr::If(
                        Box::new(is_dur),
                        Box::new(dur_mul),
                        Box::new(numeric_mul),
                    ));
                }
                if let Some(folded) = fold_numeric_binop('*', &la, &rb) {
                    return Ok(folded);
                }
                Ok(SparExpr::Multiply(Box::new(la), Box::new(rb)))
            }
            Expr::Div(a, b) => {
                // Duration / number dispatch when the LHS is a property access.
                if matches!(a.as_ref(), Expr::Property(..)) {
                    let la = self.lower_expr(a)?;
                    let rb = self.lower_expr(b)?;
                    let str_a = SparExpr::FunctionCall(Function::Str, vec![la.clone()]);
                    let is_dur = SparExpr::FunctionCall(
                        Function::StrStarts,
                        vec![
                            str_a.clone(),
                            SparExpr::Literal(SparLit::new_simple_literal("P")),
                        ],
                    );
                    let str_b = SparExpr::FunctionCall(Function::Str, vec![rb.clone()]);
                    let dur_div = SparExpr::FunctionCall(
                        Function::Custom(NamedNode::new_unchecked(
                            "urn:polygraph:duration-div-num",
                        )),
                        vec![str_a, str_b],
                    );
                    let numeric_div = SparExpr::Divide(Box::new(la), Box::new(rb));
                    return Ok(SparExpr::If(
                        Box::new(is_dur),
                        Box::new(dur_div),
                        Box::new(numeric_div),
                    ));
                }
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
                // Try full recursive constant evaluation first (handles cases like
                // `4 ^ (3 * 2) ^ 3` and `(-3) ^ 2` that aren't bare integer literals).
                if let Some(result) = const_eval_numeric(base)
                    .and_then(|b| const_eval_numeric(exp).map(|e| b.powf(e)))
                {
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
                // Guard: list ordering via string comparison gives semantically wrong results
                // (Cypher list ordering is element-wise and typed, not lexicographic on the
                // serialised string).  Return Err so legacy handles these cases.
                if matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge)
                    && (matches!(a.as_ref(), Expr::List(_)) || matches!(b.as_ref(), Expr::List(_)))
                {
                    return Err(PolygraphError::Unsupported {
                        construct: "list ordering comparison in LQA SPARQL lowering".into(),
                        spec_ref: "openCypher 9 §7.3".into(),
                        reason:
                            "list ordering not yet supported in LQA path; legacy fallback applies"
                                .into(),
                    });
                }
                // Guard: list/map equality/inequality when null is present differs from plain
                // string comparison (null propagates in Cypher, but comparing the serialised
                // strings "[null, x]" always gives a definite false/true).  Return Err.
                if matches!(op, CmpOp::Eq | CmpOp::Ne)
                    && (lqa_expr_contains_null(a) || lqa_expr_contains_null(b))
                {
                    return Err(PolygraphError::Unsupported {
                        construct: "list/map equality with null elements in LQA SPARQL lowering".into(),
                        spec_ref: "openCypher 9 §7.3".into(),
                        reason: "null propagation in list/map equality not yet handled; legacy fallback applies".into(),
                    });
                }
                // Special case: `CmpOp::In` with a list literal RHS.
                // Expand to SparExpr::In(lhs, [item1, item2, ...]) before lowering
                // the RHS to avoid hitting the Unsupported path for Expr::List.
                if let (CmpOp::In, Expr::List(items)) = (op, b.as_ref()) {
                    // Guard: when the needle or any RHS list element contains null, Cypher's
                    // three-valued IN semantics differ from SPARQL string equality.  Err → legacy.
                    if lqa_expr_contains_null(a) || items.iter().any(lqa_expr_contains_null) {
                        return Err(PolygraphError::Unsupported {
                            construct: "list IN with null elements in LQA SPARQL lowering".into(),
                            spec_ref: "openCypher 9 §6.3.2".into(),
                            reason: "null propagation in list IN not yet handled; legacy fallback applies".into(),
                        });
                    }
                    let la = self.lower_expr(a)?;
                    let sparql_items = items
                        .iter()
                        .map(|item| self.lower_expr(item))
                        .collect::<Result<Vec<_>, _>>()?;
                    return Ok(SparExpr::In(Box::new(la), sparql_items));
                }
                // Special case: `literal_string IN keys(node_var)`.
                // Lower to EXISTS { ?n <base:prop> ?_kv } rather than calling
                // lower_function_call("keys") which would fall back to legacy.
                if let (
                    CmpOp::In,
                    Expr::Literal(Literal::String(key_str)),
                    Expr::FunctionCall {
                        name: fname,
                        args: fargs,
                        ..
                    },
                ) = (op, a.as_ref(), b.as_ref())
                {
                    if fname.eq_ignore_ascii_case("keys") && fargs.len() == 1 {
                        if let Some(Expr::Variable { name: var_name, .. }) = fargs.first() {
                            // Compile-time fold: 'key' IN keys(scalar_map_var).
                            if let Some(pairs) =
                                self.scalar_map_exprs.get(var_name.as_str()).cloned()
                            {
                                let found = pairs.iter().any(|(k, _)| k == key_str);
                                return Ok(SparExpr::Literal(SparLit::new_typed_literal(
                                    found.to_string(),
                                    NamedNode::new_unchecked(
                                        "http://www.w3.org/2001/XMLSchema#boolean",
                                    ),
                                )));
                            }
                            // Node property EXISTS pattern: 'key' IN keys(node_var).
                            if self.scan_vars.contains(var_name.as_str())
                                && !self.edge_vars.contains_key(var_name.as_str())
                            {
                                let node_var = Self::var(var_name);
                                let prop_iri = self.prop_iri(key_str);
                                let val_var = self.fresh("_kv");
                                let triple = TriplePattern {
                                    subject: TermPattern::Variable(node_var),
                                    predicate: NamedNodePattern::NamedNode(prop_iri),
                                    object: TermPattern::Variable(val_var),
                                };
                                return Ok(SparExpr::Exists(Box::new(GraphPattern::Bgp {
                                    patterns: vec![triple],
                                })));
                            }
                        }
                    }
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
                    CmpOp::In => {
                        // When the RHS is not a literal Expr::List, use our custom
                        // list-contains function so that ?lb is treated as a Cypher
                        // list string (our "[item1, item2, …]" encoding) rather than
                        // as a SPARQL value-set singleton (?b IN (?c) ≡ ?b = ?c).
                        SparExpr::FunctionCall(
                            Function::Custom(NamedNode::new_unchecked(
                                "urn:polygraph:list-contains",
                            )),
                            vec![lb, la], // list string first, needle second
                        )
                    }
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
                // Guard: skip this fast path if the base variable is a scalar (e.g. a
                // string-serialized map/list from WITH).  Scalar variables can't be RDF
                // subjects; the EXISTS pattern would always be empty, giving wrong results.
                if let Expr::Property(base, key) = e.as_ref() {
                    let mut base_is_scalar = false;
                    if let Expr::Variable { name, .. } = base.as_ref() {
                        base_is_scalar = self.scalar_vars.contains(name.as_str());
                    }
                    if !base_is_scalar {
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
                // Guard: skip when base is a scalar (can't be an RDF subject).
                if let Expr::Property(base, key) = e.as_ref() {
                    let mut base_is_scalar = false;
                    if let Expr::Variable { name, .. } = base.as_ref() {
                        base_is_scalar = self.scalar_vars.contains(name.as_str());
                    }
                    if !base_is_scalar {
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

            Expr::List(items) => {
                // Fast path: all elements are compile-time constants.
                if let Some(s) = lqa_serialize_literal(expr) {
                    return Ok(Self::lit_str(&s));
                }
                // Empty list.
                if items.is_empty() {
                    return Ok(Self::lit_str("[]"));
                }
                // Dynamic: build CONCAT("[", piece0, ", ", piece1, …, "]")
                let mut parts = vec![Self::lit_str("[")];
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        parts.push(Self::lit_str(", "));
                    }
                    parts.push(self.lower_expr_as_concat_piece(item)?);
                }
                parts.push(Self::lit_str("]"));
                Ok(SparExpr::FunctionCall(Function::Concat, parts))
            }

            Expr::Map(pairs) => {
                // Fast path: all values are compile-time constants.
                if let Some(s) = lqa_serialize_literal(expr) {
                    return Ok(Self::lit_str(&s));
                }
                // Empty map.
                if pairs.is_empty() {
                    return Ok(Self::lit_str("{}"));
                }
                // Dynamic: build CONCAT("{", "key1: ", val1, ", ", "key2: ", val2, "}")
                let mut parts = vec![Self::lit_str("{")];
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        parts.push(Self::lit_str(", "));
                    }
                    parts.push(Self::lit_str(&format!("{k}: ")));
                    parts.push(self.lower_expr_as_concat_piece(v)?);
                }
                parts.push(Self::lit_str("}"));
                Ok(SparExpr::FunctionCall(Function::Concat, parts))
            }

            Expr::ListSlice { .. }
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

            Expr::ListComprehension {
                variable,
                list,
                predicate,
                projection,
            } => {
                // Special case: [x IN list | toLower(x)]
                // → urn:polygraph:list-map-lower(list_expr)
                if let (None, Some(proj)) = (predicate, projection) {
                    if is_tolower_proj(proj, variable) {
                        let list_expr = self.lower_expr(list)?;
                        return Ok(SparExpr::FunctionCall(
                            Function::Custom(NamedNode::new_unchecked(
                                "urn:polygraph:list-map-lower",
                            )),
                            vec![list_expr],
                        ));
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: "list comprehension [x IN list WHERE pred | expr] (Phase C)".into(),
                    spec_ref: "openCypher 9 §6.3.3".into(),
                    reason:
                        "list comprehension over runtime lists requires engine extension"
                            .into(),
                })
            }

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
    fn lower_expr_as_concat_piece(&mut self, expr: &Expr) -> Result<SparExpr, PolygraphError> {
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
                // Special case: if the arg is a named path variable, return
                // the static hop count (or fail if it's a varlen path).
                if let Expr::Variable { name: pv, .. } = arg {
                    if let Some(&hops) = self.path_lengths.get(pv.as_str()) {
                        if hops == usize::MAX {
                            return Err(PolygraphError::Unsupported {
                                construct: "dynamic path length".into(),
                                spec_ref: "openCypher 9 §3.7".into(),
                                reason: format!(
                                    "size/length of variable-length path `{pv}` is dynamic; \
                                     only fixed-hop paths are supported in the LQA path"
                                ),
                            });
                        }
                        return Ok(Self::lit_integer(hops as i64));
                    }
                }
                // Special case: if the arg is a compile-time constant list,
                // return the list length directly as an integer literal.
                match arg {
                    Expr::List(items) => {
                        return Ok(Self::lit_integer(items.len() as i64));
                    }
                    Expr::Map(pairs) => {
                        return Ok(Self::lit_integer(pairs.len() as i64));
                    }
                    // size(list_a + list_b) where both are constant → element count.
                    Expr::Add(box_a, box_b)
                        if matches!(box_a.as_ref(), Expr::List(_))
                            || matches!(box_b.as_ref(), Expr::List(_)) =>
                    {
                        let n_a: usize = match box_a.as_ref() {
                            Expr::List(items) => items.len(),
                            _ => 1,
                        };
                        let n_b: usize = match box_b.as_ref() {
                            Expr::List(items) => items.len(),
                            _ => 1,
                        };
                        if lqa_serialize_literal(box_a).is_some()
                            && lqa_serialize_literal(box_b).is_some()
                        {
                            return Ok(Self::lit_integer((n_a + n_b) as i64));
                        }
                    }
                    _ => {}
                }
                let a = self.lower_expr(arg)?;
                Ok(SparExpr::FunctionCall(Function::StrLen, vec![a]))
            }
            "length" => {
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                // length() on a named path variable: return the static hop count.
                // Reject varlen paths (cannot statically compute length).
                if let Expr::Variable { name: pv, .. } = arg {
                    if let Some(&hops) = self.path_lengths.get(pv.as_str()) {
                        if hops == usize::MAX {
                            return Err(PolygraphError::Unsupported {
                                construct: "dynamic path length".into(),
                                spec_ref: "openCypher 9 §3.7".into(),
                                reason: format!(
                                    "length() on variable-length path `{pv}` is dynamic; \
                                     only fixed-hop paths are supported in the LQA path"
                                ),
                            });
                        }
                        return Ok(Self::lit_integer(hops as i64));
                    }
                }
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
            "nodes" => {
                // nodes(p) on a fixed-length, non-nullable named path:
                // emit CONCAT("[", STR(?n0), ", ", STR(?n1), ..., "]").
                // Falls back to legacy for varlen or OPTIONAL MATCH paths.
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                if let Expr::Variable { name: pv, .. } = arg {
                    if let Some(node_vars) = self.path_node_vars.get(pv.as_str()).cloned() {
                        // Only use compile-time CONCAT when no node is nullable.
                        // If the path comes from OPTIONAL MATCH the nodes may be
                        // absent; in that case fall through to legacy.
                        let any_nullable =
                            node_vars.iter().any(|v| self.nullable.contains(v.as_str()));
                        if !any_nullable {
                            let mut parts: Vec<SparExpr> = vec![Self::lit_str("[")];
                            for (idx, v) in node_vars.iter().enumerate() {
                                if idx > 0 {
                                    parts.push(Self::lit_str(", "));
                                }
                                parts.push(SparExpr::FunctionCall(
                                    Function::Str,
                                    vec![SparExpr::Variable(v.clone())],
                                ));
                            }
                            parts.push(Self::lit_str("]"));
                            return Ok(SparExpr::FunctionCall(Function::Concat, parts));
                        }
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: "nodes()".into(),
                    spec_ref: "openCypher 9 §3.7".into(),
                    reason: "nodes() on varlen or nullable path requires legacy path".into(),
                })
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
                // If the argument variable is a node (in scan_vars but not an edge),
                // type() on a node is a static InvalidArgumentType — raise Translation.
                if let Some(Expr::Variable { name: rv, .. }) = args.first() {
                    if self.scan_vars.contains(rv.as_str())
                        && !self.edge_types.contains_key(rv.as_str())
                    {
                        return Err(PolygraphError::Translation {
                            message: format!(
                                "type() applied to node variable '{rv}'; expected a relationship"
                            ),
                        });
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
            "rand" => Err(PolygraphError::Unsupported {
                construct: "rand()".into(),
                spec_ref: "openCypher 9 §6.3.2".into(),
                reason: "rand() inside aggregates raises SyntaxError; legacy fallback needed"
                    .into(),
            }),
            "sqrt" => {
                // Constant-fold sqrt() when argument is a numeric literal.
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                if let Some(val) = const_eval_numeric(arg) {
                    let result = val.sqrt();
                    if result.is_finite() {
                        return Ok(Self::lit_double(result));
                    }
                }
                Err(PolygraphError::Unsupported {
                    construct: format!("{name}()"),
                    spec_ref: "openCypher 9 §6.3.2".into(),
                    reason: "sqrt() with non-constant argument requires legacy path".into(),
                })
            }
            "reverse" => {
                // Constant folding: reverse over a compile-time constant string.
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                match arg {
                    Expr::Literal(Literal::String(s)) => {
                        let reversed: String = s.chars().rev().collect();
                        Ok(SparExpr::Literal(SparLit::new_simple_literal(reversed)))
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "reverse()".into(),
                        spec_ref: "openCypher 9 §6.3.2".into(),
                        reason: "reverse() on non-constant string requires legacy path".into(),
                    }),
                }
            }
            "toboolean" => {
                let arg = args.first().ok_or_else(|| arg_err(name))?;
                match arg {
                    // Identity on booleans.
                    Expr::Literal(Literal::Boolean(b)) => Ok(Self::lit_bool(*b)),
                    // Null propagation.
                    Expr::Literal(Literal::Null) => Ok(SparExpr::Variable(self.fresh("_tob_null"))),
                    // Constant string → boolean.
                    Expr::Literal(Literal::String(s)) => match s.to_lowercase().as_str() {
                        "true" => Ok(Self::lit_bool(true)),
                        "false" => Ok(Self::lit_bool(false)),
                        _ => Ok(SparExpr::Variable(self.fresh("_tob_null"))),
                    },
                    // Runtime conversion: bind arg to a probe variable, then:
                    //   IF(!BOUND(?probe), undef,
                    //     IF(DATATYPE(?probe) = xsd:boolean, ?probe,
                    //       IF(LCASE(STR(?probe)) = "true", true,
                    //         IF(LCASE(STR(?probe)) = "false", false, undef))))
                    _ => {
                        let lowered = self.lower_expr(arg)?;
                        let (probe, a_expr) = if let SparExpr::Variable(ref v) = lowered {
                            (v.clone(), lowered.clone())
                        } else {
                            let p = self.fresh("_tob_probe");
                            self.pending_binds.push((p.clone(), lowered));
                            (p.clone(), SparExpr::Variable(p))
                        };
                        let null_var = self.fresh("_tob_null");
                        let lcase_str = SparExpr::FunctionCall(
                            spargebra::algebra::Function::LCase,
                            vec![SparExpr::FunctionCall(
                                spargebra::algebra::Function::Str,
                                vec![a_expr.clone()],
                            )],
                        );
                        // IF(LCASE(STR(?probe)) = "false", false, undef)
                        let inner = SparExpr::If(
                            Box::new(SparExpr::Equal(
                                Box::new(lcase_str.clone()),
                                Box::new(Self::lit_str("false")),
                            )),
                            Box::new(Self::lit_bool(false)),
                            Box::new(SparExpr::Variable(null_var.clone())),
                        );
                        // IF(LCASE(STR(?probe)) = "true", true, ...)
                        let inner = SparExpr::If(
                            Box::new(SparExpr::Equal(
                                Box::new(lcase_str),
                                Box::new(Self::lit_str("true")),
                            )),
                            Box::new(Self::lit_bool(true)),
                            Box::new(inner),
                        );
                        // IF(DATATYPE(?probe) = xsd:boolean, ?probe, ...)
                        let inner = SparExpr::If(
                            Box::new(SparExpr::Equal(
                                Box::new(SparExpr::FunctionCall(
                                    spargebra::algebra::Function::Datatype,
                                    vec![a_expr.clone()],
                                )),
                                Box::new(SparExpr::NamedNode(NamedNode::new_unchecked(
                                    XSD_BOOLEAN,
                                ))),
                            )),
                            Box::new(a_expr),
                            Box::new(inner),
                        );
                        // IF(!BOUND(?probe), undef, ...)
                        Ok(SparExpr::If(
                            Box::new(SparExpr::Not(Box::new(SparExpr::Bound(probe)))),
                            Box::new(SparExpr::Variable(null_var)),
                            Box::new(inner),
                        ))
                    }
                }
            }
            // ── Temporal constructors ──────────────────────────────────────
            // date(), time(), localtime(), datetime(), localdatetime(), duration()
            // plus their .transaction/.statement/.realtime variants (current-time stubs).
            // All calendar arithmetic is performed at translation time using the
            // same helpers used by the legacy translator path.
            "date"
            | "localtime"
            | "localdatetime"
            | "time"
            | "datetime"
            | "duration"
            | "date.transaction"
            | "date.statement"
            | "date.realtime"
            | "localtime.transaction"
            | "localtime.statement"
            | "localtime.realtime"
            | "time.transaction"
            | "time.statement"
            | "time.realtime"
            | "localdatetime.transaction"
            | "localdatetime.statement"
            | "localdatetime.realtime"
            | "datetime.transaction"
            | "datetime.statement"
            | "datetime.realtime" => {
                use crate::translator::cypher::{
                    strip_named_tz, temporal_date_from_map, temporal_datetime_from_map,
                    temporal_duration_from_map, temporal_localdatetime_from_map,
                    temporal_localtime_from_map, temporal_parse_date, temporal_parse_datetime,
                    temporal_parse_duration, temporal_parse_localdatetime,
                    temporal_parse_localtime, temporal_parse_time, temporal_time_from_map,
                };
                let xsd_date = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#date");
                let xsd_time = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#time");
                let xsd_dt = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#dateTime");
                // Strip .transaction/.statement/.realtime suffix for dispatch.
                let base_func = name_lower
                    .strip_suffix(".transaction")
                    .or_else(|| name_lower.strip_suffix(".statement"))
                    .or_else(|| name_lower.strip_suffix(".realtime"))
                    .unwrap_or(name_lower.as_str());

                // Zero-arg form: return deterministic fixed timestamp (openCypher §3.5).
                if args.is_empty() {
                    let lit = match base_func {
                        "date" => SparLit::new_typed_literal("2000-01-01".to_owned(), xsd_date),
                        "localtime" => {
                            SparLit::new_typed_literal("00:00:00".to_owned(), xsd_time.clone())
                        }
                        "time" => SparLit::new_typed_literal("00:00:00Z".to_owned(), xsd_time),
                        "localdatetime" => SparLit::new_typed_literal(
                            "2000-01-01T00:00:00".to_owned(),
                            xsd_dt.clone(),
                        ),
                        "datetime" => {
                            SparLit::new_typed_literal("2000-01-01T00:00Z".to_owned(), xsd_dt)
                        }
                        "duration" => SparLit::new_simple_literal("PT0S".to_owned()),
                        _ => return Ok(SparExpr::Variable(self.fresh("null"))),
                    };
                    return Ok(SparExpr::Literal(lit));
                }

                // Null propagation: temporal_f(null) → null.
                if matches!(args.first(), Some(Expr::Literal(Literal::Null))) {
                    return Ok(SparExpr::Variable(self.fresh("null")));
                }

                // Variable fold: temporal_f(v) where v is a known scalar literal.
                if let Some(Expr::Variable { name: v_name, .. }) = args.first() {
                    if let Some(s) = self.scalar_lit_vals.get(v_name.as_str()).cloned() {
                        let effective_s = if base_func == "time" {
                            temporal_parse_time(&s)
                                .map(|t| strip_named_tz(&t))
                                .unwrap_or(s.clone())
                        } else {
                            s.clone()
                        };
                        let folded_arg = Expr::Literal(Literal::String(effective_s));
                        return self.lower_function_call(name, &[folded_arg]);
                    }
                }

                // Map argument: date({year: N, month: M, ...}) etc.
                // Variable values in the map are expanded via scalar_lit_vals (e.g.
                // `date({date: other, year: 28})` where `other` is a WITH-bound
                // temporal literal).  If any variable value is not in scalar_lit_vals,
                // the constructor cannot be computed at compile time — fall through to
                // Err(Unsupported) which routes to legacy.
                if let Some(Expr::Map(pairs)) = args.first() {
                    use crate::ast::cypher::{Expression as AE, Literal as AL};
                    let mut expanded: Vec<(String, AE)> = Vec::new();
                    let mut all_resolvable = true;
                    for (k, v) in pairs {
                        let ae = match v {
                            Expr::Literal(Literal::Integer(n)) => AE::Literal(AL::Integer(*n)),
                            Expr::Literal(Literal::Float(f)) => AE::Literal(AL::Float(*f)),
                            Expr::Literal(Literal::String(s)) => AE::Literal(AL::String(s.clone())),
                            Expr::Literal(Literal::Boolean(b)) => AE::Literal(AL::Boolean(*b)),
                            Expr::Literal(Literal::Null) => AE::Literal(AL::Null),
                            Expr::Variable { name: vn, .. } => {
                                if let Some(s) = self.scalar_lit_vals.get(vn.as_str()) {
                                    AE::Literal(AL::String(s.clone()))
                                } else {
                                    all_resolvable = false;
                                    break;
                                }
                            }
                            _ => {
                                all_resolvable = false;
                                break;
                            }
                        };
                        expanded.push((k.clone(), ae));
                    }
                    if all_resolvable {
                        let lit_opt: Option<SparLit> = match base_func {
                            "date" => temporal_date_from_map(&expanded)
                                .map(|s| SparLit::new_typed_literal(s, xsd_date.clone())),
                            "localtime" => temporal_localtime_from_map(&expanded)
                                .map(|s| SparLit::new_typed_literal(s, xsd_time.clone())),
                            "time" => temporal_time_from_map(&expanded)
                                .map(|s| SparLit::new_typed_literal(s, xsd_time.clone())),
                            "localdatetime" => temporal_localdatetime_from_map(&expanded)
                                .map(|s| SparLit::new_typed_literal(s, xsd_dt.clone())),
                            "datetime" => temporal_datetime_from_map(&expanded)
                                .map(|s| SparLit::new_typed_literal(s, xsd_dt.clone())),
                            "duration" => temporal_duration_from_map(&expanded)
                                .map(SparLit::new_simple_literal),
                            _ => None,
                        };
                        if let Some(lit) = lit_opt {
                            return Ok(SparExpr::Literal(lit));
                        }
                    }
                }

                // String argument: date('2015-07-21') etc.
                if let Some(Expr::Literal(Literal::String(s))) = args.first() {
                    let s = s.clone();
                    let lit_opt: Option<SparLit> = match base_func {
                        "date" => {
                            temporal_parse_date(&s).map(|v| SparLit::new_typed_literal(v, xsd_date))
                        }
                        "localtime" => temporal_parse_localtime(&s)
                            .map(|v| SparLit::new_typed_literal(v, xsd_time.clone())),
                        "time" => {
                            temporal_parse_time(&s).map(|v| SparLit::new_typed_literal(v, xsd_time))
                        }
                        "localdatetime" => temporal_parse_localdatetime(&s)
                            .map(|v| SparLit::new_typed_literal(v, xsd_dt.clone())),
                        "datetime" => temporal_parse_datetime(&s)
                            .map(|v| SparLit::new_typed_literal(v, xsd_dt)),
                        "duration" => temporal_parse_duration(&s).map(SparLit::new_simple_literal),
                        _ => None,
                    };
                    if let Some(lit) = lit_opt {
                        return Ok(SparExpr::Literal(lit));
                    }
                }

                // Non-literal argument (runtime temporal expression) — fall back to legacy.
                Err(PolygraphError::Unsupported {
                    construct: format!("{name}()"),
                    spec_ref: "openCypher 9 §3.5".into(),
                    reason: format!(
                        "Temporal constructor '{name}' with non-literal arguments requires \
                         legacy path"
                    ),
                })
            }
            // ── range() builtin ───────────────────────────────────────────
            // range(start, end [, step]) → list of integers.
            // When bounds are constant integers (or compile-time const vars),
            // expand to a serialized list string "[s, s+1, …, e]".
            "range" => {
                if let Some(items) = eval_range_to_integers(args) {
                    let parts: Vec<String> = items.iter().map(|n| n.to_string()).collect();
                    let s = format!("[{}]", parts.join(", "));
                    return Ok(SparExpr::Literal(SparLit::new_simple_literal(s)));
                }
                Err(PolygraphError::Unsupported {
                    construct: "range()".into(),
                    spec_ref: "openCypher 9 §6.3.3".into(),
                    reason: "range() with non-literal arguments requires legacy path".into(),
                })
            }
            // ── keys() ────────────────────────────────────────────────────────────
            // keys({k: v, ...}) → compile-time list of key strings
            // keys(null) | keys(nullable_var) → null
            "keys" => {
                let arg = args.first().ok_or_else(|| PolygraphError::Unsupported {
                    construct: "keys()".into(),
                    spec_ref: "openCypher 9 §6.3.5".into(),
                    reason: "keys() requires an argument".into(),
                })?;
                match arg {
                    Expr::Map(pairs) => {
                        let key_list: Vec<String> =
                            pairs.iter().map(|(k, _)| format!("'{k}'")).collect();
                        Ok(SparExpr::Literal(SparLit::new_simple_literal(format!(
                            "[{}]",
                            key_list.join(", ")
                        ))))
                    }
                    Expr::Literal(crate::lqa::expr::Literal::Null) => {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    Expr::Variable { name: vname, .. }
                        if self.nullable.contains(vname.as_str()) =>
                    {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "keys()".into(),
                        spec_ref: "openCypher 9 §6.3.5".into(),
                        reason: "keys() on non-literal map requires legacy path".into(),
                    }),
                }
            }
            // ── labels() ──────────────────────────────────────────────────────────
            // labels(null) | labels(nullable_var) → null
            // labels(node_var) → GROUP BY subquery collecting rdf:type values
            "labels" => {
                let arg = args.first().ok_or_else(|| PolygraphError::Unsupported {
                    construct: "labels()".into(),
                    spec_ref: "openCypher 9 §6.3.5".into(),
                    reason: "labels() requires an argument".into(),
                })?;
                match arg {
                    Expr::Literal(crate::lqa::expr::Literal::Null) => {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    Expr::Variable { name: vname, .. }
                        if self.nullable.contains(vname.as_str()) =>
                    {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    Expr::Variable { name: vname, .. } => {
                        // Only handle scan_vars (MATCH-bound graph nodes).
                        // Variables that are path names, relationship variables, computed
                        // aliases, or anything not from a graph scan must fall back to
                        // legacy so that the correct SyntaxError/TypeError is raised.
                        if !self.scan_vars.contains(vname.as_str()) {
                            return Err(PolygraphError::Unsupported {
                                construct: "labels()".into(),
                                spec_ref: "openCypher 9 §6.3.5".into(),
                                reason: format!(
                                    "labels({vname}) on non-scan-var requires legacy path \
                                     (may be a path variable or relationship variable)"
                                ),
                            });
                        }
                        let n_var = Self::var(vname);
                        let ltype_var = self.fresh(&format!("_ltype_{vname}"));
                        let gc_var = self.fresh(&format!("_labels_gc_{vname}"));
                        let base = self.base_iri.clone();
                        let base_len = base.len();
                        // Inner: ?n rdf:type ?_ltype . FILTER(STRSTARTS(STR(?_ltype), base))
                        let inner_bgp = GraphPattern::Bgp {
                            patterns: vec![TriplePattern {
                                subject: TermPattern::Variable(n_var.clone()),
                                predicate: NamedNodePattern::NamedNode(NamedNode::new_unchecked(
                                    RDF_TYPE,
                                )),
                                object: TermPattern::Variable(ltype_var.clone()),
                            }],
                        };
                        let filter_expr = SparExpr::FunctionCall(
                            Function::StrStarts,
                            vec![
                                SparExpr::FunctionCall(
                                    Function::Str,
                                    vec![SparExpr::Variable(ltype_var.clone())],
                                ),
                                SparExpr::Literal(SparLit::new_simple_literal(base)),
                            ],
                        );
                        let inner_filtered = GraphPattern::Filter {
                            expr: filter_expr,
                            inner: Box::new(inner_bgp),
                        };
                        // Label name: SUBSTR(STR(?_ltype), base_len + 1)
                        let label_name_expr = SparExpr::FunctionCall(
                            Function::SubStr,
                            vec![
                                SparExpr::FunctionCall(
                                    Function::Str,
                                    vec![SparExpr::Variable(ltype_var.clone())],
                                ),
                                SparExpr::Literal(SparLit::new_typed_literal(
                                    (base_len + 1).to_string(),
                                    NamedNode::new_unchecked(XSD_INTEGER),
                                )),
                            ],
                        );
                        // Quoted: CONCAT("'", label_name, "'")
                        let quoted_label = SparExpr::FunctionCall(
                            Function::Concat,
                            vec![
                                SparExpr::Literal(SparLit::new_simple_literal("'")),
                                label_name_expr,
                                SparExpr::Literal(SparLit::new_simple_literal("'")),
                            ],
                        );
                        let gc_agg = AggregateExpression::FunctionCall {
                            name: AggregateFunction::GroupConcat {
                                separator: Some(", ".into()),
                            },
                            expr: quoted_label,
                            distinct: true,
                        };
                        let group_pattern = GraphPattern::Group {
                            inner: Box::new(inner_filtered),
                            variables: vec![n_var],
                            aggregates: vec![(gc_var.clone(), gc_agg)],
                        };
                        self.pending_optional_patterns.push(group_pattern);
                        // Result: IF(BOUND(?gc), CONCAT("[", ?gc, "]"), "[]")
                        Ok(SparExpr::If(
                            Box::new(SparExpr::Bound(gc_var.clone())),
                            Box::new(SparExpr::FunctionCall(
                                Function::Concat,
                                vec![
                                    SparExpr::Literal(SparLit::new_simple_literal("[")),
                                    SparExpr::Variable(gc_var),
                                    SparExpr::Literal(SparLit::new_simple_literal("]")),
                                ],
                            )),
                            Box::new(SparExpr::Literal(SparLit::new_simple_literal("[]"))),
                        ))
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "labels()".into(),
                        spec_ref: "openCypher 9 §6.3.5".into(),
                        reason: "labels() on non-variable argument requires legacy path".into(),
                    }),
                }
            }
            // ── properties() ──────────────────────────────────────────────────────
            // properties(null) | properties(nullable_var) → null
            // properties({k: v, ...}) → serialized map string
            "properties" => {
                let arg = args.first().ok_or_else(|| PolygraphError::Unsupported {
                    construct: "properties()".into(),
                    spec_ref: "openCypher 9 §6.3.5".into(),
                    reason: "properties() requires an argument".into(),
                })?;
                match arg {
                    Expr::Literal(crate::lqa::expr::Literal::Null) => {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    Expr::Map(pairs) => {
                        let serialized = Self::serialize_map_literal(pairs);
                        Ok(SparExpr::Literal(SparLit::new_simple_literal(serialized)))
                    }
                    Expr::Variable { name: vname, .. }
                        if self.nullable.contains(vname.as_str()) =>
                    {
                        Ok(SparExpr::Variable(self.fresh("_null")))
                    }
                    // Edge variable (check before scan_vars — edge vars are also in scan_vars):
                    // build GROUP BY subquery via RDF-star reification.
                    Expr::Variable { name: vname, .. }
                        if self.edge_vars.contains_key(vname.as_str()) =>
                    {
                        Ok(self.build_edge_properties_expr(vname))
                    }
                    // Node variable: build a GROUP BY subquery that collects all
                    // base-namespace properties and formats them as "{key: val, ...}".
                    Expr::Variable { name: vname, .. }
                        if self.scan_vars.contains(vname.as_str()) =>
                    {
                        Ok(self.build_node_properties_expr(vname))
                    }
                    _ => Err(PolygraphError::Unsupported {
                        construct: "properties()".into(),
                        spec_ref: "openCypher 9 §6.3.5".into(),
                        reason: "properties() on node/relationship requires legacy path".into(),
                    }),
                }
            }
            // Known openCypher functions that are valid but not yet implemented in the
            // LQA SPARQL path — fall through to legacy rather than raising a hard error.
            "datetime.truncate"
            | "localdatetime.truncate"
            | "date.truncate"
            | "time.truncate"
            | "localtime.truncate"
            | "duration.between"
            | "duration.inmonths"
            | "duration.indays"
            | "duration.inseconds"
            | "datetime.fromepoch"
            | "datetime.fromepochmillis"
            | "sin"
            | "cos"
            | "tan"
            | "asin"
            | "acos"
            | "atan"
            | "atan2"
            | "cot"
            | "degrees"
            | "radians"
            | "haversin"
            | "log"
            | "log10"
            | "e"
            | "pi"
            | "reduce"
            | "any"
            | "all"
            | "none"
            | "single"
            | "relationships"
            | "shortestpath"
            | "allshortestpaths"
            | "split"
            | "replace"
            | "left"
            | "right"
            | "collect"
            | "percentiledisc"
            | "percentilecont"
            | "stdev"
            | "stdevp" => Err(PolygraphError::Unsupported {
                construct: format!("{name}()"),
                spec_ref: "openCypher 9 §6.3".into(),
                reason: format!("function '{name}' not yet in LQA path; legacy fallback applies"),
            }),
            _ => {
                // Truly unknown function: raise a Translation/SyntaxError.
                Err(PolygraphError::Translation {
                    message: format!("Unknown function '{name}'"),
                })
            }
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

    /// Compute a JDN-based temporal property (week, weekYear, weekDay, ordinalDay,
    /// dayOfQuarter) for a RUNTIME date/datetime variable `var_name`.
    ///
    /// Intermediate computations are pushed to `self.pending_binds`; these are
    /// flushed as SPARQL BIND/EXTEND nodes by `flush_pending` in the enclosing
    /// `lower_op` call.  Returns a `SparExpr` for the final value.
    fn lower_temporal_jdn_property(&mut self, var_name: &str, prop: &str) -> Option<SparExpr> {
        use spargebra::algebra::Function;
        type SE = SparExpr;

        let xsi_nn = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#integer");
        let xsd_dec = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#decimal");

        let d_var = SE::Variable(Self::var(var_name));
        let str_e = SE::FunctionCall(Function::Str, vec![d_var]);

        // ── arithmetic / cast helpers (all produce SparExpr inline) ──────────
        macro_rules! dim {
            ($n:expr) => {
                SE::Literal(SparLit::new_typed_literal(
                    ($n as i64).to_string(),
                    xsi_nn.clone(),
                ))
            };
        }
        macro_rules! ddm {
            ($s:expr) => {
                SE::Literal(SparLit::new_typed_literal($s.to_owned(), xsd_dec.clone()))
            };
        }
        macro_rules! int_cast {
            ($e:expr) => {
                SE::FunctionCall(Function::Custom(xsi_nn.clone()), vec![$e])
            };
        }
        macro_rules! dec_cast {
            ($e:expr) => {
                SE::FunctionCall(Function::Custom(xsd_dec.clone()), vec![$e])
            };
        }
        macro_rules! substr2 {
            ($s:expr, $st:expr, $ln:expr) => {
                SE::FunctionCall(Function::SubStr, vec![$s, dim!($st), dim!($ln)])
            };
        }
        macro_rules! floor_f {
            ($e:expr) => {
                SE::FunctionCall(Function::Floor, vec![$e])
            };
        }
        macro_rules! add {
            ($a:expr, $b:expr) => {
                SE::Add(Box::new($a), Box::new($b))
            };
        }
        macro_rules! sub {
            ($a:expr, $b:expr) => {
                SE::Subtract(Box::new($a), Box::new($b))
            };
        }
        macro_rules! mul {
            ($a:expr, $b:expr) => {
                SE::Multiply(Box::new($a), Box::new($b))
            };
        }
        macro_rules! div {
            ($a:expr, $b:expr) => {
                SE::Divide(Box::new($a), Box::new($b))
            };
        }

        // Push intermediate bind; returns SparExpr::Variable referencing it.
        macro_rules! bind {
            ($hint:literal, $expr:expr) => {{
                let v = self.fresh(concat!("tp_", $hint));
                self.pending_binds.push((v.clone(), $expr));
                SE::Variable(v)
            }};
        }

        // ── Date component extraction (string-based, works for xsd:date/dateTime) ─
        let v_Y = bind!("Y", int_cast!(substr2!(str_e.clone(), 1, 4)));
        let v_M = bind!("M", int_cast!(substr2!(str_e.clone(), 6, 2)));
        let v_D = bind!("D", int_cast!(substr2!(str_e.clone(), 9, 2)));
        let v_Yd = bind!("Yd", dec_cast!(v_Y.clone()));
        let v_Md = bind!("Md", dec_cast!(v_M.clone()));
        let v_Dd = bind!("Dd", dec_cast!(v_D.clone()));

        // ── Julian Day Number (JDN) ─────────────────────────────────────────
        // a = FLOOR((14 – Md) / 12)
        let v_14mM = bind!("14mM", sub!(ddm!("14"), v_Md.clone()));
        let v_jdn_a = bind!("jdna", floor_f!(div!(v_14mM, ddm!("12"))));
        // y = Yd + 4800 – a
        let v_jdn_y = bind!(
            "jdny",
            sub!(add!(v_Yd.clone(), ddm!("4800")), v_jdn_a.clone())
        );
        // m = Md + 12*a – 3
        let v_12a = bind!("12a", mul!(ddm!("12"), v_jdn_a.clone()));
        let v_jdn_m = bind!("jdnm", sub!(add!(v_Md.clone(), v_12a), ddm!("3")));
        // FLOOR((153*m + 2) / 5)
        let v_153m = bind!("153m", mul!(ddm!("153"), v_jdn_m));
        let v_153m2 = bind!("153m2", add!(v_153m, ddm!("2")));
        let v_f153m25 = bind!("f153m25", floor_f!(div!(v_153m2, ddm!("5"))));
        // 365*y, FLOOR(y/4), FLOOR(y/100), FLOOR(y/400)
        let v_365y = bind!("365y", mul!(ddm!("365"), v_jdn_y.clone()));
        let v_y4 = bind!("y4", floor_f!(div!(v_jdn_y.clone(), ddm!("4"))));
        let v_y100 = bind!("y100", floor_f!(div!(v_jdn_y.clone(), ddm!("100"))));
        let v_y400 = bind!("y400", floor_f!(div!(v_jdn_y.clone(), ddm!("400"))));
        // JDN = D + f153m25 + 365y + y4 – y100 + y400 – 32045
        // Oxigraph right-assoc workaround: sum positives, sum negatives, then subtract.
        let v_jdn_pos = bind!(
            "JDNp",
            add!(add!(add!(add!(v_Dd, v_f153m25), v_365y), v_y4), v_y400)
        );
        let v_jdn_neg = bind!("JDNn", add!(v_y100, ddm!("32045")));
        let v_JDN = bind!("JDN", sub!(v_jdn_pos, v_jdn_neg));

        // ── JDN mod 7 (0 = Monday … 6 = Sunday) ────────────────────────────
        let v_JDN7 = bind!("JDN7", floor_f!(div!(v_JDN.clone(), ddm!("7"))));
        let v_mod7 = bind!("mod7", sub!(v_JDN.clone(), mul!(ddm!("7"), v_JDN7)));

        if prop == "weekDay" || prop == "dayOfWeek" {
            // ISO weekday: 1=Mon .. 7=Sun; int_cast wraps the Add ✓
            return Some(int_cast!(add!(v_mod7, ddm!("1"))));
        }

        // ── ordinalDay = JDN − JDN(Y, 1, 1) + 1 ─────────────────────────────
        let v_y4799 = bind!("y4799", add!(v_Yd.clone(), ddm!("4799")));
        let v_365yj1 = bind!("365yj1", mul!(ddm!("365"), v_y4799.clone()));
        let v_yj1_4 = bind!("yj1_4", floor_f!(div!(v_y4799.clone(), ddm!("4"))));
        let v_yj1_100 = bind!("yj1_100", floor_f!(div!(v_y4799.clone(), ddm!("100"))));
        let v_yj1_400 = bind!("yj1_400", floor_f!(div!(v_y4799, ddm!("400"))));
        // JDN(Y,1,1): D=1, M=1 → a=1, y=Y+4799, m=10 → D + FLOOR((153*10+2)/5) = 1+306 = 307
        let v_JDNj1_p = bind!(
            "JDNj1p",
            add!(add!(add!(ddm!("307"), v_365yj1), v_yj1_4), v_yj1_400)
        );
        let v_JDNj1_n = bind!("JDNj1n", add!(v_yj1_100, ddm!("32045")));
        let v_JDN_j1 = bind!("JDNj1", sub!(v_JDNj1_p, v_JDNj1_n));

        if prop == "ordinalDay" || prop == "dayOfYear" {
            let v_dj1 = bind!("dj1", sub!(v_JDN.clone(), v_JDN_j1));
            return Some(int_cast!(add!(v_dj1, ddm!("1"))));
        }

        // ── ISO week / weekYear ───────────────────────────────────────────────
        // JDN of the Thursday of the same ISO week: thu_jdn = JDN + 3 – mod7
        let v_thu_jdn = bind!("thujdn", sub!(add!(v_JDN.clone(), ddm!("3")), v_mod7));

        // Inverse JDN formula to recover thu_year (Gregorian proleptic year)
        let v_inv_a = bind!("inva", add!(v_thu_jdn.clone(), ddm!("32044")));
        let v_4a = bind!("4a", mul!(ddm!("4"), v_inv_a.clone()));
        let v_4a3 = bind!("4a3", add!(v_4a, ddm!("3")));
        let v_inv_b = bind!("invb", floor_f!(div!(v_4a3, ddm!("146097"))));
        let v_146097b = bind!("146b", mul!(ddm!("146097"), v_inv_b.clone()));
        let v_146097b4 = bind!("146b4", floor_f!(div!(v_146097b, ddm!("4"))));
        let v_inv_c = bind!("invc", sub!(v_inv_a, v_146097b4));
        let v_4c = bind!("4c", mul!(ddm!("4"), v_inv_c.clone()));
        let v_4c3 = bind!("4c3", add!(v_4c, ddm!("3")));
        let v_inv_d = bind!("invd", floor_f!(div!(v_4c3, ddm!("1461"))));
        let v_1461d = bind!("1461d", mul!(ddm!("1461"), v_inv_d.clone()));
        let v_1461d4 = bind!("1461d4", floor_f!(div!(v_1461d, ddm!("4"))));
        let v_inv_e = bind!("inve", sub!(v_inv_c, v_1461d4));
        let v_5e = bind!("5e", mul!(ddm!("5"), v_inv_e));
        let v_5e2 = bind!("5e2", add!(v_5e, ddm!("2")));
        let v_inv_m = bind!("invm", floor_f!(div!(v_5e2, ddm!("153"))));
        let v_m10 = bind!("m10", floor_f!(div!(v_inv_m, ddm!("10"))));
        let v_100b = bind!("100b", mul!(ddm!("100"), v_inv_b));
        // thu_year = 100*b + d + FLOOR(m/10) – 4800
        let v_tyr_p = bind!("tyrp", add!(add!(v_100b, v_inv_d), v_m10));
        let v_thu_year = bind!("tyr", sub!(v_tyr_p, ddm!("4800")));

        if prop == "weekYear" {
            return Some(int_cast!(v_thu_year));
        }

        if prop == "week" {
            // JDN(thu_year, 1, 4): D=4, a=1, y=ty+4799, m=10 → 4+FLOOR((153*10+2)/5)=4+306=310
            let v_ty4799 = bind!("ty4799", add!(dec_cast!(v_thu_year), ddm!("4799")));
            let v_365ty = bind!("365ty", mul!(ddm!("365"), v_ty4799.clone()));
            let v_ty4 = bind!("ty4", floor_f!(div!(v_ty4799.clone(), ddm!("4"))));
            let v_ty100 = bind!("ty100", floor_f!(div!(v_ty4799.clone(), ddm!("100"))));
            let v_ty400 = bind!("ty400", floor_f!(div!(v_ty4799, ddm!("400"))));
            let v_JDNtj4_p = bind!(
                "JDNtj4p",
                add!(add!(add!(ddm!("310"), v_365ty), v_ty4), v_ty400)
            );
            let v_JDNtj4_n = bind!("JDNtj4n", add!(v_ty100, ddm!("32045")));
            let v_JDN_tj4 = bind!("JDNtj4", sub!(v_JDNtj4_p, v_JDNtj4_n));
            // w1_mon = JDN_tj4 – (JDN_tj4 mod 7)  (Monday of ISO week 1)
            let v_tj4_7 = bind!("tj47", floor_f!(div!(v_JDN_tj4.clone(), ddm!("7"))));
            let v_j4mod7 = bind!("j4m7", sub!(v_JDN_tj4.clone(), mul!(ddm!("7"), v_tj4_7)));
            let v_w1_mon = bind!("w1mon", sub!(v_JDN_tj4, v_j4mod7));
            let v_thu_w1 = bind!("thuw1", sub!(v_thu_jdn, v_w1_mon));
            let v_wraw = bind!("wraw", floor_f!(div!(v_thu_w1, ddm!("7"))));
            return Some(int_cast!(add!(v_wraw, ddm!("1"))));
        }

        if prop == "dayOfQuarter" {
            // quarter start month: FLOOR((Md – 1) / 3) * 3 + 1
            let v_m1 = bind!("m1", sub!(v_Md.clone(), ddm!("1")));
            let v_qm3 = bind!("qm3", floor_f!(div!(v_m1, ddm!("3"))));
            let v_qsm = bind!("qsm", add!(mul!(ddm!("3"), v_qm3), ddm!("1")));
            // JDN(Y, qsm, 1)
            let v_14qs = bind!("14qs", sub!(ddm!("14"), v_qsm.clone()));
            let v_qs_a = bind!("qsa", floor_f!(div!(v_14qs, ddm!("12"))));
            let v_qs_y = bind!("qsy", sub!(add!(v_Yd, ddm!("4800")), v_qs_a.clone()));
            let v_12qsa = bind!("12qsa", mul!(ddm!("12"), v_qs_a));
            let v_qs_m = bind!("qsm2", sub!(add!(v_qsm, v_12qsa), ddm!("3")));
            let v_153qm = bind!("153qm", mul!(ddm!("153"), v_qs_m));
            let v_153qm2 = bind!("153qm2", add!(v_153qm, ddm!("2")));
            let v_f153q = bind!("f153q", floor_f!(div!(v_153qm2, ddm!("5"))));
            let v_365qy = bind!("365qy", mul!(ddm!("365"), v_qs_y.clone()));
            let v_qy4 = bind!("qy4", floor_f!(div!(v_qs_y.clone(), ddm!("4"))));
            let v_qy100 = bind!("qy100", floor_f!(div!(v_qs_y.clone(), ddm!("100"))));
            let v_qy400 = bind!("qy400", floor_f!(div!(v_qs_y, ddm!("400"))));
            // JDN of quarter start (D=1): 1 + FLOOR((153*m+2)/5) → same as above but D=1
            let v_JDNqs_p = bind!(
                "JDNqsp",
                add!(
                    add!(add!(add!(ddm!("1"), v_f153q), v_365qy), v_qy4),
                    v_qy400
                )
            );
            let v_JDNqs_n = bind!("JDNqsn", add!(v_qy100, ddm!("32045")));
            let v_JDN_qs = bind!("JDNqs", sub!(v_JDNqs_p, v_JDNqs_n));
            let v_dqs = bind!("dqs", sub!(v_JDN, v_JDN_qs));
            return Some(int_cast!(add!(v_dqs, ddm!("1"))));
        }

        None
    }

    /// Recursively resolve a map literal structure rooted at `expr`.
    ///
    /// Returns the `(key, value)` pairs of the resolved map if `expr` is:
    /// * An inline `Expr::Map` literal, or
    /// * A `Variable` bound to a map literal via `scalar_map_exprs`, or
    /// * A `Property` access on any of the above that itself yields a nested map.
    ///
    /// Returns `None` if the expression cannot be resolved to a compile-time map.
    fn try_get_map_pairs(&self, expr: &Expr) -> Option<Vec<(String, Expr)>> {
        match expr {
            Expr::Map(pairs) => Some(pairs.clone()),
            Expr::Variable { name, .. } => self.scalar_map_exprs.get(name.as_str()).cloned(),
            Expr::Property(inner_base, inner_key) => {
                let pairs = self.try_get_map_pairs(inner_base)?;
                let val = pairs
                    .into_iter()
                    .find(|(k, _)| k.as_str() == inner_key.as_str())
                    .map(|(_, v)| v)?;
                if let Expr::Map(inner_pairs) = val {
                    Some(inner_pairs)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Recursively evaluate a pure integer constant expression using Cypher integer semantics
/// (truncating division). Returns `None` if the expression contains any non-constant
/// sub-expression, non-integer literals, or overflow.
fn const_eval_integer(expr: &Expr) -> Option<i64> {
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Literal(Literal::Integer(n)) => Some(*n),
        E::Unary(UnaryOp::Neg, e) => const_eval_integer(e)?.checked_neg(),
        E::Unary(UnaryOp::Pos, e) => const_eval_integer(e),
        E::Add(a, b) => const_eval_integer(a)?.checked_add(const_eval_integer(b)?),
        E::Sub(a, b) => const_eval_integer(a)?.checked_sub(const_eval_integer(b)?),
        E::Mul(a, b) => const_eval_integer(a)?.checked_mul(const_eval_integer(b)?),
        E::Div(a, b) => {
            let denom = const_eval_integer(b)?;
            if denom == 0 {
                return None;
            }
            Some(const_eval_integer(a)? / denom)
        }
        E::Mod(a, b) => {
            let denom = const_eval_integer(b)?;
            if denom == 0 {
                return None;
            }
            Some(const_eval_integer(a)? % denom)
        }
        _ => None,
    }
}

/// Recursively evaluate a pure numeric constant expression to `f64`.
/// Sub-expressions that are purely integer-typed use integer arithmetic so that
/// `3 / 2` yields `1.0` (Cypher integer division), not `1.5`.
/// Returns `None` if any non-constant sub-expression is encountered.
fn const_eval_numeric(expr: &Expr) -> Option<f64> {
    // Fast path: if the entire expression is integer-typed, use integer semantics.
    if let Some(n) = const_eval_integer(expr) {
        return Some(n as f64);
    }
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Literal(Literal::Float(f)) => Some(*f),
        E::Unary(UnaryOp::Neg, e) => Some(-const_eval_numeric(e)?),
        E::Unary(UnaryOp::Pos, e) => const_eval_numeric(e),
        E::Add(a, b) => Some(const_eval_numeric(a)? + const_eval_numeric(b)?),
        E::Sub(a, b) => Some(const_eval_numeric(a)? - const_eval_numeric(b)?),
        E::Mul(a, b) => Some(const_eval_numeric(a)? * const_eval_numeric(b)?),
        E::Div(a, b) => {
            // Each operand is evaluated with its own type (integer-first semantics).
            // If one side is a float literal, the result is floating-point.
            let denom = const_eval_numeric(b)?;
            if denom == 0.0 {
                return None;
            }
            Some(const_eval_numeric(a)? / denom)
        }
        E::Mod(a, b) => {
            let denom = const_eval_numeric(b)?;
            if denom == 0.0 {
                return None;
            }
            Some(const_eval_numeric(a)? % denom)
        }
        E::Pow(a, b) => {
            let base = const_eval_numeric(a)?;
            let exp = const_eval_numeric(b)?;
            Some(base.powf(exp))
        }
        _ => None,
    }
}

/// Returns `true` if `var` appears as a direct operand to an arithmetic
/// operator (`%`, `/`, `*`, `-`, `^`) anywhere in `expr`.
/// Used to detect type-mismatch quantifiers at compile time.
fn quant_pred_uses_arithmetic(expr: &Expr, var: &str) -> bool {
    use crate::lqa::expr::Expr as E;
    match expr {
        E::Mod(a, b) | E::Div(a, b) | E::Mul(a, b) | E::Sub(a, b) | E::Pow(a, b) => {
            expr_contains_var(a, var)
                || expr_contains_var(b, var)
                || quant_pred_uses_arithmetic(a, var)
                || quant_pred_uses_arithmetic(b, var)
        }
        E::Unary(UnaryOp::Neg, a) => {
            expr_contains_var(a, var) || quant_pred_uses_arithmetic(a, var)
        }
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

/// Map a Cypher temporal component name to a SPARQL built-in function call on a
/// Evaluate a compile-time string expression.  Handles string literals and
/// string concatenation of literals (e.g. `'nam' + 'e'` → `"name"`).
/// Returns `None` for any expression with a runtime component.
fn lqa_eval_string_expr(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Literal(Literal::String(s)) => Some(s.clone()),
        Expr::Add(a, b) => {
            // String concatenation of two string expressions.
            let sa = lqa_eval_string_expr(a)?;
            let sb = lqa_eval_string_expr(b)?;
            Some(format!("{sa}{sb}"))
        }
        _ => None,
    }
}

/// Extract the canonical string value from a literal for use in temporal
/// component extraction.  Returns the raw lexical value for typed literals
/// (dates, durations) or plain string literals.
fn lqa_literal_str_value(lit: &Literal) -> Option<String> {
    match lit {
        Literal::String(s) => Some(s.clone()),
        Literal::TypedLiteral(v, _) => Some(v.clone()),
        _ => None,
    }
}

/// Build a SPARQL expression for an exotic temporal component using SPARQL
/// arithmetic built-ins.  Returns `None` for components that cannot be
/// expressed with standard SPARQL functions.
fn lqa_temporal_component_expr(component: &str, arg: SparExpr) -> Option<SparExpr> {
    use spargebra::algebra::Expression as SE;
    let xsd_int = NamedNode::new_unchecked(XSD_INTEGER);
    let lit_int = |n: i64| -> SparExpr {
        SparExpr::Literal(SparLit::new_typed_literal(n.to_string(), xsd_int.clone()))
    };
    let lit_flt = |n: f64| -> SparExpr {
        SparExpr::Literal(SparLit::new_typed_literal(
            format!("{n}"),
            NamedNode::new_unchecked(XSD_DOUBLE),
        ))
    };
    let month_expr = || SparExpr::FunctionCall(Function::Month, vec![arg.clone()]);
    let sec_expr = || SparExpr::FunctionCall(Function::Seconds, vec![arg.clone()]);

    match component {
        // quarter: 1-4
        // FLOOR((MONTH(?d) - 1) / 3) + 1
        "quarter" => Some(SE::Add(
            Box::new(SE::FunctionCall(
                Function::Floor,
                vec![SE::Divide(
                    Box::new(SE::Subtract(Box::new(month_expr()), Box::new(lit_int(1)))),
                    Box::new(lit_flt(3.0)),
                )],
            )),
            Box::new(lit_int(1)),
        )),
        // millisecond: 0-999  (fractional seconds × 1000, mod 1000)
        // FLOOR((SECONDS(?d) - FLOOR(SECONDS(?d))) * 1000)
        "millisecond" | "millisecondOfSecond" | "millisecondsOfSecond" => Some(SE::FunctionCall(
            Function::Floor,
            vec![SE::Multiply(
                Box::new(SE::Subtract(
                    Box::new(sec_expr()),
                    Box::new(SE::FunctionCall(Function::Floor, vec![sec_expr()])),
                )),
                Box::new(lit_flt(1000.0)),
            )],
        )),
        // microsecond: 0-999 within the millisecond
        // FLOOR((SECONDS(?d) - FLOOR(SECONDS(?d))) * 1000000) % 1000
        "microsecond" | "microsecondOfSecond" | "microsecondsOfSecond" => {
            let ms_total = SE::FunctionCall(
                Function::Floor,
                vec![SE::Multiply(
                    Box::new(SE::Subtract(
                        Box::new(sec_expr()),
                        Box::new(SE::FunctionCall(Function::Floor, vec![sec_expr()])),
                    )),
                    Box::new(lit_flt(1_000_000.0)),
                )],
            );
            // ms_total % 1000 — spargebra doesn't expose modulo directly; use
            // ms_total - FLOOR(ms_total / 1000) * 1000 instead.
            Some(SE::Subtract(
                Box::new(ms_total.clone()),
                Box::new(SE::Multiply(
                    Box::new(SE::FunctionCall(
                        Function::Floor,
                        vec![SE::Divide(Box::new(ms_total), Box::new(lit_flt(1000.0)))],
                    )),
                    Box::new(lit_flt(1000.0)),
                )),
            ))
        }
        _ => None,
    }
}

/// Map a Cypher temporal component name to a SPARQL expression on a runtime
/// temporal variable.
///
/// Uses string-based extraction (SUBSTR/STRAFTER) rather than SPARQL built-ins
/// (YEAR/MONTH/DAY/…) because temporal values stored as node properties in the
/// TCK and in typical usage are plain string literals ("1984-10-11"), not typed
/// xsd:date/dateTime literals.  SPARQL built-ins return null on plain strings;
/// SUBSTR works correctly on both typed and plain string representations.
fn lqa_temporal_component_fn(component: &str, arg: SparExpr) -> Option<SparExpr> {
    use spargebra::algebra::Function;

    let xsi_nn = NamedNode::new_unchecked(XSD_INTEGER);
    let xsd_dec = NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#decimal");

    let str_e = SparExpr::FunctionCall(Function::Str, vec![arg.clone()]);

    let dim = |n: i64| SparExpr::Literal(SparLit::new_typed_literal(n.to_string(), xsi_nn.clone()));
    let slit = |s: &str| SparExpr::Literal(SparLit::new_simple_literal(s.to_owned()));
    let int_cast = |e: SparExpr| SparExpr::FunctionCall(Function::Custom(xsi_nn.clone()), vec![e]);
    let dec_cast = |e: SparExpr| SparExpr::FunctionCall(Function::Custom(xsd_dec.clone()), vec![e]);
    let substr2 = |s: SparExpr, start: i64, len: i64| {
        SparExpr::FunctionCall(Function::SubStr, vec![s, dim(start), dim(len)])
    };
    let contains_f =
        |s: SparExpr, sub: &str| SparExpr::FunctionCall(Function::Contains, vec![s, slit(sub)]);
    let strafter_f =
        |s: SparExpr, delim: &str| SparExpr::FunctionCall(Function::StrAfter, vec![s, slit(delim)]);
    let floor_f = |e: SparExpr| SparExpr::FunctionCall(Function::Floor, vec![e]);
    let ceil_f = |e: SparExpr| SparExpr::FunctionCall(Function::Ceil, vec![e]);
    let add = |a: SparExpr, b: SparExpr| SparExpr::Add(Box::new(a), Box::new(b));
    let sub = |a: SparExpr, b: SparExpr| SparExpr::Subtract(Box::new(a), Box::new(b));
    let mul = |a: SparExpr, b: SparExpr| SparExpr::Multiply(Box::new(a), Box::new(b));
    let div = |a: SparExpr, b: SparExpr| SparExpr::Divide(Box::new(a), Box::new(b));

    // Time portion helper: IF(CONTAINS(str, "T"), STRAFTER(str, "T"), str).
    // Works for both "HH:MM:SS…" (time-only) and "YYYY-MM-DDTHH:MM:SS…" (datetime).
    let time_str = || {
        SparExpr::If(
            Box::new(contains_f(str_e.clone(), "T")),
            Box::new(strafter_f(str_e.clone(), "T")),
            Box::new(str_e.clone()),
        )
    };
    let t_str = time_str();

    // Fractional-second helper: raw string after "."  in the time part.
    let frac_raw = strafter_f(t_str.clone(), ".");
    let frac9 = substr2(
        SparExpr::FunctionCall(Function::Concat, vec![frac_raw, slit("000000000")]),
        1,
        9,
    );

    match component {
        // ── Date components ─────────────────────────────────────────────────
        "year" => Some(int_cast(substr2(str_e.clone(), 1, 4))),
        "month" => Some(int_cast(substr2(str_e.clone(), 6, 2))),
        "day" => Some(int_cast(substr2(str_e.clone(), 9, 2))),
        "quarter" | "quarterOfYear" => {
            // CEIL(DEC(month) / 3) — correct for all 12 months.
            // month 1-3 → 1, 4-6 → 2, 7-9 → 3, 10-12 → 4.
            let month_i = int_cast(substr2(str_e.clone(), 6, 2));
            let month_d = dec_cast(month_i);
            Some(int_cast(ceil_f(div(
                month_d,
                SparExpr::Literal(SparLit::new_typed_literal("3".to_owned(), xsd_dec.clone())),
            ))))
        }
        // ── Time components ─────────────────────────────────────────────────
        "hour" => Some(int_cast(substr2(t_str.clone(), 1, 2))),
        "minute" => Some(int_cast(substr2(t_str.clone(), 4, 2))),
        "second" => Some(int_cast(substr2(t_str.clone(), 7, 2))),
        "millisecond" | "millisecondOfSecond" | "millisecondsOfSecond" => {
            Some(int_cast(substr2(frac9.clone(), 1, 3)))
        }
        "microsecond" | "microsecondOfSecond" | "microsecondsOfSecond" => {
            Some(int_cast(substr2(frac9.clone(), 1, 6)))
        }
        "nanosecond" | "nanosecondOfSecond" | "nanosecondsOfSecond" => Some(int_cast(frac9)),
        // ── Timezone ────────────────────────────────────────────────────────
        // TZ() works on typed xsd:dateTime; for plain strings we'd need string
        // extraction. Accept TZ() for now — timezone tests use typed literals.
        "timezone" | "offset" => Some(SparExpr::FunctionCall(Function::Tz, vec![arg])),
        // ── Arithmetic-based exotic components ──────────────────────────────
        other => lqa_temporal_component_expr(other, arg),
    }
}

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
        // weekDay is a synonym for dayOfWeek (1=Monday … 7=Sunday)
        "weekDay" => {
            let y = comps.year?;
            let m = comps.month?;
            let d = comps.day?;
            let (_, _, dow) = crate::translator::cypher::date_to_iso_week(y, m, d);
            dow
        }
        // epochSeconds: seconds since Unix epoch 1970-01-01T00:00:00Z
        "epochSeconds" => {
            let y = comps.year?;
            let mo = comps.month?;
            let d = comps.day?;
            // temporal_epoch returns absolute day count from year 1; subtract Unix epoch offset.
            const UNIX_EPOCH_DAY: i64 = 719163; // temporal_epoch(1970, 1, 1)
            let epoch_days =
                crate::translator::cypher::temporal_epoch(y, mo, d) as i64 - UNIX_EPOCH_DAY;
            let h = comps.hour.unwrap_or(0);
            let mi = comps.minute.unwrap_or(0);
            let s = comps.second.unwrap_or(0);
            epoch_days * 86_400 + h * 3600 + mi * 60 + s
        }
        // epochMillis: milliseconds since Unix epoch
        "epochMillis" => {
            let y = comps.year?;
            let mo = comps.month?;
            let d = comps.day?;
            const UNIX_EPOCH_DAY: i64 = 719163; // temporal_epoch(1970, 1, 1)
            let epoch_days =
                crate::translator::cypher::temporal_epoch(y, mo, d) as i64 - UNIX_EPOCH_DAY;
            let h = comps.hour.unwrap_or(0);
            let mi = comps.minute.unwrap_or(0);
            let s = comps.second.unwrap_or(0);
            let ns = comps.ns.unwrap_or(0) as i64;
            let secs = epoch_days * 86_400 + h * 3600 + mi * 60 + s;
            secs * 1_000 + ns / 1_000_000
        }
        "offset" | "timezone" | "offsetMinutes" | "offsetSeconds" => return None,
        _ => return None,
    };
    Some(SparExpr::Literal(SparLit::new_typed_literal(
        n.to_string(),
        NamedNode::new_unchecked(XSD_INTEGER),
    )))
}

/// Recursively serialize a fully-literal `Expr` to the Cypher string representation,
/// e.g. `[1, 'foo', null]` → `"[1, 'foo', null]"` and `{a: 1}` → `"{a: 1}"`.
/// Returns `None` if any sub-expression is not a compile-time literal.
/// Returns `true` if `e` contains a `Literal::Null` anywhere inside a
/// `Expr::List` or `Expr::Map` nesting.  Used to guard equality/IN lowering:
/// when null is present, Cypher's three-valued semantics diverge from plain
/// string comparison.
fn lqa_expr_contains_null(e: &Expr) -> bool {
    match e {
        Expr::Literal(Literal::Null) => true,
        Expr::List(items) => items.iter().any(lqa_expr_contains_null),
        Expr::Map(pairs) => pairs.iter().any(|(_, v)| lqa_expr_contains_null(v)),
        _ => false,
    }
}

fn lqa_serialize_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::List(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(lqa_serialize_literal)
                .collect::<Option<_>>()?;
            Some(format!("[{}]", parts.join(", ")))
        }
        Expr::Map(pairs) => {
            let entries: Vec<String> = pairs
                .iter()
                .map(|(k, v)| lqa_serialize_literal(v).map(|s| format!("{k}: {s}")))
                .collect::<Option<_>>()?;
            Some(format!("{{{}}}", entries.join(", ")))
        }
        _ => lqa_lit_elem_str(e),
    }
}

/// Evaluate `range(start, end [, step])` arguments to a `Vec<i64>`, returning
/// `None` if any argument is not a statically-evaluable integer constant.
/// Mirrors the logic in the legacy translator's `range()` implementation.
fn eval_range_to_integers(args: &[Expr]) -> Option<Vec<i64>> {
    let start = const_eval_integer(args.first()?)?;
    let end_val = const_eval_integer(args.get(1)?)?;
    let step: i64 = if let Some(step_arg) = args.get(2) {
        let s = const_eval_integer(step_arg)?;
        if s == 0 {
            return None; // step=0 is invalid; let legacy raise the error
        }
        s
    } else {
        1
    };
    let mut items = Vec::new();
    let mut i = start;
    // Guard: cap at 100_000 elements to avoid memory bombs on large ranges.
    while (step > 0 && i <= end_val) || (step < 0 && i >= end_val) {
        items.push(i);
        i += step;
        if items.len() > 100_000 {
            return None; // too large; let legacy handle
        }
    }
    Some(items)
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

/// Returns true if `proj` is `toLower(variable)` (the list-map-lower pattern).
fn is_tolower_proj(proj: &Expr, variable: &str) -> bool {
    match proj {
        Expr::FunctionCall { name, args, .. } => {
            name.eq_ignore_ascii_case("toLower")
                && args.len() == 1
                && matches!(&args[0], Expr::Variable { name: v, .. } if v == variable)
        }
        _ => false,
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
