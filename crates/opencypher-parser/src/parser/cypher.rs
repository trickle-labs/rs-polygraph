use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser;

use crate::ast::cypher::{
    AggregateExpr, CallClause, Clause, CompOp, CreateClause, CypherQuery, DeleteClause, Direction,
    Expression, Ident, Label, Literal, MapLiteral, MatchClause, MergeClause, NodePattern,
    OrderByClause, Pattern, PatternElement, PatternList, QuantifierKind, RangeQuantifier,
    RelationshipPattern, RemoveClause, RemoveItem, ReturnClause, ReturnItem, ReturnItems,
    SetClause, SetItem, SortItem, UnwindClause, WhereClause, WithClause,
};
use crate::error::ParseError;

// The #[grammar] path is relative to the Cargo.toml (crate root).
#[derive(Parser)]
#[grammar = "grammars/cypher.pest"]
struct CypherPestParser;

/// Parse an openCypher query string into a typed [`CypherQuery`] AST.
///
/// # Errors
///
/// Returns [`ParseError::Parse`] if the input does not conform to the
/// supported openCypher subset.
pub fn parse(input: &str) -> Result<CypherQuery, ParseError> {
    let mut pairs = CypherPestParser::parse(Rule::query, input).map_err(|e| {
        let span = match e.location {
            pest::error::InputLocation::Pos(p) => format!("pos:{p}"),
            pest::error::InputLocation::Span((s, end)) => format!("span:{s}..{end}"),
        };
        ParseError::Syntax {
            span,
            message: e.to_string(),
        }
    })?;
    let query_pair = pairs.next().unwrap(); // Rule::query guaranteed by grammar
    build_query(query_pair)
}

// ── Top-level builders ────────────────────────────────────────────────────────

fn build_query(pair: Pair<Rule>) -> Result<CypherQuery, ParseError> {
    // query = { SOI ~ statement ~ EOI }
    let statement = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::statement)
        .expect("grammar guarantees a statement");
    build_statement(statement)
}

fn build_statement(pair: Pair<Rule>) -> Result<CypherQuery, ParseError> {
    // statement = { single_query ~ (union_marker ~ single_query)* }
    // single_query = { clause+ }
    let mut clauses = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::single_query => {
                for sq_child in child.into_inner() {
                    if sq_child.as_rule() == Rule::clause {
                        let inner = sq_child
                            .into_inner()
                            .next()
                            .expect("clause always has an inner rule");
                        let clause = build_clause(inner)?;
                        clauses.push(clause);
                    }
                }
            }
            Rule::union_marker => {
                let all = child.into_inner().any(|p| p.as_rule() == Rule::kw_ALL);
                clauses.push(Clause::Union { all });
            }
            _ => {}
        }
    }
    Ok(CypherQuery { clauses })
}

fn build_clause(inner: Pair<Rule>) -> Result<Clause, ParseError> {
    match inner.as_rule() {
        Rule::match_clause => Ok(Clause::Match(build_match_clause(inner)?)),
        Rule::with_clause => Ok(Clause::With(build_with_clause(inner)?)),
        Rule::return_clause => Ok(Clause::Return(build_return_clause(inner)?)),
        Rule::unwind_clause => Ok(Clause::Unwind(build_unwind_clause(inner)?)),
        Rule::create_clause => Ok(Clause::Create(build_create_clause(inner)?)),
        Rule::merge_clause => Ok(Clause::Merge(build_merge_clause(inner)?)),
        Rule::set_clause => Ok(Clause::Set(build_set_clause(inner)?)),
        Rule::delete_clause => Ok(Clause::Delete(build_delete_clause(inner)?)),
        Rule::remove_clause => Ok(Clause::Remove(build_remove_clause(inner)?)),
        Rule::call_subquery => Err(ParseError::UnsupportedFeature {
            feature: "CALL { } subquery".to_string(),
        }),
        Rule::call_clause => Ok(Clause::Call(build_call_clause(inner)?)),
        Rule::foreach_clause => Err(ParseError::UnsupportedFeature {
            feature: "FOREACH clause".to_string(),
        }),
        _ => unreachable!("unexpected clause rule: {:?}", inner.as_rule()),
    }
}

// ── Clause builders ───────────────────────────────────────────────────────────

fn build_match_clause(pair: Pair<Rule>) -> Result<MatchClause, ParseError> {
    // match_clause = { optional_marker? ~ kw_MATCH ~ pattern_list ~ where_clause? }
    let mut optional = false;
    let mut pattern = None;
    let mut where_ = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::optional_marker => optional = true,
            Rule::kw_MATCH => {}
            Rule::pattern_list => pattern = Some(build_pattern_list(inner)?),
            Rule::where_clause => where_ = Some(build_where_clause(inner)?),
            _ => {}
        }
    }
    Ok(MatchClause {
        optional,
        pattern: pattern.expect("grammar guarantees pattern_list"),
        where_,
    })
}

fn build_where_clause(pair: Pair<Rule>) -> Result<WhereClause, ParseError> {
    // where_clause = { kw_WHERE ~ expression }
    let expr_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::expression)
        .expect("grammar guarantees expression");
    Ok(WhereClause {
        expression: build_expression(expr_pair)?,
    })
}

fn build_return_clause(pair: Pair<Rule>) -> Result<ReturnClause, ParseError> {
    // return_clause = { kw_RETURN ~ projection_body }
    let pb = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::projection_body)
        .expect("return_clause has projection_body");
    let (distinct, items, order_by, skip, limit) = build_projection_body(pb)?;
    Ok(ReturnClause {
        distinct,
        items,
        order_by,
        skip,
        limit,
    })
}

fn build_with_clause(pair: Pair<Rule>) -> Result<WithClause, ParseError> {
    // with_clause = { kw_WITH ~ projection_body ~ where_clause? }
    let mut pb_opt = None;
    let mut where_ = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_WITH => {}
            Rule::projection_body => pb_opt = Some(inner),
            Rule::where_clause => where_ = Some(build_where_clause(inner)?),
            _ => {}
        }
    }
    let (distinct, items, order_by, skip, limit) =
        build_projection_body(pb_opt.expect("with_clause has projection_body"))?;
    Ok(WithClause {
        distinct,
        items,
        where_,
        order_by,
        skip,
        limit,
    })
}

// ── Projection body ──────────────────────────────────────────────────────────

/// Returns (distinct, items, order_by, skip, limit).
#[allow(clippy::type_complexity)]
fn build_projection_body(
    pair: Pair<Rule>,
) -> Result<
    (
        bool,
        ReturnItems,
        Option<OrderByClause>,
        Option<Expression>,
        Option<Expression>,
    ),
    ParseError,
> {
    // projection_body = { distinct_marker? ~ return_items ~ order_by_clause? ~ skip_clause? ~ limit_clause? }
    let mut distinct = false;
    let mut items = ReturnItems::All;
    let mut order_by = None;
    let mut skip = None;
    let mut limit = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::distinct_marker => distinct = true,
            Rule::return_items => items = build_return_items(inner)?,
            Rule::order_by_clause => order_by = Some(build_order_by_clause(inner)?),
            Rule::skip_clause => {
                skip = Some(build_expression(
                    inner
                        .into_inner()
                        .find(|p| p.as_rule() == Rule::expression)
                        .expect("skip_clause has expression"),
                )?);
            }
            Rule::limit_clause => {
                limit = Some(build_expression(
                    inner
                        .into_inner()
                        .find(|p| p.as_rule() == Rule::expression)
                        .expect("limit_clause has expression"),
                )?);
            }
            _ => {}
        }
    }
    Ok((distinct, items, order_by, skip, limit))
}

fn build_return_items(pair: Pair<Rule>) -> Result<ReturnItems, ParseError> {
    // return_items = { star_projection | explicit_items }
    let inner = pair
        .into_inner()
        .next()
        .expect("return_items has one child");
    match inner.as_rule() {
        Rule::star_projection => Ok(ReturnItems::All),
        Rule::explicit_items => {
            let items: Result<Vec<_>, _> = inner
                .into_inner()
                .filter(|p| p.as_rule() == Rule::return_item)
                .map(build_return_item)
                .collect();
            Ok(ReturnItems::Explicit(items?))
        }
        _ => unreachable!(),
    }
}

fn build_return_item(pair: Pair<Rule>) -> Result<ReturnItem, ParseError> {
    // return_item = { expression ~ (kw_AS ~ variable)? }
    let mut expr = None;
    let mut alias = None;
    let mut saw_as = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expression => expr = Some(build_expression(inner)?),
            Rule::kw_AS => saw_as = true,
            Rule::variable if saw_as => alias = Some(ident_text(&inner)),
            _ => {}
        }
    }
    Ok(ReturnItem {
        expression: expr.expect("grammar guarantees expression"),
        alias,
    })
}

// ── Pattern builders ──────────────────────────────────────────────────────────

fn build_pattern_list(pair: Pair<Rule>) -> Result<PatternList, ParseError> {
    // pattern_list = { pattern ~ ("," ~ pattern)* }
    let patterns: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::pattern)
        .map(build_pattern)
        .collect();
    Ok(PatternList(patterns?))
}

fn build_pattern(pair: Pair<Rule>) -> Result<Pattern, ParseError> {
    // pattern = { (variable ~ "=")? ~ anonymous_pattern_part }
    let mut variable = None;
    let mut elements: Option<Vec<PatternElement>> = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::variable => {
                variable = Some(ident_text(&inner));
            }
            Rule::anonymous_pattern_part => {
                let app_inner = inner
                    .into_inner()
                    .next()
                    .expect("anonymous_pattern_part has child");
                elements = Some(match app_inner.as_rule() {
                    Rule::pattern_element => build_pattern_element(app_inner)?,
                    Rule::shortest_path_pattern => {
                        // Also unwrap and build the inner pattern_element
                        let pe = app_inner
                            .into_inner()
                            .find(|p| p.as_rule() == Rule::pattern_element)
                            .expect("shortest_path_pattern has pattern_element");
                        build_pattern_element(pe)?
                    }
                    _ => unreachable!(
                        "unexpected anonymous_pattern_part: {:?}",
                        app_inner.as_rule()
                    ),
                });
            }
            _ => {}
        }
    }
    let elements = elements.expect("grammar guarantees anonymous_pattern_part");
    Ok(Pattern { variable, elements })
}

fn build_pattern_element(pair: Pair<Rule>) -> Result<Vec<PatternElement>, ParseError> {
    // pattern_element = { node_pattern ~ pattern_element_chain* | "(" ~ pattern_element ~ ")" }
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => {
                elements.push(PatternElement::Node(build_node_pattern(inner)?));
            }
            Rule::pattern_element_chain => {
                // pattern_element_chain = { rel_pattern ~ node_pattern }
                for link_inner in inner.into_inner() {
                    match link_inner.as_rule() {
                        Rule::rel_pattern => {
                            elements
                                .push(PatternElement::Relationship(build_rel_pattern(link_inner)?));
                        }
                        Rule::node_pattern => {
                            elements.push(PatternElement::Node(build_node_pattern(link_inner)?));
                        }
                        _ => {}
                    }
                }
            }
            Rule::pattern_element => {
                // Parenthesized pattern_element: recurse
                elements.extend(build_pattern_element(inner)?);
            }
            _ => {}
        }
    }
    Ok(elements)
}

fn build_node_labels(pair: Pair<Rule>) -> Result<Vec<Label>, ParseError> {
    let mut labels = Vec::new();
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::node_label => {
                // node_label = { ":" ~ "!"? ~ schema_name }
                // The "!"? is an optional literal; it doesn't create a named child pair.
                // We collect the schema_name and ignore the negation flag for now
                // (flat label semantics; Phase 3 will add proper boolean label algebra).
                if let Some(name_pair) = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::schema_name)
                {
                    labels.push(name_pair.as_str().trim_matches('`').to_string());
                }
            }
            Rule::gql_label_more => {
                // gql_label_more = { (":"|"|"|"&") ~ "!"? ~ schema_name }
                // Connector (|/&/:) and optional ! don't create named pairs; collect the name.
                if let Some(name_pair) = child
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::schema_name)
                {
                    labels.push(name_pair.as_str().trim_matches('`').to_string());
                }
            }
            _ => {}
        }
    }
    Ok(labels)
}

fn build_node_pattern(pair: Pair<Rule>) -> Result<NodePattern, ParseError> {
    // node_pattern = { "(" ~ variable? ~ node_labels? ~ properties? ~ where_clause? ~ ")" }
    let mut variable = None;
    let mut labels = Vec::new();
    let mut properties = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::variable => variable = Some(ident_text(&inner)),
            Rule::node_labels => {
                labels = build_node_labels(inner)?;
            }
            Rule::properties => properties = Some(build_map_literal(inner)?),
            // where_clause inside a node pattern (GQL inline filter) is parsed but
            // semantically ignored in Phase 2; Phase 3 will scope it properly.
            Rule::where_clause => {}
            _ => {}
        }
    }
    Ok(NodePattern {
        variable,
        labels,
        properties,
    })
}

fn build_rel_pattern(pair: Pair<Rule>) -> Result<RelationshipPattern, ParseError> {
    // rel_pattern = { left_arrow ~ rel_body ~ rel_dash
    //               | rel_dash ~ rel_body ~ right_arrow
    //               | rel_dash ~ rel_body ~ rel_dash }
    let mut has_left_arrow = false;
    let mut has_right_arrow = false;
    let mut variable = None;
    let mut rel_types = Vec::new();
    let mut range = None;
    let mut properties = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::left_arrow => has_left_arrow = true,
            Rule::right_arrow => has_right_arrow = true,
            Rule::full_arrow => {
                // <--> : undirected / any-direction — same semantics as --
            }
            Rule::rel_dash => {}
            Rule::rel_body => {
                for rb in inner.into_inner() {
                    match rb.as_rule() {
                        Rule::variable => variable = Some(ident_text(&rb)),
                        Rule::rel_type_list => {
                            for rt in rb.into_inner() {
                                if rt.as_rule() == Rule::rel_type_elem {
                                    rel_types.push(rt.as_str().trim_matches('`').to_string());
                                }
                            }
                        }
                        Rule::range_literal => range = Some(build_range_literal(rb)?),
                        Rule::properties => properties = Some(build_map_literal(rb)?),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    let direction = match (has_left_arrow, has_right_arrow) {
        (true, false) => Direction::Left,
        (false, true) => Direction::Right,
        _ => Direction::Both,
    };

    Ok(RelationshipPattern {
        variable,
        direction,
        rel_types,
        properties,
        range,
    })
}

fn build_range_literal(pair: Pair<Rule>) -> Result<RangeQuantifier, ParseError> {
    // range_literal = { "*" ~ (integer_literal ~ (".." ~ integer_literal?)?)? }
    let text = pair.as_str().trim();
    if text == "*" {
        return Ok(RangeQuantifier {
            lower: None,
            upper: None,
        });
    }
    // Strip leading "*"
    let rest = text.trim_start_matches('*').trim();
    if rest.is_empty() {
        return Ok(RangeQuantifier {
            lower: None,
            upper: None,
        });
    }
    if let Some((lo, hi)) = rest.split_once("..") {
        let lower = if lo.trim().is_empty() {
            None
        } else {
            Some(lo.trim().parse::<u64>().unwrap_or(0))
        };
        let upper = if hi.trim().is_empty() {
            None
        } else {
            Some(hi.trim().parse::<u64>().unwrap_or(0))
        };
        Ok(RangeQuantifier { lower, upper })
    } else {
        let n = rest.parse::<u64>().unwrap_or(0);
        Ok(RangeQuantifier {
            lower: Some(n),
            upper: Some(n),
        })
    }
}

// ── Phase 4 clause builders ───────────────────────────────────────────────────

fn build_unwind_clause(pair: Pair<Rule>) -> Result<UnwindClause, ParseError> {
    // unwind_clause = { kw_UNWIND ~ expression ~ kw_AS ~ variable }
    let mut expr = None;
    let mut var = None;
    let mut saw_as = false;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_UNWIND => {}
            Rule::expression => expr = Some(build_expression(inner)?),
            Rule::kw_AS => saw_as = true,
            Rule::variable if saw_as => var = Some(ident_text(&inner)),
            _ => {}
        }
    }
    Ok(UnwindClause {
        expression: expr.expect("unwind_clause has expression"),
        variable: var.expect("unwind_clause has variable"),
    })
}

fn build_create_clause(pair: Pair<Rule>) -> Result<CreateClause, ParseError> {
    // create_clause = { kw_CREATE ~ pattern_list }
    let pl_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::pattern_list)
        .expect("create_clause has pattern_list");
    Ok(CreateClause {
        pattern: build_pattern_list(pl_pair)?,
    })
}

fn build_merge_clause(pair: Pair<Rule>) -> Result<MergeClause, ParseError> {
    // merge_clause = { kw_MERGE ~ pattern ~ merge_action* }
    // merge_action = { kw_ON ~ (kw_MATCH | kw_CREATE) ~ set_clause }
    use crate::ast::cypher::MergeAction;
    let mut pattern = None;
    let mut actions = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::pattern => {
                pattern = Some(build_pattern(inner)?);
            }
            Rule::merge_action => {
                let mut is_create = false;
                let mut set_items = Vec::new();
                for child in inner.into_inner() {
                    match child.as_rule() {
                        Rule::kw_CREATE => is_create = true,
                        Rule::kw_MATCH => is_create = false,
                        Rule::set_clause => {
                            let sc = build_set_clause(child)?;
                            set_items = sc.items;
                        }
                        _ => {}
                    }
                }
                actions.push(MergeAction {
                    on_create: is_create,
                    items: set_items,
                });
            }
            _ => {}
        }
    }
    Ok(MergeClause {
        pattern: pattern.expect("merge_clause has pattern"),
        actions,
    })
}

fn build_set_clause(pair: Pair<Rule>) -> Result<SetClause, ParseError> {
    // set_clause = { kw_SET ~ set_item ~ ("," ~ set_item)* }
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::set_item)
        .map(build_set_item)
        .collect();
    Ok(SetClause { items: items? })
}

fn build_set_item(pair: Pair<Rule>) -> Result<SetItem, ParseError> {
    // set_item = { prop_access_expr ~ "=" ~ expression
    //            | variable ~ "+=" ~ map_literal
    //            | variable ~ "=" ~ expression }
    let mut children: Vec<_> = pair.into_inner().collect();
    // Detect variant by first child rule
    match children[0].as_rule() {
        Rule::prop_access_expr => {
            // prop_access_expr = { (variable | "(" ~ variable ~ ")") ~ "." ~ prop_name }
            let mut acc_inner = children.remove(0).into_inner();
            // Skip any leading "(" — the first variable is what we want.
            let variable = {
                let first = acc_inner
                    .next()
                    .expect("prop_access_expr has variable or paren");
                if first.as_rule() == Rule::variable {
                    ident_text(&first)
                } else {
                    // Parenthesized: skip the "(" and get the variable inside.
                    acc_inner
                        .find(|p| p.as_rule() == Rule::variable)
                        .map(|p| ident_text(&p))
                        .expect("prop_access_expr paren has variable")
                }
            };
            let key_pair = acc_inner
                .find(|p| p.as_rule() == Rule::prop_name)
                .expect("prop_access_expr has prop_name");
            let key = key_pair.as_str().trim_matches('`').to_string();
            // children[0] is now the expression (after removing prop_access_expr)
            let value = build_expression(
                children
                    .into_iter()
                    .find(|p| p.as_rule() == Rule::expression)
                    .expect("set_item property has expression"),
            )?;
            Ok(SetItem::Property {
                variable,
                key,
                value,
            })
        }
        Rule::variable => {
            let var_name = ident_text(&children[0]);
            // Determine which set_item variant based on what follows the variable:
            //   n += {map}  → MergeMap
            //   n:Label     → SetLabel
            //   n = expr    → NodeReplace
            let has_map = children.iter().any(|p| p.as_rule() == Rule::map_literal);
            let has_labels = children.iter().any(|p| p.as_rule() == Rule::node_labels);
            if has_map {
                let map_pair = children
                    .into_iter()
                    .find(|p| p.as_rule() == Rule::map_literal)
                    .expect("merge map has map_literal");
                let map = build_map_literal(map_pair)?;
                Ok(SetItem::MergeMap {
                    variable: var_name,
                    map,
                })
            } else if has_labels {
                let labels_pair = children
                    .into_iter()
                    .find(|p| p.as_rule() == Rule::node_labels)
                    .expect("set_item label has node_labels");
                let labels = build_node_labels(labels_pair)?;
                Ok(SetItem::SetLabel {
                    variable: var_name,
                    labels,
                })
            } else {
                let expr_pair = children
                    .into_iter()
                    .find(|p| p.as_rule() == Rule::expression)
                    .expect("set_item replace has expression");
                let value = build_expression(expr_pair)?;
                Ok(SetItem::NodeReplace {
                    variable: var_name,
                    value,
                })
            }
        }
        _ => unreachable!(
            "unexpected set_item first child: {:?}",
            children[0].as_rule()
        ),
    }
}

fn build_delete_clause(pair: Pair<Rule>) -> Result<DeleteClause, ParseError> {
    // delete_clause = { detach_marker? ~ kw_DELETE ~ expression ~ ("," ~ expression)* }
    let mut detach = false;
    let mut exprs = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::detach_marker => detach = true,
            Rule::kw_DELETE => {}
            Rule::expression => exprs.push(build_expression(inner)?),
            _ => {}
        }
    }
    Ok(DeleteClause {
        detach,
        expressions: exprs,
    })
}

fn build_remove_clause(pair: Pair<Rule>) -> Result<RemoveClause, ParseError> {
    // remove_clause = { kw_REMOVE ~ remove_item ~ ("," ~ remove_item)* }
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::remove_item)
        .map(build_remove_item)
        .collect();
    Ok(RemoveClause { items: items? })
}

fn build_remove_item(pair: Pair<Rule>) -> Result<RemoveItem, ParseError> {
    // remove_item = { prop_access_expr | variable ~ node_labels }
    let mut children: Vec<_> = pair.into_inner().collect();
    match children[0].as_rule() {
        Rule::prop_access_expr => {
            let mut acc_inner = children.remove(0).into_inner();
            let variable = {
                let first = acc_inner.next().expect("prop_access_expr var");
                if first.as_rule() == Rule::variable {
                    ident_text(&first)
                } else {
                    acc_inner
                        .find(|p| p.as_rule() == Rule::variable)
                        .map(|p| ident_text(&p))
                        .expect("prop_access_expr paren var")
                }
            };
            let key = acc_inner
                .find(|p| p.as_rule() == Rule::prop_name)
                .expect("prop_access_expr key")
                .as_str()
                .trim_matches('`')
                .to_string();
            Ok(RemoveItem::Property { variable, key })
        }
        Rule::variable => {
            let variable = ident_text(&children[0]);
            let mut labels: Vec<Label> = Vec::new();
            for child in children.iter().skip(1) {
                if child.as_rule() == Rule::node_labels {
                    labels = build_node_labels(child.clone())?;
                }
            }
            Ok(RemoveItem::Label { variable, labels })
        }
        _ => unreachable!("unexpected remove_item: {:?}", children[0].as_rule()),
    }
}

fn build_call_clause(pair: Pair<Rule>) -> Result<CallClause, ParseError> {
    // call_clause = { kw_CALL ~ proc_name ~ ("(" ~ call_args? ~ ")")? ~ yield_clause? }
    let mut procedure = String::new();
    let mut args = Vec::new();
    let mut yields = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_CALL => {}
            Rule::proc_name => procedure = inner.as_str().to_string(),
            Rule::call_args => {
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::expression {
                        args.push(build_expression(child)?);
                    }
                }
            }
            Rule::yield_clause => {
                // yield_clause = { kw_YIELD ~ (yield_star | yield_items) ~ where_clause? }
                for yc in inner.into_inner() {
                    match yc.as_rule() {
                        Rule::kw_YIELD => {}
                        Rule::yield_star => {} // YIELD * — all fields; not tracked in yields vec
                        Rule::yield_items => {
                            for yi in yc.into_inner() {
                                if yi.as_rule() == Rule::yield_item {
                                    // yield_item = { (schema_name ~ kw_AS)? ~ variable }
                                    for v in yi.into_inner() {
                                        if v.as_rule() == Rule::variable {
                                            yields.push(ident_text(&v));
                                        }
                                    }
                                }
                            }
                        }
                        Rule::where_clause => {} // WHERE inside YIELD; not stored
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(CallClause {
        procedure,
        args,
        yields,
    })
}

// ── ORDER BY builder ──────────────────────────────────────────────────────────

fn build_order_by_clause(pair: Pair<Rule>) -> Result<OrderByClause, ParseError> {
    // order_by_clause = { kw_ORDER ~ kw_BY ~ sort_item ~ ("," ~ sort_item)* }
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::sort_item)
        .map(build_sort_item)
        .collect();
    Ok(OrderByClause { items: items? })
}

fn build_sort_item(pair: Pair<Rule>) -> Result<SortItem, ParseError> {
    // sort_item = { expression ~ sort_direction? }
    let mut expr = None;
    let mut descending = false;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expression => expr = Some(build_expression(inner)?),
            Rule::sort_direction => {
                // sort_direction = { kw_ASCENDING | kw_DESCENDING | kw_ASC | kw_DESC }
                let dir = inner.into_inner().next().expect("sort_direction has child");
                descending = matches!(dir.as_rule(), Rule::kw_DESC | Rule::kw_DESCENDING);
            }
            _ => {}
        }
    }
    Ok(SortItem {
        expression: expr.expect("sort_item has expression"),
        descending,
    })
}

// ── Map literal builder ───────────────────────────────────────────────────────

fn build_map_literal(pair: Pair<Rule>) -> Result<MapLiteral, ParseError> {
    // map_literal = { "{" ~ (map_entry ~ ("," ~ map_entry)*)? ~ "}" }
    let map_pair = if pair.as_rule() == Rule::properties {
        pair.into_inner()
            .find(|p| p.as_rule() == Rule::map_literal)
            .expect("properties wraps map_literal")
    } else {
        pair
    };

    let mut entries = Vec::new();
    for entry in map_pair.into_inner() {
        if entry.as_rule() == Rule::map_entry {
            // map_entry = { prop_name ~ ":" ~ expression }
            let mut key = None;
            let mut val = None;
            for part in entry.into_inner() {
                match part.as_rule() {
                    Rule::prop_name => {
                        key = Some(part.as_str().trim_matches('`').to_string());
                    }
                    Rule::expression => val = Some(build_expression(part)?),
                    _ => {}
                }
            }
            entries.push((
                key.expect("map_entry has prop_name"),
                val.expect("map_entry has expression"),
            ));
        }
    }
    Ok(entries)
}

// ── Expression builder ────────────────────────────────────────────────────────

fn build_expression(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // expression = { or_expr }
    let inner = pair.into_inner().next().expect("expression wraps or_expr");
    build_or_expr(inner)
}

fn build_or_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // or_expr = { xor_expr ~ (kw_OR ~ xor_expr)* }
    let mut children: Vec<_> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::xor_expr)
        .collect();
    let first = build_xor_expr(children.remove(0))?;
    children.into_iter().try_fold(first, |acc, p| {
        Ok(Expression::Or(Box::new(acc), Box::new(build_xor_expr(p)?)))
    })
}

fn build_xor_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children: Vec<_> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::and_expr)
        .collect();
    let first = build_and_expr(children.remove(0))?;
    children.into_iter().try_fold(first, |acc, p| {
        Ok(Expression::Xor(Box::new(acc), Box::new(build_and_expr(p)?)))
    })
}

fn build_and_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children: Vec<_> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::not_expr)
        .collect();
    let first = build_not_expr(children.remove(0))?;
    children.into_iter().try_fold(first, |acc, p| {
        Ok(Expression::And(Box::new(acc), Box::new(build_not_expr(p)?)))
    })
}

fn build_not_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // not_expr = { kw_NOT ~ not_expr | comparison_expr }
    let mut children = pair.into_inner();
    let first = children.next().expect("not_expr has child");
    match first.as_rule() {
        Rule::kw_NOT => {
            // The second inner pair is the nested not_expr
            let nested = children.next().expect("NOT is followed by not_expr");
            Ok(Expression::Not(Box::new(build_not_expr(nested)?)))
        }
        Rule::comparison_expr => build_comparison_expr(first),
        _ => unreachable!("unexpected not_expr child: {:?}", first.as_rule()),
    }
}

fn build_comparison_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // comparison_expr = { add_sub_expr ~ comparison_suffix* }
    let mut inners = pair.into_inner();
    let lhs_pair = inners.next().expect("comparison_expr has add_sub_expr");
    let mut current = build_add_sub_expr(lhs_pair)?;

    // Apply each comparison suffix left-to-right.
    // e.g. `(a AND b) IS NULL = (b AND a) IS NULL` becomes
    // Comparison(IsNull((a AND b)), Eq, IsNull((b AND a)))
    for suffix in inners {
        current = build_comparison_suffix(current, suffix)?;
    }
    Ok(current)
}

fn build_comparison_suffix(
    lhs: Expression,
    pair: Pair<Rule>,
) -> Result<Expression, ParseError> {
    // Peek at the first child to determine which variant.
    let mut children = pair.into_inner().peekable();
    let first = children.next().expect("comparison_suffix has children");

    match first.as_rule() {
        Rule::comp_op => {
            let op = match first.as_str() {
                "=" => CompOp::Eq,
                "<>" => CompOp::Ne,
                "<=" => CompOp::Le,
                ">=" => CompOp::Ge,
                "<" => CompOp::Lt,
                ">" => CompOp::Gt,
                other => {
                    return Err(ParseError::Syntax {
                        span: String::new(),
                        message: format!("unknown comparison operator: {other}"),
                    })
                }
            };
            // RHS is comparison_expr.  Detect chained comparisons at the parser level:
            //   `a < b = c` → comparison_expr(b = c) has a comp_op comparison_suffix
            //   `a = (b = c)` → comparison_expr((b=c)) has NO comparison_suffix child
            //   `a = b IS NULL` → comparison_expr(b IS NULL) has IS NULL suffix (not comp_op)
            //
            // Only expand when the inner comparison_expr has a comparison_suffix whose
            // first token is a comp_op (a binary comparison).  IS NULL / IN / STARTS WITH
            // etc. are unary/keyword suffixes and do NOT trigger chaining; they keep the
            // "null predicate takes precedence over comparison" rule intact.
            let rhs_pair = children
                .next()
                .expect("comp_op is followed by comparison_expr");

            // Check whether any inner comparison_suffix starts with comp_op.
            let inner_has_comp_suffix = rhs_pair
                .clone()
                .into_inner()
                .filter(|p| p.as_rule() == Rule::comparison_suffix)
                .any(|sfx| {
                    sfx.into_inner()
                        .next()
                        .map(|ch| ch.as_rule() == Rule::comp_op)
                        .unwrap_or(false)
                });

            if inner_has_comp_suffix {
                // Chained comparison: extract the "middle" operand from rhs's add_sub_expr,
                // then recursively process the remaining comparison suffixes.
                let mut rhs_inner = rhs_pair.into_inner();
                let mid_pair = rhs_inner
                    .next()
                    .expect("comparison_expr starts with add_sub_expr");
                let mid = build_add_sub_expr(mid_pair)?;
                // Process the rest of the chained comparison starting from `mid`.
                let mut current = mid.clone();
                for sfx in rhs_inner {
                    current = build_comparison_suffix(current, sfx)?;
                }
                // Expand to (lhs op mid) AND (mid chain rest).
                let left = Expression::Comparison(Box::new(lhs), op, Box::new(mid));
                Ok(Expression::And(Box::new(left), Box::new(current)))
            } else {
                // Simple or parenthesized RHS — keep as a straightforward Comparison.
                let rhs = build_comparison_expr(rhs_pair)?;
                Ok(Expression::Comparison(Box::new(lhs), op, Box::new(rhs)))
            }
        }
        Rule::regex_op => {
            let rhs_pair = children.next().expect("=~ is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::RegexMatch,
                Box::new(rhs),
            ))
        }
        Rule::kw_IS => {
            // IS NULL or IS NOT NULL
            let next = children.next().expect("IS is followed by something");
            if next.as_rule() == Rule::kw_NOT {
                // IS NOT NULL
                Ok(Expression::IsNotNull(Box::new(lhs)))
            } else {
                // IS NULL (next is kw_NULL)
                Ok(Expression::IsNull(Box::new(lhs)))
            }
        }
        Rule::kw_IN => {
            let rhs_pair = children.next().expect("IN is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::In,
                Box::new(rhs),
            ))
        }
        Rule::kw_NOT => {
            // NOT IN expr
            let _kw_in = children.next().expect("NOT IN: kw_IN expected");
            let rhs_pair = children.next().expect("NOT IN is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Not(Box::new(Expression::Comparison(
                Box::new(lhs),
                CompOp::In,
                Box::new(rhs),
            ))))
        }
        Rule::kw_STARTS => {
            // STARTS WITH expr
            let _kw_with = children.next(); // kw_WITH
            let rhs_pair = children
                .next()
                .expect("STARTS WITH is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::StartsWith,
                Box::new(rhs),
            ))
        }
        Rule::kw_ENDS => {
            // ENDS WITH expr
            let _kw_with = children.next(); // kw_WITH
            let rhs_pair = children
                .next()
                .expect("ENDS WITH is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::EndsWith,
                Box::new(rhs),
            ))
        }
        Rule::kw_CONTAINS => {
            let rhs_pair = children
                .next()
                .expect("CONTAINS is followed by add_sub_expr");
            let rhs = build_add_sub_expr(rhs_pair)?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::Contains,
                Box::new(rhs),
            ))
        }
        _ => unreachable!(
            "unexpected comparison_suffix first child: {:?}",
            first.as_rule()
        ),
    }
}

fn build_add_sub_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // add_sub_expr = { mul_div_expr ~ (add_sub_op ~ mul_div_expr)* }
    let mut children = pair.into_inner().peekable();
    let first = children.next().expect("add_sub_expr has mul_div_expr");
    let mut acc = build_mul_div_expr(first)?;

    while let Some(op_pair) = children.next() {
        let operand_pair = children.next().expect("operator is followed by operand");
        let rhs = build_mul_div_expr(operand_pair)?;
        acc = match op_pair.as_str() {
            "+" => Expression::Add(Box::new(acc), Box::new(rhs)),
            "-" => Expression::Subtract(Box::new(acc), Box::new(rhs)),
            _ => unreachable!(),
        };
    }
    Ok(acc)
}

fn build_mul_div_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // mul_div_expr = { power_expr ~ (mul_div_op ~ power_expr)* }
    let mut children = pair.into_inner().peekable();
    let first = children.next().expect("mul_div_expr has power_expr");
    let mut acc = build_power_expr(first)?;

    while let Some(op_pair) = children.next() {
        let operand_pair = children.next().expect("operator is followed by operand");
        let rhs = build_power_expr(operand_pair)?;
        acc = match op_pair.as_str() {
            "*" => Expression::Multiply(Box::new(acc), Box::new(rhs)),
            "/" => Expression::Divide(Box::new(acc), Box::new(rhs)),
            "%" => Expression::Modulo(Box::new(acc), Box::new(rhs)),
            _ => unreachable!(),
        };
    }
    Ok(acc)
}

fn build_unary_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // unary_expr = { unary_minus | unary_plus | non_arith_expr }
    let inner = pair.into_inner().next().expect("unary_expr has child");
    match inner.as_rule() {
        Rule::unary_minus => {
            // unary_minus = { "-" ~ unary_expr }
            let operand = inner
                .into_inner()
                .next()
                .expect("unary_minus has unary_expr");
            // Special case: -2^63 = i64::MIN.  The literal 2^63 overflows i64 by itself,
            // but when negated it is exactly representable as i64::MIN.
            let raw = operand.as_str().trim();
            const MIN_INT_MAG: u64 = i64::MAX as u64 + 1;
            let is_min_int = raw
                .parse::<u64>()
                .map(|n| n == MIN_INT_MAG)
                .unwrap_or(false)
                || raw
                    .strip_prefix("0x")
                    .or_else(|| raw.strip_prefix("0X"))
                    .and_then(|h| u64::from_str_radix(h, 16).map(|n| n == MIN_INT_MAG).ok())
                    .unwrap_or(false)
                || raw
                    .strip_prefix("0o")
                    .or_else(|| raw.strip_prefix("0O"))
                    .and_then(|o| u64::from_str_radix(o, 8).map(|n| n == MIN_INT_MAG).ok())
                    .unwrap_or(false);
            if is_min_int {
                return Ok(Expression::Literal(Literal::Integer(i64::MIN)));
            }
            Ok(Expression::Negate(Box::new(build_unary_expr(operand)?)))
        }
        Rule::unary_plus => {
            // unary_plus = { "+" ~ unary_expr } — no-op, just unwrap
            let operand = inner
                .into_inner()
                .next()
                .expect("unary_plus has unary_expr");
            build_unary_expr(operand)
        }
        Rule::non_arith_expr => build_non_arith_expr(inner),
        _ => unreachable!("unexpected unary_expr child: {:?}", inner.as_rule()),
    }
}

fn build_power_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // power_expr = { unary_expr ~ ("^" ~ unary_expr)* }
    // Left-associative: a^b^c = (a^b)^c
    let mut children = pair.into_inner();
    let mut acc = build_unary_expr(children.next().expect("power_expr has unary_expr"))?;
    for next in children {
        acc = Expression::Power(Box::new(acc), Box::new(build_unary_expr(next)?));
    }
    Ok(acc)
}

fn build_non_arith_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // non_arith_expr = { atom ~ (property_lookup | slice_access | subscript_access)* ~ node_labels? }
    let mut children = pair.into_inner();
    let atom_pair = children.next().expect("non_arith_expr has atom");
    let mut acc = build_atom(atom_pair)?;
    for postfix in children {
        match postfix.as_rule() {
            Rule::property_lookup => {
                // property_lookup = { "." ~ prop_name }
                let key = postfix
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::prop_name)
                    .expect("property_lookup has prop_name")
                    .as_str()
                    .trim_matches('`')
                    .to_string();
                acc = Expression::Property(Box::new(acc), key);
            }
            Rule::slice_access => {
                // slice_access = { "[" ~ expression? ~ ".." ~ expression? ~ "]" }
                let mut start: Option<Box<Expression>> = None;
                let mut end: Option<Box<Expression>> = None;
                // Use the source span to distinguish start (before "..") from
                // end (after ".."). For "[..x]" the single expression is the end;
                // for "[x..]" it is the start; for "[x..y]" both are present.
                let dotdot_abs =
                    postfix.as_span().start() + postfix.as_str().find("..").unwrap_or(0);
                for child in postfix.into_inner() {
                    if child.as_rule() == Rule::expression {
                        if child.as_span().start() < dotdot_abs {
                            start = Some(Box::new(build_expression(child)?));
                        } else {
                            end = Some(Box::new(build_expression(child)?));
                        }
                    }
                }
                acc = Expression::ListSlice {
                    list: Box::new(acc),
                    start,
                    end,
                };
            }
            Rule::subscript_access => {
                // subscript_access = { "[" ~ expression ~ "]" }
                let idx_pair = postfix
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::expression)
                    .expect("subscript_access has expression");
                let idx = build_expression(idx_pair)?;
                acc = Expression::Subscript(Box::new(acc), Box::new(idx));
            }
            // node_labels at end of non_arith_expr: e.g. (n):Label — fold into a LabelCheck
            Rule::node_labels => {
                // Convert acc to a label check if it's a variable
                let labels = build_node_labels(postfix)?;
                if let Expression::Variable(var_name) = &acc {
                    acc = Expression::LabelCheck {
                        variable: var_name.clone(),
                        labels,
                    };
                }
                // If not a variable (e.g. result of property access), ignore label check
                // — this is an unusual edge case not needed for current TCK scenarios
            }
            _ => {}
        }
    }
    Ok(acc)
}

fn build_atom(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // atom = { float_literal | integer_literal | string_literal | boolean_literal
    //        | null_literal | list_literal | map_literal | "(" ~ expression ~ ")" | variable }
    let inner = pair.into_inner().next().expect("atom has child");
    match inner.as_rule() {
        Rule::integer_literal => {
            let n: i64 = inner.as_str().parse().map_err(|_| ParseError::Syntax {
                span: inner.as_str().to_string(),
                message: "integer literal out of range".to_string(),
            })?;
            Ok(Expression::Literal(Literal::Integer(n)))
        }
        Rule::hex_integer_literal => {
            let s = inner.as_str();
            let digits = &s[2..]; // strip "0x" / "0X"
            let n = i64::from_str_radix(digits, 16).map_err(|_| ParseError::Syntax {
                span: s.to_string(),
                message: "hexadecimal integer literal out of range".to_string(),
            })?;
            Ok(Expression::Literal(Literal::Integer(n)))
        }
        Rule::octal_integer_literal => {
            let s = inner.as_str();
            let digits = &s[2..]; // strip "0o" / "0O"
            let n = i64::from_str_radix(digits, 8).map_err(|_| ParseError::Syntax {
                span: s.to_string(),
                message: "octal integer literal out of range".to_string(),
            })?;
            Ok(Expression::Literal(Literal::Integer(n)))
        }
        Rule::float_literal => {
            let f: f64 = inner.as_str().parse().map_err(|_| ParseError::Syntax {
                span: inner.as_str().to_string(),
                message: "float literal out of range".to_string(),
            })?;
            if f.is_infinite() {
                return Err(ParseError::UnsupportedFeature {
                    feature: "FloatingPointOverflow: float literal value is too large".to_string(),
                });
            }
            Ok(Expression::Literal(Literal::Float(f)))
        }
        Rule::string_literal => {
            let raw = inner.as_str();
            // Strip outer quotes
            let content = &raw[1..raw.len() - 1];
            // Basic escape processing
            let s = unescape_string(content);
            Ok(Expression::Literal(Literal::String(s)))
        }
        Rule::boolean_literal => {
            let b = inner.as_str().eq_ignore_ascii_case("true");
            Ok(Expression::Literal(Literal::Boolean(b)))
        }
        Rule::null_literal => Ok(Expression::Literal(Literal::Null)),
        Rule::parameter => Err(ParseError::UnsupportedFeature {
            feature: format!("query parameter: {}", inner.as_str()),
        }),
        Rule::legacy_parameter => Err(ParseError::UnsupportedFeature {
            feature: format!("legacy parameter: {}", inner.as_str()),
        }),
        Rule::aggregate_expr => build_aggregate_expr(inner),
        Rule::case_expression => build_case_expression(inner),
        Rule::reduce_expr => Err(ParseError::UnsupportedFeature {
            feature: "REDUCE expression".to_string(),
        }),
        Rule::exists_subquery => {
            // exists_subquery = { kw_EXISTS ~ "{" ~ (statement | (pattern_list ~ where_clause?)) ~ "}" }
            // We currently only support the simple form: pattern_list with optional WHERE.
            let mut patterns: Option<PatternList> = None;
            let mut where_expr: Option<Box<Expression>> = None;
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::pattern_list => patterns = Some(build_pattern_list(child)?),
                    Rule::where_clause => {
                        where_expr = Some(Box::new(build_where_clause(child)?.expression));
                    }
                    Rule::statement => {
                        // Parse `EXISTS { ... }` body.
                        // Simple form: `EXISTS { MATCH pat [WHERE pred] [RETURN ...] }`
                        //   → ExistsSubquery { patterns, where_ }
                        // Full form: `EXISTS { MATCH ... WITH ... WHERE ... RETURN ... }`
                        //   → ExistsFullSubquery { clauses }
                        // Write clauses (CREATE, SET, DELETE, MERGE, REMOVE) → SyntaxError.
                        let mut single_queries: Vec<Pair<Rule>> = Vec::new();
                        for sq in child.into_inner() {
                            if sq.as_rule() == Rule::single_query {
                                single_queries.push(sq);
                            }
                        }
                        if single_queries.len() != 1 {
                            return Err(ParseError::UnsupportedFeature {
                                feature: "EXISTS subquery with UNION (Phase 4+)".to_string(),
                            });
                        }
                        let mut has_with = false;
                        let mut found_match: Option<Pair<Rule>> = None;
                        let mut found_where: Option<Pair<Rule>> = None;
                        let mut all_clause_pairs: Vec<Pair<Rule>> = Vec::new();
                        for cl in single_queries.remove(0).into_inner() {
                            if cl.as_rule() != Rule::clause {
                                continue;
                            }
                            // Peek at the inner rule to detect write clauses.
                            let inner_cl = cl
                                .clone()
                                .into_inner()
                                .next()
                                .expect("clause has inner rule");
                            match inner_cl.as_rule() {
                                Rule::create_clause
                                | Rule::set_clause
                                | Rule::delete_clause
                                | Rule::merge_clause
                                | Rule::remove_clause => {
                                    return Err(ParseError::UnsupportedFeature {
                                        feature: "EXISTS subquery with non-MATCH/RETURN clauses (Phase 4+)".to_string(),
                                    });
                                }
                                Rule::with_clause => {
                                    has_with = true;
                                }
                                Rule::match_clause if !has_with => {
                                    // Simple path: collect match/where for ExistsSubquery.
                                    if found_match.is_some() {
                                        // Promote to full form.
                                        has_with = true;
                                    } else {
                                        for mc_inner in inner_cl.into_inner() {
                                            match mc_inner.as_rule() {
                                                Rule::pattern_list => {
                                                    found_match = Some(mc_inner);
                                                }
                                                Rule::where_clause => {
                                                    found_where = Some(mc_inner);
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                _ => {} // return_clause, unwind_clause: ignored in simple path
                            }
                            all_clause_pairs.push(cl);
                        }

                        if has_with {
                            // Full form: parse all clauses via build_clause.
                            let mut full_clauses = Vec::new();
                            for cl_pair in all_clause_pairs {
                                let inner_cl =
                                    cl_pair.into_inner().next().expect("clause has inner rule");
                                full_clauses.push(build_clause(inner_cl)?);
                            }
                            return Ok(Expression::ExistsFullSubquery {
                                clauses: full_clauses,
                            });
                        }

                        let pl_pair =
                            found_match.ok_or_else(|| ParseError::UnsupportedFeature {
                                feature: "EXISTS subquery without MATCH".to_string(),
                            })?;
                        patterns = Some(build_pattern_list(pl_pair)?);
                        if let Some(wp) = found_where {
                            where_expr = Some(Box::new(build_where_clause(wp)?.expression));
                        }
                    }
                    _ => {}
                }
            }
            let pl = patterns.ok_or_else(|| ParseError::UnsupportedFeature {
                feature: "EXISTS subquery with no pattern".to_string(),
            })?;
            Ok(Expression::ExistsSubquery {
                patterns: pl,
                where_: where_expr,
            })
        }
        Rule::shortest_path_atom => Err(ParseError::UnsupportedFeature {
            feature: "shortestPath / allShortestPaths expression".to_string(),
        }),
        Rule::quantifier_expr => build_quantifier_expr(inner),
        Rule::function_call => build_function_call(inner),
        Rule::label_check => build_label_check(inner),
        Rule::list_comprehension => build_list_comprehension(inner),
        Rule::pattern_comprehension => build_pattern_comprehension(inner),
        Rule::pattern_predicate => {
            // pattern_predicate = { relationships_pattern }
            // relationships_pattern = { node_pattern ~ pattern_element_chain+ }
            let rp = inner
                .into_inner()
                .find(|p| p.as_rule() == Rule::relationships_pattern)
                .expect("pattern_predicate has relationships_pattern");
            let elements = build_relationships_pattern(rp)?;
            Ok(Expression::PatternPredicate(Pattern {
                variable: None,
                elements,
            }))
        }
        Rule::list_literal => {
            let items: Result<Vec<_>, _> = inner
                .into_inner()
                .filter(|p| p.as_rule() == Rule::expression)
                .map(build_expression)
                .collect();
            Ok(Expression::List(items?))
        }
        Rule::map_literal => {
            let entries = build_map_literal(inner)?;
            Ok(Expression::Map(entries))
        }
        Rule::expression => build_expression(inner),
        Rule::variable => Ok(Expression::Variable(ident_text(&inner))),
        _ => unreachable!("unexpected atom child: {:?}", inner.as_rule()),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn build_case_expression(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // case_expression = { kw_CASE ~ expression? ~ case_when+ ~ (kw_ELSE ~ expression)? ~ kw_END }
    let mut operand: Option<Box<Expression>> = None;
    let mut whens: Vec<(Expression, Expression)> = Vec::new();
    let mut else_expr: Option<Box<Expression>> = None;
    let mut found_else = false;
    let mut last_was_kw_else = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::kw_CASE | Rule::kw_END => {}
            Rule::kw_ELSE => {
                last_was_kw_else = true;
            }
            Rule::case_when => {
                // case_when = { kw_WHEN ~ expression ~ kw_THEN ~ expression }
                let mut exprs: Vec<Expression> = Vec::new();
                for c in child.into_inner() {
                    if c.as_rule() == Rule::expression {
                        exprs.push(build_expression(c)?);
                    }
                }
                if exprs.len() == 2 {
                    whens.push((exprs.remove(0), exprs.remove(0)));
                }
            }
            Rule::expression => {
                if last_was_kw_else {
                    else_expr = Some(Box::new(build_expression(child)?));
                    found_else = true;
                } else if whens.is_empty() {
                    // This is the operand (before any WHEN)
                    operand = Some(Box::new(build_expression(child)?));
                }
                last_was_kw_else = false;
            }
            _ => {
                last_was_kw_else = false;
            }
        }
    }
    let _ = found_else;
    Ok(Expression::CaseExpression {
        operand,
        whens,
        else_expr,
    })
}

fn build_quantifier_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // quantifier_expr = { (kw_ALL | kw_ANY | kw_NONE | kw_SINGLE) ~ "(" ~ filter_expression ~ ")" }
    let mut kind: Option<QuantifierKind> = None;
    let mut variable: Option<String> = None;
    let mut list: Option<Expression> = None;
    let mut predicate: Option<Box<Expression>> = None;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::kw_ALL => kind = Some(QuantifierKind::All),
            Rule::kw_ANY => kind = Some(QuantifierKind::Any),
            Rule::kw_NONE => kind = Some(QuantifierKind::None),
            Rule::kw_SINGLE => kind = Some(QuantifierKind::Single),
            Rule::filter_expression => {
                // filter_expression = { variable ~ kw_IN ~ expression ~ (kw_WHERE ~ expression)? }
                let mut exprs: Vec<Expression> = Vec::new();
                for c in child.into_inner() {
                    match c.as_rule() {
                        Rule::variable => variable = Some(ident_text(&c)),
                        Rule::expression => exprs.push(build_expression(c)?),
                        _ => {}
                    }
                }
                if !exprs.is_empty() {
                    list = Some(exprs.remove(0));
                }
                if !exprs.is_empty() {
                    predicate = Some(Box::new(exprs.remove(0)));
                }
            }
            _ => {}
        }
    }
    Ok(Expression::QuantifierExpr {
        kind: kind.expect("quantifier_expr has kind"),
        variable: variable.expect("filter_expression has variable"),
        list: Box::new(list.expect("filter_expression has list")),
        predicate,
    })
}

fn build_list_comprehension(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // list_comprehension = { "[" ~ filter_expression ~ ("|" ~ expression)? ~ "]" }
    // filter_expression = { variable ~ kw_IN ~ expression ~ (kw_WHERE ~ expression)? }
    let mut variable: Option<String> = None;
    let mut list: Option<Expression> = None;
    let mut predicate: Option<Box<Expression>> = None;
    let mut projection: Option<Box<Expression>> = None;
    let mut filter_done = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::filter_expression => {
                let mut exprs: Vec<Expression> = Vec::new();
                for c in child.into_inner() {
                    match c.as_rule() {
                        Rule::variable => variable = Some(ident_text(&c)),
                        Rule::expression => exprs.push(build_expression(c)?),
                        _ => {}
                    }
                }
                if !exprs.is_empty() {
                    list = Some(exprs.remove(0));
                }
                if !exprs.is_empty() {
                    predicate = Some(Box::new(exprs.remove(0)));
                }
                filter_done = true;
            }
            Rule::expression if filter_done => {
                // The | projection expression
                projection = Some(Box::new(build_expression(child)?));
            }
            _ => {}
        }
    }
    Ok(Expression::ListComprehension {
        variable: variable.expect("list_comprehension has variable"),
        list: Box::new(list.expect("list_comprehension has list")),
        predicate,
        projection,
    })
}

fn build_pattern_comprehension(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // pattern_comprehension = { "[" ~ (variable ~ "=")? ~ relationships_pattern ~ (kw_WHERE ~ expression)? ~ "|" ~ expression ~ "]" }
    let mut alias: Option<String> = None;
    let mut elements: Option<Vec<PatternElement>> = None;
    let mut predicate: Option<Box<Expression>> = None;
    let mut projection: Option<Box<Expression>> = None;
    let mut after_chain = false;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::variable if elements.is_none() => {
                alias = Some(ident_text(&child));
            }
            Rule::relationships_pattern => {
                elements = Some(build_relationships_pattern(child)?);
                after_chain = true;
            }
            Rule::expression if after_chain && predicate.is_none() => {
                // Could be the WHERE predicate (first expression after chain) or the projection.
                predicate = Some(Box::new(build_expression(child)?));
            }
            Rule::expression => {
                projection = Some(Box::new(build_expression(child)?));
            }
            _ => {}
        }
    }
    // If only one expression was seen, it's the projection (no WHERE clause).
    if predicate.is_some() && projection.is_none() {
        projection = predicate.take();
    }
    let pattern = Pattern {
        variable: alias.clone(),
        elements: elements.expect("pattern_comprehension has relationships_pattern"),
    };
    Ok(Expression::PatternComprehension {
        alias,
        pattern,
        predicate,
        projection: projection.expect("pattern_comprehension has projection"),
    })
}

/// Build elements from a `relationships_pattern` pair.
/// relationships_pattern = { node_pattern ~ pattern_element_chain+ }
fn build_relationships_pattern(pair: Pair<Rule>) -> Result<Vec<PatternElement>, ParseError> {
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => {
                elements.push(PatternElement::Node(build_node_pattern(inner)?));
            }
            Rule::pattern_element_chain => {
                for link_inner in inner.into_inner() {
                    match link_inner.as_rule() {
                        Rule::rel_pattern => {
                            elements
                                .push(PatternElement::Relationship(build_rel_pattern(link_inner)?));
                        }
                        Rule::node_pattern => {
                            elements.push(PatternElement::Node(build_node_pattern(link_inner)?));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(elements)
}

fn build_aggregate_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // aggregate_expr = { agg_call_expr }
    let inner = pair.into_inner().next().expect("aggregate_expr has child");
    match inner.as_rule() {
        Rule::count_expr => {
            // count_expr = { kw_COUNT ~ "(" ~ (count_star | (kw_DISTINCT? ~ expression)) ~ ")" }
            let mut distinct = false;
            let mut expr: Option<Box<Expression>> = None;
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::count_star => {} // COUNT(*) — expr stays None
                    Rule::kw_DISTINCT => distinct = true,
                    Rule::expression => expr = Some(Box::new(build_expression(child)?)),
                    _ => {}
                }
            }
            Ok(Expression::Aggregate(AggregateExpr::Count {
                distinct,
                expr,
            }))
        }
        Rule::agg_call_expr => {
            // agg_call_expr = { agg_func_name ~ "(" ~ kw_DISTINCT? ~ expression ~ ")" }
            let mut func_name = String::new();
            let mut distinct = false;
            let mut expr: Option<Box<Expression>> = None;
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::agg_func_name => {
                        func_name = child.as_str().to_ascii_lowercase();
                    }
                    Rule::kw_DISTINCT => distinct = true,
                    Rule::expression => expr = Some(Box::new(build_expression(child)?)),
                    _ => {}
                }
            }
            let e = expr.expect("agg_call_expr has expression");
            let agg = match func_name.as_str() {
                "sum" => AggregateExpr::Sum { distinct, expr: e },
                "avg" => AggregateExpr::Avg { distinct, expr: e },
                "min" => AggregateExpr::Min { distinct, expr: e },
                "max" => AggregateExpr::Max { distinct, expr: e },
                "collect" => AggregateExpr::Collect { distinct, expr: e },
                other => {
                    return Err(ParseError::Syntax {
                        span: other.to_string(),
                        message: format!("unknown aggregate function: {other}"),
                    })
                }
            };
            Ok(Expression::Aggregate(agg))
        }
        _ => unreachable!("unexpected aggregate_expr child: {:?}", inner.as_rule()),
    }
}

/// Extract the text of a `variable` rule, handling backtick-escaped identifiers.
fn ident_text(pair: &Pair<Rule>) -> Ident {
    // variable = { !(keyword ~ !ident_char) ~ (ident_escaped | ident) }
    let inner = pair
        .clone()
        .into_inner()
        .next()
        .expect("variable has an ident or ident_escaped child");
    inner.as_str().trim_matches('`').to_string()
}

fn build_function_call(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // function_call = { func_name ~ "(" ~ (kw_DISTINCT? ~ expression ~ ("," ~ expression)*)? ~ ")" }
    // func_name = @{ (ident ~ ".")* ~ ident }  — atomic, gives full dotted name
    let mut children = pair.into_inner().peekable();
    let name_pair = children.next().expect("function_call has func_name");
    // func_name is atomic — as_str() gives the full name including namespace dots
    let name = name_pair.as_str().to_string();
    let mut distinct = false;
    let mut args = Vec::new();
    for child in children {
        match child.as_rule() {
            Rule::kw_DISTINCT => distinct = true,
            Rule::expression => args.push(build_expression(child)?),
            _ => {}
        }
    }
    Ok(Expression::FunctionCall {
        name,
        distinct,
        args,
    })
}

fn build_label_check(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // label_check = { variable ~ node_labels }
    let mut children = pair.into_inner();
    let var_pair = children.next().expect("label_check has variable");
    let variable = ident_text(&var_pair);
    let mut labels = Vec::new();
    for child in children {
        if child.as_rule() == Rule::node_labels {
            labels = build_node_labels(child)?;
        }
    }
    Ok(Expression::LabelCheck { variable, labels })
}

/// Unescape a Cypher string literal body (content between quotes).
fn unescape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some(c2) => out.push(c2),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::cypher::*;

    fn parse_ok(q: &str) -> CypherQuery {
        parse(q).unwrap_or_else(|e| panic!("parse failed for {q:?}: {e}"))
    }

    // --- Round-trip tests -------------------------------------------------------

    #[test]
    fn match_return_node() {
        let q = parse_ok("MATCH (n) RETURN n");
        assert_eq!(q.clauses.len(), 2);
        assert!(matches!(q.clauses[0], Clause::Match(_)));
        assert!(matches!(q.clauses[1], Clause::Return(_)));
    }

    #[test]
    fn match_node_with_label() {
        let q = parse_ok("MATCH (n:Person) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Node(node) = &pat.elements[0] {
                assert_eq!(node.labels, vec!["Person"]);
            } else {
                panic!("expected node");
            }
        } else {
            panic!("expected match");
        }
    }

    #[test]
    fn match_node_with_multiple_labels() {
        let q = parse_ok("MATCH (n:Person:Employee) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Node(node) = &pat.elements[0] {
                assert_eq!(node.labels, vec!["Person", "Employee"]);
            } else {
                panic!("expected node");
            }
        } else {
            panic!("expected match");
        }
    }

    #[test]
    fn match_node_with_property() {
        let q = parse_ok("MATCH (n:Person {name: 'Alice'}) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Node(node) = &pat.elements[0] {
                let props = node.properties.as_ref().unwrap();
                assert_eq!(props[0].0, "name");
                assert_eq!(
                    props[0].1,
                    Expression::Literal(Literal::String("Alice".to_string()))
                );
            } else {
                panic!("expected node");
            }
        }
    }

    #[test]
    fn match_relationship_right() {
        let q = parse_ok("MATCH (a)-[:KNOWS]->(b) RETURN a, b");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            assert_eq!(pat.elements.len(), 3);
            if let PatternElement::Relationship(r) = &pat.elements[1] {
                assert_eq!(r.direction, Direction::Right);
                assert_eq!(r.rel_types, vec!["KNOWS"]);
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn match_relationship_left() {
        let q = parse_ok("MATCH (a)<-[:KNOWS]-(b) RETURN a");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Relationship(r) = &pat.elements[1] {
                assert_eq!(r.direction, Direction::Left);
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn match_relationship_undirected() {
        let q = parse_ok("MATCH (a)-[:KNOWS]-(b) RETURN a");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Relationship(r) = &pat.elements[1] {
                assert_eq!(r.direction, Direction::Both);
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn optional_match() {
        let q = parse_ok("OPTIONAL MATCH (n:Person) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            assert!(m.optional);
        } else {
            panic!("expected match");
        }
    }

    #[test]
    fn match_where_return() {
        let q = parse_ok("MATCH (n:Person) WHERE n.age > 30 RETURN n.name");
        assert_eq!(q.clauses.len(), 2);
        if let Clause::Match(m) = &q.clauses[0] {
            assert!(m.where_.is_some());
        }
    }

    #[test]
    fn return_distinct() {
        let q = parse_ok("MATCH (n) RETURN DISTINCT n.name");
        if let Clause::Return(r) = &q.clauses[1] {
            assert!(r.distinct);
        }
    }

    #[test]
    fn return_star() {
        let q = parse_ok("MATCH (n) RETURN *");
        if let Clause::Return(r) = &q.clauses[1] {
            assert!(matches!(r.items, ReturnItems::All));
        }
    }

    #[test]
    fn return_with_alias() {
        let q = parse_ok("MATCH (n) RETURN n.name AS name");
        if let Clause::Return(r) = &q.clauses[1] {
            if let ReturnItems::Explicit(items) = &r.items {
                assert_eq!(items[0].alias.as_deref(), Some("name"));
            }
        }
    }

    #[test]
    fn with_clause() {
        let q = parse_ok("MATCH (n:Person) WITH n WHERE n.age > 18 RETURN n");
        assert_eq!(q.clauses.len(), 3);
        assert!(matches!(q.clauses[1], Clause::With(_)));
        if let Clause::With(w) = &q.clauses[1] {
            assert!(w.where_.is_some());
        }
    }

    #[test]
    fn multi_hop_path() {
        let q = parse_ok("MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) RETURN a, c");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            assert_eq!(pat.elements.len(), 5); // node, rel, node, rel, node
        }
    }

    #[test]
    fn return_multiple_items() {
        let q = parse_ok("MATCH (n) RETURN n.name, n.age");
        if let Clause::Return(r) = &q.clauses[1] {
            if let ReturnItems::Explicit(items) = &r.items {
                assert_eq!(items.len(), 2);
            }
        }
    }

    #[test]
    fn expression_and_or() {
        let q = parse_ok("MATCH (n) WHERE n.age > 18 AND n.active = true RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let expr = &m.where_.as_ref().unwrap().expression;
            assert!(matches!(expr, Expression::And(_, _)));
        }
    }

    #[test]
    fn expression_not() {
        let q = parse_ok("MATCH (n) WHERE NOT n.deleted RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let expr = &m.where_.as_ref().unwrap().expression;
            assert!(matches!(expr, Expression::Not(_)));
        }
    }

    #[test]
    fn expression_is_null() {
        let q = parse_ok("MATCH (n) WHERE n.name IS NULL RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let expr = &m.where_.as_ref().unwrap().expression;
            assert!(matches!(expr, Expression::IsNull(_)));
        }
    }

    #[test]
    fn expression_is_not_null() {
        let q = parse_ok("MATCH (n) WHERE n.name IS NOT NULL RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let expr = &m.where_.as_ref().unwrap().expression;
            assert!(matches!(expr, Expression::IsNotNull(_)));
        }
    }

    #[test]
    fn case_insensitive_keywords() {
        let q = parse_ok("match (n:Person) where n.age > 30 return n");
        assert_eq!(q.clauses.len(), 2);
    }

    #[test]
    fn mixed_case_keywords() {
        let q = parse_ok("Match (n) Return n.name As name");
        assert_eq!(q.clauses.len(), 2);
    }

    #[test]
    fn string_literal_double_quoted() {
        let q = parse_ok(r#"MATCH (n {name: "Alice"}) RETURN n"#);
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Node(node) = &pat.elements[0] {
                let props = node.properties.as_ref().unwrap();
                assert_eq!(
                    props[0].1,
                    Expression::Literal(Literal::String("Alice".to_string()))
                );
            }
        }
    }

    #[test]
    fn integer_literal_in_expression() {
        let q = parse_ok("MATCH (n) WHERE n.age = 42 RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            if let Expression::Comparison(_, CompOp::Eq, rhs) =
                &m.where_.as_ref().unwrap().expression
            {
                assert_eq!(**rhs, Expression::Literal(Literal::Integer(42)));
            }
        }
    }

    #[test]
    fn relationship_variable() {
        let q = parse_ok("MATCH (a)-[r:KNOWS]->(b) RETURN r");
        if let Clause::Match(m) = &q.clauses[0] {
            let pat = &m.pattern.0[0];
            if let PatternElement::Relationship(r) = &pat.elements[1] {
                assert_eq!(r.variable.as_deref(), Some("r"));
            }
        }
    }

    #[test]
    fn parse_error_returns_err() {
        assert!(parse("NOT VALID CYPHER %%%").is_err());
    }

    #[test]
    fn empty_input_returns_err() {
        assert!(parse("").is_err());
    }
}
