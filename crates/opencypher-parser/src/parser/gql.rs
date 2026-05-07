/// ISO GQL parser (Phase 5).
///
/// Parses a subset of ISO GQL (ISO/IEC 39075:2024) and lowers the AST to
/// equivalent openCypher clause types so the Cypher translator can be reused.
///
/// GQL-specific lowering rules applied during parsing:
/// - `(n IS Person)` → `(n:Person)` (IS predicate → Cypher label)
/// - `(n IS A & B)` → `(n:A:B)` (ampersand multi-label → multiple labels)
/// - `FILTER expr` → inline WHERE (same as Cypher)
/// - `NEXT` → `WITH *` (scope boundary)
/// - `-[r IS KNOWS]->` → `-[r:KNOWS]->` (IS edge type → Cypher rel type)
use pest::iterators::Pair;
use pest::Parser;
use pest_derive::Parser;

use crate::ast::cypher::{
    AggregateExpr, CallClause, Clause, CompOp, CreateClause, DeleteClause, Direction,
    Expression, Ident, Label, Literal, MapLiteral, MatchClause, MergeClause, NodePattern,
    OrderByClause, Pattern, PatternElement, PatternList, RangeQuantifier, RelationshipPattern,
    RemoveClause, RemoveItem, ReturnClause, ReturnItem, ReturnItems, SetClause, SetItem, SortItem,
    UnwindClause, WhereClause, WithClause,
};
use crate::ast::gql::GqlQuery;
use crate::error::ParseError;

#[derive(Parser)]
#[grammar = "grammars/gql.pest"]
struct GqlPestParser;

/// Parse an ISO GQL query string into a [`GqlQuery`] AST.
///
/// The returned query stores clauses as equivalent Cypher clause variants so
/// they can be forwarded directly to the Cypher translator.
pub fn parse(input: &str) -> Result<GqlQuery, ParseError> {
    let mut pairs = GqlPestParser::parse(Rule::query, input).map_err(|e| {
        let span = match e.location {
            pest::error::InputLocation::Pos(p) => format!("pos:{p}"),
            pest::error::InputLocation::Span((s, end)) => format!("span:{s}..{end}"),
        };
        ParseError::Syntax {
            span,
            message: e.to_string(),
        }
    })?;
    let query_pair = pairs.next().unwrap();
    build_query(query_pair)
}

// ── Top-level ────────────────────────────────────────────────────────────────

fn build_query(pair: Pair<Rule>) -> Result<GqlQuery, ParseError> {
    let statement = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::statement)
        .expect("grammar guarantees a statement");
    let clauses = build_statement(statement)?;
    Ok(GqlQuery { clauses })
}

fn build_statement(pair: Pair<Rule>) -> Result<Vec<Clause>, ParseError> {
    let mut clauses = Vec::new();
    for clause_pair in pair.into_inner() {
        let inner = clause_pair
            .into_inner()
            .next()
            .expect("clause always has an inner rule");
        let clause = match inner.as_rule() {
            Rule::match_clause => Clause::Match(build_match_clause(inner)?),
            Rule::filter_clause => {
                // Standalone FILTER/WHERE → fold into preceding MATCH or produce a WITH WHERE.
                // Strategy: wrap as a WithClause with no items (pass-through + WHERE filter).
                // The translator handles WithClause the same as Cypher's WITH … WHERE.
                let expr = build_filter_clause(inner)?;
                Clause::With(WithClause {
                    distinct: false,
                    items: ReturnItems::All,
                    where_: Some(WhereClause { expression: expr }),
                    order_by: None,
                    skip: None,
                    limit: None,
                })
            }
            Rule::return_clause => Clause::Return(build_return_clause(inner)?),
            Rule::next_clause => {
                // NEXT [WITH return_body] → WITH *  (scope boundary)
                build_next_clause(inner)?
            }
            Rule::unwind_clause => Clause::Unwind(build_unwind_clause(inner)?),
            Rule::create_clause => Clause::Create(build_create_clause(inner)?),
            Rule::merge_clause => Clause::Merge(build_merge_clause(inner)?),
            Rule::set_clause => Clause::Set(build_set_clause(inner)?),
            Rule::delete_clause => Clause::Delete(build_delete_clause(inner)?),
            Rule::remove_clause => Clause::Remove(build_remove_clause(inner)?),
            Rule::call_clause => Clause::Call(build_call_clause(inner)?),
            _ => unreachable!("unexpected GQL clause rule: {:?}", inner.as_rule()),
        };
        clauses.push(clause);
    }
    Ok(clauses)
}

// ── Clause builders ───────────────────────────────────────────────────────────

fn build_match_clause(pair: Pair<Rule>) -> Result<MatchClause, ParseError> {
    let mut optional = false;
    let mut pattern = None;
    let mut where_: Option<WhereClause> = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::optional_marker => optional = true,
            Rule::kw_MATCH => {}
            Rule::pattern_list => pattern = Some(build_pattern_list(inner)?),
            Rule::match_where => {
                // match_where = { kw_WHERE ~ expression }
                let expr_pair = inner
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::expression)
                    .expect("match_where has expression");
                where_ = Some(WhereClause {
                    expression: build_expression(expr_pair)?,
                });
            }
            _ => {}
        }
    }
    Ok(MatchClause {
        optional,
        pattern: pattern.expect("grammar guarantees pattern_list"),
        where_,
    })
}

fn build_filter_clause(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    // filter_clause = { (kw_WHERE | kw_FILTER) ~ expression }
    let expr_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::expression)
        .expect("filter_clause has expression");
    build_expression(expr_pair)
}

fn build_return_clause(pair: Pair<Rule>) -> Result<ReturnClause, ParseError> {
    let mut body_pair = None;
    let mut order_by = None;
    let mut skip = None;
    let mut limit = None;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_RETURN => {}
            Rule::return_body => body_pair = Some(inner),
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
    let (distinct, items) = build_return_body(body_pair.expect("grammar guarantees return_body"))?;
    Ok(ReturnClause {
        distinct,
        items,
        order_by,
        skip,
        limit,
    })
}

fn build_next_clause(pair: Pair<Rule>) -> Result<Clause, ParseError> {
    // next_clause = { kw_NEXT ~ (kw_WITH_kw ~ return_body)? }
    // If NEXT has a WITH body, project those items. Otherwise pass through with RETURN *.
    let mut items = ReturnItems::All;
    let mut distinct = false;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_NEXT | Rule::kw_WITH_kw => {}
            Rule::return_body => {
                let (d, it) = build_return_body(inner)?;
                distinct = d;
                items = it;
            }
            _ => {}
        }
    }
    Ok(Clause::With(WithClause {
        distinct,
        items,
        where_: None,
        order_by: None,
        skip: None,
        limit: None,
    }))
}

// ── Write clause builders (same as Cypher parser) ─────────────────────────────

fn build_unwind_clause(pair: Pair<Rule>) -> Result<UnwindClause, ParseError> {
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
    let pl_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::pattern_list)
        .expect("create_clause has pattern_list");
    Ok(CreateClause {
        pattern: build_pattern_list(pl_pair)?,
    })
}

fn build_merge_clause(pair: Pair<Rule>) -> Result<MergeClause, ParseError> {
    use crate::ast::cypher::MergeClause;
    let pat_pair = pair
        .into_inner()
        .find(|p| p.as_rule() == Rule::pattern)
        .expect("merge_clause has pattern");
    Ok(MergeClause {
        pattern: build_pattern(pat_pair)?,
        actions: Vec::new(),
    })
}

fn build_set_clause(pair: Pair<Rule>) -> Result<SetClause, ParseError> {
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::set_item)
        .map(build_set_item)
        .collect();
    Ok(SetClause { items: items? })
}

fn build_set_item(pair: Pair<Rule>) -> Result<SetItem, ParseError> {
    let mut children: Vec<_> = pair.into_inner().collect();
    match children[0].as_rule() {
        Rule::prop_access_expr => {
            let mut acc_inner = children.remove(0).into_inner();
            let variable = ident_text(&acc_inner.next().expect("prop_access_expr variable"));
            let key = acc_inner
                .find(|p| p.as_rule() == Rule::prop_name)
                .expect("prop_access_expr key")
                .as_str()
                .trim_matches('`')
                .to_string();
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
            let has_map = children.iter().any(|p| p.as_rule() == Rule::map_literal);
            if has_map {
                let map = build_map_literal(
                    children
                        .into_iter()
                        .find(|p| p.as_rule() == Rule::map_literal)
                        .unwrap(),
                )?;
                Ok(SetItem::MergeMap {
                    variable: var_name,
                    map,
                })
            } else {
                let value = build_expression(
                    children
                        .into_iter()
                        .find(|p| p.as_rule() == Rule::expression)
                        .unwrap(),
                )?;
                Ok(SetItem::NodeReplace {
                    variable: var_name,
                    value,
                })
            }
        }
        _ => unreachable!("unexpected set_item start: {:?}", children[0].as_rule()),
    }
}

fn build_delete_clause(pair: Pair<Rule>) -> Result<DeleteClause, ParseError> {
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
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::remove_item)
        .map(build_remove_item)
        .collect();
    Ok(RemoveClause { items: items? })
}

fn build_remove_item(pair: Pair<Rule>) -> Result<RemoveItem, ParseError> {
    let mut children: Vec<_> = pair.into_inner().collect();
    match children[0].as_rule() {
        Rule::prop_access_expr => {
            let mut acc_inner = children.remove(0).into_inner();
            let variable = ident_text(&acc_inner.next().expect("prop_access_expr var"));
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
                    for lbl in child.clone().into_inner() {
                        if lbl.as_rule() == Rule::node_label {
                            let name = lbl
                                .into_inner()
                                .next()
                                .expect("node_label ident")
                                .as_str()
                                .trim_matches('`')
                                .to_string();
                            labels.push(name);
                        }
                    }
                }
            }
            Ok(RemoveItem::Label { variable, labels })
        }
        _ => unreachable!("unexpected remove_item: {:?}", children[0].as_rule()),
    }
}

fn build_call_clause(pair: Pair<Rule>) -> Result<CallClause, ParseError> {
    let mut procedure = String::new();
    let mut args = Vec::new();
    let mut yields = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::kw_CALL => {}
            Rule::proc_name => procedure = inner.as_str().to_string(),
            Rule::expression => args.push(build_expression(inner)?),
            Rule::kw_WITH_kw => {}
            Rule::yield_items => {
                for v in inner.into_inner() {
                    if v.as_rule() == Rule::variable {
                        yields.push(ident_text(&v));
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

// ── ORDER BY ──────────────────────────────────────────────────────────────────

fn build_order_by_clause(pair: Pair<Rule>) -> Result<OrderByClause, ParseError> {
    let items: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::sort_item)
        .map(build_sort_item)
        .collect();
    Ok(OrderByClause { items: items? })
}

fn build_sort_item(pair: Pair<Rule>) -> Result<SortItem, ParseError> {
    let mut expr = None;
    let mut descending = false;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::expression => expr = Some(build_expression(inner)?),
            Rule::sort_direction => {
                let dir = inner.into_inner().next().expect("sort_direction has child");
                descending = dir.as_rule() == Rule::kw_DESC;
            }
            _ => {}
        }
    }
    Ok(SortItem {
        expression: expr.expect("sort_item has expression"),
        descending,
    })
}

// ── Return body ───────────────────────────────────────────────────────────────

fn build_return_body(pair: Pair<Rule>) -> Result<(bool, ReturnItems), ParseError> {
    let mut distinct = false;
    let mut items = ReturnItems::All;
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::distinct_marker => distinct = true,
            Rule::return_items => items = build_return_items(inner)?,
            _ => {}
        }
    }
    Ok((distinct, items))
}

fn build_return_items(pair: Pair<Rule>) -> Result<ReturnItems, ParseError> {
    let inner = pair.into_inner().next().expect("return_items has child");
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
        expression: expr.expect("return_item has expression"),
        alias,
    })
}

// ── Pattern builders ──────────────────────────────────────────────────────────

fn build_pattern_list(pair: Pair<Rule>) -> Result<PatternList, ParseError> {
    let patterns: Result<Vec<_>, _> = pair
        .into_inner()
        .filter(|p| p.as_rule() == Rule::pattern)
        .map(build_pattern)
        .collect();
    Ok(PatternList(patterns?))
}

fn build_pattern(pair: Pair<Rule>) -> Result<Pattern, ParseError> {
    use crate::ast::cypher::Pattern;
    let mut variable = None;
    let mut chain_pair = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::variable if chain_pair.is_none() => variable = Some(ident_text(&inner)),
            Rule::node_pattern_chain => chain_pair = Some(inner),
            _ => {}
        }
    }
    let chain = chain_pair.expect("grammar guarantees node_pattern_chain");
    let elements = build_node_pattern_chain(chain)?;
    Ok(Pattern { variable, elements })
}

fn build_node_pattern_chain(pair: Pair<Rule>) -> Result<Vec<PatternElement>, ParseError> {
    let mut elements = Vec::new();
    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::node_pattern => elements.push(PatternElement::Node(build_node_pattern(inner)?)),
            Rule::chain_link => {
                for link_inner in inner.into_inner() {
                    match link_inner.as_rule() {
                        Rule::rel_pattern => elements
                            .push(PatternElement::Relationship(build_rel_pattern(link_inner)?)),
                        Rule::node_pattern => {
                            elements.push(PatternElement::Node(build_node_pattern(link_inner)?))
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

fn build_node_pattern(pair: Pair<Rule>) -> Result<NodePattern, ParseError> {
    // node_pattern = { "(" ~ variable? ~ node_labels? ~ gql_is_labels? ~ properties? ~ ")" }
    let mut variable = None;
    let mut labels: Vec<Label> = Vec::new();
    let mut properties = None;

    for inner in pair.into_inner() {
        match inner.as_rule() {
            Rule::variable => variable = Some(ident_text(&inner)),
            Rule::node_labels => {
                // :Label notation
                for label_pair in inner.into_inner() {
                    if label_pair.as_rule() == Rule::node_label {
                        let name = label_pair
                            .into_inner()
                            .next()
                            .expect("node_label has ident")
                            .as_str()
                            .trim_matches('`')
                            .to_string();
                        labels.push(name);
                    }
                }
            }
            Rule::gql_is_labels => {
                // IS Label [& Label2 ...] notation — lower to same as :Label
                for child in inner.into_inner() {
                    if child.as_rule() == Rule::gql_label_name {
                        labels.push(child.as_str().trim_matches('`').to_string());
                    }
                }
            }
            Rule::properties => properties = Some(build_map_literal(inner)?),
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
            Rule::rel_dash => {}
            Rule::rel_body => {
                for rb in inner.into_inner() {
                    match rb.as_rule() {
                        Rule::variable => variable = Some(ident_text(&rb)),
                        Rule::rel_type_spec => {
                            // Both colon_type_list and is_type_list produce rel_type_elem children.
                            for rtc in rb.into_inner() {
                                match rtc.as_rule() {
                                    Rule::colon_type_list | Rule::is_type_list => {
                                        for rt in rtc.into_inner() {
                                            if rt.as_rule() == Rule::rel_type_elem {
                                                rel_types.push(
                                                    rt.as_str().trim_matches('`').to_string(),
                                                );
                                            }
                                        }
                                    }
                                    _ => {}
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
    let text = pair.as_str().trim();
    if text == "*" {
        return Ok(RangeQuantifier {
            lower: None,
            upper: None,
        });
    }
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

// ── Map literal builder ───────────────────────────────────────────────────────

fn build_map_literal(pair: Pair<Rule>) -> Result<MapLiteral, ParseError> {
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
            let mut key = None;
            let mut val = None;
            for part in entry.into_inner() {
                match part.as_rule() {
                    Rule::prop_name => key = Some(part.as_str().trim_matches('`').to_string()),
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
    let inner = pair.into_inner().next().expect("expression wraps or_expr");
    build_or_expr(inner)
}

fn build_or_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
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
    let mut children = pair.into_inner();
    let first = children.next().expect("not_expr has child");
    match first.as_rule() {
        Rule::kw_NOT => {
            let nested = children.next().expect("NOT is followed by not_expr");
            Ok(Expression::Not(Box::new(build_not_expr(nested)?)))
        }
        Rule::comparison_expr => build_comparison_expr(first),
        _ => unreachable!("unexpected not_expr child: {:?}", first.as_rule()),
    }
}

fn build_comparison_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut inners = pair.into_inner();
    let lhs_pair = inners.next().expect("comparison_expr has add_sub_expr");
    let lhs = build_add_sub_expr(lhs_pair)?;
    if let Some(suffix) = inners.next() {
        build_comparison_suffix(lhs, suffix)
    } else {
        Ok(lhs)
    }
}

fn build_comparison_suffix(
    lhs: Expression,
    pair: Pair<Rule>,
) -> Result<Expression, ParseError> {
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
            let rhs = build_add_sub_expr(children.next().expect("rhs after comp_op"))?;
            Ok(Expression::Comparison(Box::new(lhs), op, Box::new(rhs)))
        }
        Rule::kw_IS => {
            let next = children.next().expect("IS is followed by something");
            if next.as_rule() == Rule::kw_NOT {
                Ok(Expression::IsNotNull(Box::new(lhs)))
            } else {
                Ok(Expression::IsNull(Box::new(lhs)))
            }
        }
        Rule::kw_IN => {
            let rhs = build_add_sub_expr(children.next().expect("IN has rhs"))?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::In,
                Box::new(rhs),
            ))
        }
        Rule::kw_STARTS => {
            let _kw_with = children.next();
            let rhs = build_add_sub_expr(children.next().expect("STARTS WITH has rhs"))?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::StartsWith,
                Box::new(rhs),
            ))
        }
        Rule::kw_ENDS => {
            let _kw_with = children.next();
            let rhs = build_add_sub_expr(children.next().expect("ENDS WITH has rhs"))?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::EndsWith,
                Box::new(rhs),
            ))
        }
        Rule::kw_CONTAINS => {
            let rhs = build_add_sub_expr(children.next().expect("CONTAINS has rhs"))?;
            Ok(Expression::Comparison(
                Box::new(lhs),
                CompOp::Contains,
                Box::new(rhs),
            ))
        }
        _ => unreachable!("unexpected comparison_suffix start: {:?}", first.as_rule()),
    }
}

fn build_add_sub_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children = pair.into_inner().peekable();
    let first = children.next().expect("add_sub_expr has mul_div_expr");
    let mut acc = build_mul_div_expr(first)?;
    while let Some(op_pair) = children.next() {
        let operand = children.next().expect("operator followed by operand");
        let rhs = build_mul_div_expr(operand)?;
        acc = match op_pair.as_str() {
            "+" => Expression::Add(Box::new(acc), Box::new(rhs)),
            "-" => Expression::Subtract(Box::new(acc), Box::new(rhs)),
            _ => unreachable!(),
        };
    }
    Ok(acc)
}

fn build_mul_div_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children = pair.into_inner().peekable();
    let first = children.next().expect("mul_div_expr has unary_expr");
    let mut acc = build_unary_expr(first)?;
    while let Some(op_pair) = children.next() {
        let operand = children.next().expect("operator followed by operand");
        let rhs = build_unary_expr(operand)?;
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
    let inner = pair.into_inner().next().expect("unary_expr has child");
    match inner.as_rule() {
        Rule::unary_minus => {
            let operand = inner
                .into_inner()
                .next()
                .expect("unary_minus has unary_expr");
            Ok(Expression::Negate(Box::new(build_unary_expr(operand)?)))
        }
        Rule::power_expr => build_power_expr(inner),
        _ => unreachable!(),
    }
}

fn build_power_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children = pair.into_inner();
    let base = build_prop_expr(children.next().expect("power_expr has prop_expr"))?;
    if let Some(exponent) = children.next() {
        Ok(Expression::Power(
            Box::new(base),
            Box::new(build_unary_expr(exponent)?),
        ))
    } else {
        Ok(base)
    }
}

fn build_prop_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let mut children = pair.into_inner();
    let atom_pair = children.next().expect("prop_expr has atom");
    let mut acc = build_atom(atom_pair)?;
    for lookup in children {
        let key = lookup
            .into_inner()
            .find(|p| p.as_rule() == Rule::prop_name)
            .expect("property_lookup has prop_name")
            .as_str()
            .trim_matches('`')
            .to_string();
        acc = Expression::Property(Box::new(acc), key);
    }
    Ok(acc)
}

fn build_atom(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let inner = pair.into_inner().next().expect("atom has child");
    match inner.as_rule() {
        Rule::integer_literal => {
            let n: i64 = inner.as_str().parse().map_err(|_| ParseError::Syntax {
                span: inner.as_str().to_string(),
                message: "integer literal out of range".to_string(),
            })?;
            Ok(Expression::Literal(Literal::Integer(n)))
        }
        Rule::float_literal => {
            let f: f64 = inner.as_str().parse().map_err(|_| ParseError::Syntax {
                span: inner.as_str().to_string(),
                message: "float literal out of range".to_string(),
            })?;
            Ok(Expression::Literal(Literal::Float(f)))
        }
        Rule::string_literal => {
            let raw = inner.as_str();
            let content = &raw[1..raw.len() - 1];
            Ok(Expression::Literal(Literal::String(unescape_string(
                content,
            ))))
        }
        Rule::boolean_literal => {
            let b = inner.as_str().eq_ignore_ascii_case("true");
            Ok(Expression::Literal(Literal::Boolean(b)))
        }
        Rule::null_literal => Ok(Expression::Literal(Literal::Null)),
        Rule::aggregate_expr => build_aggregate_expr(inner),
        Rule::list_literal => {
            let items: Result<Vec<_>, _> = inner
                .into_inner()
                .filter(|p| p.as_rule() == Rule::expression)
                .map(build_expression)
                .collect();
            Ok(Expression::List(items?))
        }
        Rule::map_literal => Ok(Expression::Map(build_map_literal(inner)?)),
        Rule::expression => build_expression(inner),
        Rule::variable => Ok(Expression::Variable(ident_text(&inner))),
        _ => unreachable!("unexpected atom child: {:?}", inner.as_rule()),
    }
}

fn build_aggregate_expr(pair: Pair<Rule>) -> Result<Expression, ParseError> {
    let inner = pair.into_inner().next().expect("aggregate_expr has child");
    match inner.as_rule() {
        Rule::count_expr => {
            let mut distinct = false;
            let mut expr: Option<Box<Expression>> = None;
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::kw_COUNT => {}
                    Rule::count_star => {}
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
            let mut func_name = String::new();
            let mut distinct = false;
            let mut expr: Option<Box<Expression>> = None;
            for child in inner.into_inner() {
                match child.as_rule() {
                    Rule::agg_func_name => func_name = child.as_str().to_ascii_lowercase(),
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

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ident_text(pair: &Pair<Rule>) -> Ident {
    let inner = pair
        .clone()
        .into_inner()
        .next()
        .expect("variable has an ident or ident_escaped child");
    inner.as_str().trim_matches('`').to_string()
}

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

    fn parse_ok(q: &str) -> GqlQuery {
        parse(q).unwrap_or_else(|e| panic!("GQL parse failed for {q:?}: {e}"))
    }

    // ── Basic MATCH RETURN ────────────────────────────────────────────────────

    #[test]
    fn simple_match_return() {
        let q = parse_ok("MATCH (n) RETURN n");
        assert_eq!(q.clauses.len(), 2);
        assert!(matches!(q.clauses[0], Clause::Match(_)));
        assert!(matches!(q.clauses[1], Clause::Return(_)));
    }

    #[test]
    fn match_colon_label() {
        let q = parse_ok("MATCH (n:Person) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Node(node) = &m.pattern.0[0].elements[0] {
                assert_eq!(node.labels, vec!["Person"]);
            } else {
                panic!("expected node");
            }
        }
    }

    #[test]
    fn match_is_label() {
        // GQL: IS Label notation
        let q = parse_ok("MATCH (n IS Person) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Node(node) = &m.pattern.0[0].elements[0] {
                assert_eq!(
                    node.labels,
                    vec!["Person"],
                    "IS label should lower to colon label"
                );
            } else {
                panic!("expected node");
            }
        }
    }

    #[test]
    fn match_is_multiple_labels_ampersand() {
        // IS Person & Employee
        let q = parse_ok("MATCH (n IS Person & Employee) RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Node(node) = &m.pattern.0[0].elements[0] {
                assert_eq!(node.labels, vec!["Person", "Employee"]);
            } else {
                panic!("expected node");
            }
        }
    }

    #[test]
    fn filter_clause_lowered_to_with_where() {
        // Standalone FILTER → WithClause with WHERE
        let q = parse_ok("MATCH (n:Person) FILTER n.age > 30 RETURN n");
        assert!(matches!(q.clauses[1], Clause::With(_)));
        if let Clause::With(w) = &q.clauses[1] {
            assert!(w.where_.is_some());
        }
    }

    #[test]
    fn where_in_match_clause() {
        let q = parse_ok("MATCH (n:Person) WHERE n.age > 30 RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            assert!(m.where_.is_some());
        }
    }

    #[test]
    fn next_clause_lowered_to_with() {
        let q = parse_ok("MATCH (n:Person) RETURN n NEXT MATCH (m:Movie) RETURN m");
        // NEXT becomes a WithClause
        assert!(q.clauses.iter().any(|c| matches!(c, Clause::With(_))));
    }

    #[test]
    fn rel_pattern_colon_type() {
        let q = parse_ok("MATCH (a:Person)-[r:KNOWS]->(b:Person) RETURN a, b");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Relationship(rel) = &m.pattern.0[0].elements[1] {
                assert_eq!(rel.rel_types, vec!["KNOWS"]);
                assert_eq!(rel.direction, Direction::Right);
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn rel_pattern_is_type() {
        let q = parse_ok("MATCH (a)-[r IS KNOWS]->(b) RETURN a");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Relationship(rel) = &m.pattern.0[0].elements[1] {
                assert_eq!(
                    rel.rel_types,
                    vec!["KNOWS"],
                    "IS type should lower to colon type"
                );
            } else {
                panic!("expected relationship");
            }
        }
    }

    #[test]
    fn return_distinct() {
        let q = parse_ok("MATCH (n:Person) RETURN DISTINCT n");
        if let Clause::Return(r) = q.clauses.last().unwrap() {
            assert!(r.distinct);
        }
    }

    #[test]
    fn return_with_alias() {
        let q = parse_ok("MATCH (n:Person) RETURN n.name AS name");
        if let Clause::Return(r) = q.clauses.last().unwrap() {
            if let ReturnItems::Explicit(items) = &r.items {
                assert_eq!(items[0].alias.as_deref(), Some("name"));
            }
        }
    }

    #[test]
    fn optional_match() {
        let q = parse_ok("MATCH (n:Person) OPTIONAL MATCH (n)-[:KNOWS]->(m) RETURN n, m");
        assert!(matches!(q.clauses[0], Clause::Match(ref m) if !m.optional));
        assert!(matches!(q.clauses[1], Clause::Match(ref m) if m.optional));
    }

    #[test]
    fn return_order_by_limit() {
        let q = parse_ok("MATCH (n:Person) RETURN n ORDER BY n.name LIMIT 10");
        if let Clause::Return(r) = q.clauses.last().unwrap() {
            assert!(r.order_by.is_some());
            assert!(r.limit.is_some());
        }
    }

    #[test]
    fn count_star_aggregate() {
        let q = parse_ok("MATCH (n:Person) RETURN count(*) AS total");
        if let Clause::Return(r) = q.clauses.last().unwrap() {
            if let ReturnItems::Explicit(items) = &r.items {
                assert!(matches!(
                    &items[0].expression,
                    Expression::Aggregate(AggregateExpr::Count { expr: None, .. })
                ));
            }
        }
    }

    #[test]
    fn property_access_in_where() {
        let q = parse_ok("MATCH (n:Person) WHERE n.age > 30 RETURN n");
        if let Clause::Match(m) = &q.clauses[0] {
            let expr = &m.where_.as_ref().unwrap().expression;
            assert!(matches!(expr, Expression::Comparison(_, CompOp::Gt, _)));
        }
    }

    #[test]
    fn multi_hop_relationship() {
        let q = parse_ok("MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN a, c");
        if let Clause::Match(m) = &q.clauses[0] {
            assert_eq!(m.pattern.0[0].elements.len(), 5);
        }
    }

    #[test]
    fn variable_length_path_star() {
        let q = parse_ok("MATCH (a)-[:KNOWS*]->(b) RETURN a, b");
        if let Clause::Match(m) = &q.clauses[0] {
            if let PatternElement::Relationship(rel) = &m.pattern.0[0].elements[1] {
                let rq = rel.range.as_ref().expect("should have range");
                assert!(rq.lower.is_none() && rq.upper.is_none());
            }
        }
    }

    #[test]
    fn parse_error_returns_err() {
        let result = parse("MATCH RETURN");
        assert!(result.is_err());
    }
}
