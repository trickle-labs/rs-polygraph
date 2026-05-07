//! Runner — executes a [`QuerySpec`] end-to-end against an Oxigraph store and
//! reports the [`ComparisonOutcome`].

use oxigraph::{
    model::Term as OxTerm,
    sparql::{QueryResults, SparqlEvaluator},
    store::Store,
};
use polygraph::{sparql_engine::TargetEngine, TranspileOutput, Transpiler};

use crate::oracle::{Comparison, ComparisonOutcome};
use crate::rdf_projection::{to_insert_data, DEFAULT_BASE};
use crate::suite::QuerySpec;
use crate::value::Value;

/// One difftest run report — composable so callers can aggregate.
#[derive(Debug)]
pub struct RunReport {
    pub name: String,
    pub spec_ref: String,
    pub outcome: ComparisonOutcome,
    pub sparql: String,
    pub actual_columns: Vec<String>,
    pub actual_rows: Vec<Vec<Value>>,
    pub error: Option<String>,
}

impl RunReport {
    pub fn passed(&self) -> bool {
        self.error.is_none() && matches!(self.outcome, ComparisonOutcome::Match)
    }
}

struct DifftestEngine;

impl TargetEngine for DifftestEngine {
    fn supports_rdf_star(&self) -> bool {
        true
    }
    fn supports_federation(&self) -> bool {
        false
    }
    fn base_iri(&self) -> Option<&str> {
        Some(DEFAULT_BASE)
    }
}

/// Run a single curated [`QuerySpec`].
pub fn run_one(spec: &QuerySpec) -> RunReport {
    let engine = DifftestEngine;
    let store = match Store::new() {
        Ok(s) => s,
        Err(e) => {
            return RunReport {
                name: spec.name.clone(),
                spec_ref: spec.spec_ref.clone(),
                outcome: ComparisonOutcome::Match,
                sparql: String::new(),
                actual_columns: vec![],
                actual_rows: vec![],
                error: Some(format!("oxigraph store init: {e}")),
            }
        }
    };

    // Load fixture.
    let insert = to_insert_data(&spec.fixture, DEFAULT_BASE);
    if !insert.ends_with("{}") {
        if let Err(e) = store.update(insert.as_str()) {
            return RunReport {
                name: spec.name.clone(),
                spec_ref: spec.spec_ref.clone(),
                outcome: ComparisonOutcome::Match,
                sparql: insert.clone(),
                actual_columns: vec![],
                actual_rows: vec![],
                error: Some(format!("fixture load: {e}")),
            };
        }
    }

    // Transpile.
    let sparql = match Transpiler::cypher_to_sparql(&spec.cypher, &engine) {
        Ok(TranspileOutput::Complete { sparql, .. }) => sparql,
        Ok(TranspileOutput::Continuation { .. }) => {
            return RunReport {
                name: spec.name.clone(),
                spec_ref: spec.spec_ref.clone(),
                outcome: ComparisonOutcome::Match,
                sparql: String::new(),
                actual_columns: vec![],
                actual_rows: vec![],
                error: Some("L2 continuation: out of scope for curated suite".into()),
            }
        }
        Ok(TranspileOutput::Write { updates, select }) => {
            // Execute UPDATE statements.
            for upd in &updates {
                if let Err(e) = store.update(upd.as_str()) {
                    return RunReport {
                        name: spec.name.clone(),
                        spec_ref: spec.spec_ref.clone(),
                        outcome: ComparisonOutcome::Match,
                        sparql: upd.clone(),
                        actual_columns: vec![],
                        actual_rows: vec![],
                        error: Some(format!("write update: {e}")),
                    };
                }
            }
            match select {
                None => {
                    // Write-only: return empty results (no ComparisonOutcome needed).
                    return RunReport {
                        name: spec.name.clone(),
                        spec_ref: spec.spec_ref.clone(),
                        outcome: ComparisonOutcome::Match,
                        sparql: String::new(),
                        actual_columns: vec![],
                        actual_rows: vec![],
                        error: None,
                    };
                }
                Some(sel) => match *sel {
                    TranspileOutput::Complete { sparql, .. } => sparql,
                    _ => {
                        return RunReport {
                            name: spec.name.clone(),
                            spec_ref: spec.spec_ref.clone(),
                            outcome: ComparisonOutcome::Match,
                            sparql: String::new(),
                            actual_columns: vec![],
                            actual_rows: vec![],
                            error: Some("unexpected non-Complete select in Write output".into()),
                        };
                    }
                },
            }
        }
        Err(e) => {
            return RunReport {
                name: spec.name.clone(),
                spec_ref: spec.spec_ref.clone(),
                outcome: ComparisonOutcome::Match,
                sparql: String::new(),
                actual_columns: vec![],
                actual_rows: vec![],
                error: Some(format!("transpile: {e}")),
            }
        }
    };

    // Execute.
    #[expect(deprecated)]
    let res = store.query_opt(
        sparql.as_str(),
        SparqlEvaluator::new()
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:unsupported-pow"),
                |args| {
                    let a = match args.first()? {
                        OxTerm::Literal(l) => l.value().parse::<f64>().ok()?,
                        _ => return None,
                    };
                    let b = match args.get(1)? {
                        OxTerm::Literal(l) => l.value().parse::<f64>().ok()?,
                        _ => return None,
                    };
                    Some(OxTerm::Literal(
                        oxigraph::model::Literal::new_typed_literal(
                            a.powf(b).to_string(),
                            oxigraph::model::NamedNode::new_unchecked(
                                "http://www.w3.org/2001/XMLSchema#double",
                            ),
                        ),
                    ))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:duration-add"),
                |args| {
                    let a = match args.first()? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let b = match args.get(1)? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let r = polygraph::translator::cypher::duration_add_str(&a, &b)?;
                    Some(OxTerm::Literal(
                        oxigraph::model::Literal::new_simple_literal(r),
                    ))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:duration-sub"),
                |args| {
                    let a = match args.first()? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let b = match args.get(1)? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let r = polygraph::translator::cypher::duration_sub_str(&a, &b)?;
                    Some(OxTerm::Literal(
                        oxigraph::model::Literal::new_simple_literal(r),
                    ))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:duration-mul-num"),
                |args| {
                    let dur = match args.first()? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let num = match args.get(1)? {
                        OxTerm::Literal(l) => l.value().parse::<f64>().ok()?,
                        _ => return None,
                    };
                    let r = polygraph::translator::cypher::duration_mul_num_str(&dur, num)?;
                    Some(OxTerm::Literal(
                        oxigraph::model::Literal::new_simple_literal(r),
                    ))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:duration-div-num"),
                |args| {
                    let dur = match args.first()? {
                        OxTerm::Literal(l) => l.value().to_owned(),
                        _ => return None,
                    };
                    let num = match args.get(1)? {
                        OxTerm::Literal(l) => l.value().parse::<f64>().ok()?,
                        _ => return None,
                    };
                    let r = polygraph::translator::cypher::duration_div_num_str(&dur, num)?;
                    Some(OxTerm::Literal(
                        oxigraph::model::Literal::new_simple_literal(r),
                    ))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:list-contains"),
                |args| {
                    let list = match args.first()? { OxTerm::Literal(l) => l.value().to_owned(), _ => return None };
                    let value_str = match args.get(1)? {
                        OxTerm::Literal(l) => {
                            let dt = l.datatype().as_str();
                            if dt.ends_with("#boolean") || dt.ends_with("#integer")
                                || dt.ends_with("#long") || dt.ends_with("#double")
                                || dt.ends_with("#float") || dt.ends_with("#decimal")
                            {
                                l.value().to_owned()
                            } else {
                                format!("'{}'", l.value().replace('\\', "\\\\").replace('\'', "\\'"))
                            }
                        }
                        _ => return None,
                    };
                    let result = polygraph::translator::cypher::list_contains_str(&list, &value_str);
                    Some(OxTerm::Literal(oxigraph::model::Literal::new_typed_literal(
                        result.to_string(),
                        oxigraph::model::NamedNode::new_unchecked("http://www.w3.org/2001/XMLSchema#boolean"),
                    )))
                },
            )
            .with_custom_function(
                oxigraph::model::NamedNode::new_unchecked("urn:polygraph:list-map-lower"),
                |args| {
                    use oxigraph::model::Term as OxTerm;
                    let list = match args.first()? { OxTerm::Literal(l) => l.value().to_owned(), _ => return None };
                    let result = polygraph::translator::cypher::list_map_lower_str(&list);
                    Some(OxTerm::Literal(oxigraph::model::Literal::new_simple_literal(result)))
                },
            ),
    );
    let (actual_columns, actual_rows) = match res {
        Err(e) => {
            return RunReport {
                name: spec.name.clone(),
                spec_ref: spec.spec_ref.clone(),
                outcome: ComparisonOutcome::Match,
                sparql,
                actual_columns: vec![],
                actual_rows: vec![],
                error: Some(format!("execute: {e}")),
            }
        }
        Ok(QueryResults::Solutions(mut solutions)) => {
            let vars: Vec<String> = solutions
                .variables()
                .iter()
                .map(|v| v.as_str().to_owned())
                .collect();
            let mut rows: Vec<Vec<Value>> = Vec::new();
            for sol in solutions.by_ref() {
                match sol {
                    Err(e) => {
                        return RunReport {
                            name: spec.name.clone(),
                            spec_ref: spec.spec_ref.clone(),
                            outcome: ComparisonOutcome::Match,
                            sparql,
                            actual_columns: vars,
                            actual_rows: vec![],
                            error: Some(format!("solution: {e}")),
                        };
                    }
                    Ok(s) => {
                        let row: Vec<Value> = vars
                            .iter()
                            .map(|v| match s.get(v.as_str()) {
                                None => Value::Null,
                                Some(t) => term_to_value(t),
                            })
                            .collect();
                        rows.push(row);
                    }
                }
            }
            (vars, rows)
        }
        Ok(QueryResults::Boolean(b)) => (vec!["__bool__".into()], vec![vec![Value::Bool(b)]]),
        Ok(QueryResults::Graph(_)) => (vec![], vec![]),
    };

    let outcome = Comparison::compare(
        &spec.expected.columns,
        &spec.expected.rows,
        &actual_columns,
        &actual_rows,
        spec.expected.order.into(),
    );

    RunReport {
        name: spec.name.clone(),
        spec_ref: spec.spec_ref.clone(),
        outcome,
        sparql,
        actual_columns,
        actual_rows,
        error: None,
    }
}

/// Run every `*.toml` curated query under the given directory.
pub fn run_curated(dir: &std::path::Path) -> Vec<RunReport> {
    let mut reports = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    entries.sort();
    for path in entries {
        let s = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                reports.push(RunReport {
                    name: path.display().to_string(),
                    spec_ref: String::new(),
                    outcome: ComparisonOutcome::Match,
                    sparql: String::new(),
                    actual_columns: vec![],
                    actual_rows: vec![],
                    error: Some(format!("read {path:?}: {e}")),
                });
                continue;
            }
        };
        let spec = match QuerySpec::from_toml_str(&s) {
            Ok(s) => s,
            Err(e) => {
                reports.push(RunReport {
                    name: path.display().to_string(),
                    spec_ref: String::new(),
                    outcome: ComparisonOutcome::Match,
                    sparql: String::new(),
                    actual_columns: vec![],
                    actual_rows: vec![],
                    error: Some(format!("parse {path:?}: {e}")),
                });
                continue;
            }
        };
        reports.push(run_one(&spec));
    }
    reports
}

fn term_to_value(t: &OxTerm) -> Value {
    match t {
        OxTerm::NamedNode(n) => {
            // A node IRI under our base — strip the prefix and report as a node ref.
            let s = n.as_str();
            if let Some(local) = s.strip_prefix(DEFAULT_BASE) {
                Value::Node(local.to_owned())
            } else {
                Value::String(s.to_owned())
            }
        }
        OxTerm::BlankNode(b) => Value::Node(b.as_str().to_owned()),
        OxTerm::Literal(l) => {
            let dt = l.datatype().as_str().to_owned();
            let v = l.value();
            match dt.as_str() {
                "http://www.w3.org/2001/XMLSchema#integer"
                | "http://www.w3.org/2001/XMLSchema#long"
                | "http://www.w3.org/2001/XMLSchema#int" => v
                    .parse::<i64>()
                    .map(Value::Int)
                    .unwrap_or(Value::String(v.to_owned())),
                "http://www.w3.org/2001/XMLSchema#double"
                | "http://www.w3.org/2001/XMLSchema#float"
                | "http://www.w3.org/2001/XMLSchema#decimal" => v
                    .parse::<f64>()
                    .map(Value::Float)
                    .unwrap_or(Value::String(v.to_owned())),
                "http://www.w3.org/2001/XMLSchema#boolean" => Value::Bool(v == "true"),
                "http://www.w3.org/2001/XMLSchema#string"
                | "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" => {
                    Value::String(v.to_owned())
                }
                _ => Value::String(v.to_owned()),
            }
        }
        // Older oxigraph 0.5 may have additional Term variants; treat unknowns as opaque strings.
        _ => Value::String(format!("{t:?}")),
    }
}
