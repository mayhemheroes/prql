//! This module is responsible for translating PRQL AST to sqlparser AST, and
//! then to a String. We use sqlparser because it's trivial to create the string
//! once it's in their AST (it's just `.to_string()`). It also lets us support a
//! few dialects of SQL immediately.
// The average code quality here is low — we're basically plugging in test
// cases and fixing what breaks, with some occasional refactors. I'm not sure
// that's a terrible approach — the SQL spec is huge, so we're not reasonably
// going to be isomorphically mapping everything back from SQL to PRQL. But it
// does mean we should continue to iterate on this file and refactor things when
// necessary.
use anyhow::{anyhow, bail, Result};
use itertools::Itertools;
use sqlformat::{format, FormatOptions, QueryParams};
use sqlparser::ast::{
    self as sql_ast, BinaryOperator, DateTimeField, Function, FunctionArg, FunctionArgExpr, Join,
    JoinConstraint, JoinOperator, ObjectName, OrderByExpr, Select, SelectItem, SetExpr, TableAlias,
    TableFactor, TableWithJoins, Top, UnaryOperator, Value, WindowFrameBound, WindowSpec,
};
use std::collections::HashMap;

use crate::ast::JoinFilter;
use crate::ast::*;
use crate::error::{Error, Reason};
use crate::semantic::Context;
use crate::utils::OrMap;
use crate::utils::*;

use super::materializer::MaterializationContext;
use super::{distinct, un_group, MaterializedFrame};

/// Translate a PRQL AST into a SQL string.
pub fn translate(query: Query, context: Context) -> Result<String> {
    let sql_query = translate_query(query, context)?;

    let sql_query_string = sql_query.to_string();

    let formatted = format(
        &sql_query_string,
        &QueryParams::default(),
        FormatOptions::default(),
    );

    // The sql formatter turns `{{` into `{ {`, and while that's reasonable SQL,
    // we want to allow jinja expressions through. So we (somewhat hackily) replace
    // any `{ {` with `{{`.
    let formatted = formatted.replace("{ {", "{{").replace("} }", "}}");

    Ok(formatted)
}

pub fn translate_query(query: Query, context: Context) -> Result<sql_ast::Query> {
    // extract tables and the pipeline
    let tables = into_tables(query.main_pipeline, query.tables)?;

    let mut context = MaterializationContext::from(context);

    // split to atomics
    let atomics = atomic_tables_of_tables(tables, &mut context)?;

    // materialize each atomic in two stages
    let mut materialized = Vec::new();
    for t in atomics {
        let table_id = t.name.clone().and_then(|x| x.declared_at);

        let (pipeline, frame, c) = super::materialize(t.pipeline, context, table_id)?;
        context = c;

        materialized.push(AtomicTable {
            name: t.name,
            frame: Some(frame),
            pipeline,
        });
    }

    let dialect = query.def.dialect.handler();

    // take last table
    if materialized.is_empty() {
        bail!("No tables?");
    }
    let main_query = materialized.remove(materialized.len() - 1);
    let ctes = materialized;

    // convert each of the CTEs
    let ctes: Vec<_> = ctes
        .into_iter()
        .map(|t| table_to_sql_cte(t, dialect.as_ref()))
        .try_collect()?;

    // convert main query
    // dbg!(&main_query);
    let mut main_query = sql_query_of_atomic_table(main_query, dialect.as_ref())?;

    // attach CTEs
    if !ctes.is_empty() {
        main_query.with = Some(sql_ast::With {
            cte_tables: ctes,
            recursive: false,
        });
    }

    Ok(main_query)
}

#[derive(Debug)]
pub struct AtomicTable {
    name: Option<TableRef>,
    pipeline: Vec<Transform>,
    frame: Option<MaterializedFrame>,
}

fn into_tables(main_pipeline: Vec<Transform>, tables: Vec<Table>) -> Result<Vec<Table>> {
    let main = Table {
        id: None,
        name: String::default(),
        pipeline: main_pipeline,
    };
    Ok([tables, vec![main]].concat())
}

fn table_to_sql_cte(table: AtomicTable, dialect: &dyn DialectHandler) -> Result<sql_ast::Cte> {
    let alias = sql_ast::TableAlias {
        name: translate_ident_part(table.name.as_ref().unwrap().clone().name, dialect),
        columns: vec![],
    };
    Ok(sql_ast::Cte {
        alias,
        query: sql_query_of_atomic_table(table, dialect)?,
        from: None,
    })
}

fn table_factor_of_table_ref(table_ref: &TableRef, dialect: &dyn DialectHandler) -> TableFactor {
    TableFactor::Table {
        name: sql_ast::ObjectName(translate_ident(table_ref.name.clone(), dialect)),
        alias: table_ref.alias.clone().map(|a| TableAlias {
            name: sql_ast::Ident::new(a),
            columns: vec![],
        }),
        args: None,
        with_hints: vec![],
    }
}

// impl Translator for
// fn sql_query_of_atomic_table(table: AtomicTable, dialect: &Dialect) -> Result<sql_ast::Query> {
fn sql_query_of_atomic_table(
    table: AtomicTable,
    dialect: &dyn DialectHandler,
) -> Result<sql_ast::Query> {
    let frame = table.frame.ok_or_else(|| anyhow!("frame not provided?"))?;

    let mut from = table
        .pipeline
        .iter()
        .filter_map(|t| match &t.kind {
            TransformKind::From(table_ref) => Some(TableWithJoins {
                relation: table_factor_of_table_ref(table_ref, dialect),
                joins: vec![],
            }),
            _ => None,
        })
        .collect::<Vec<_>>();

    let joins = table
        .pipeline
        .iter()
        .filter(|t| matches!(t.kind, TransformKind::Join { .. }))
        .map(|j| translate_join(&j.kind, dialect))
        .collect::<Result<Vec<_>>>()?;
    if !joins.is_empty() {
        if let Some(from) = from.last_mut() {
            from.joins = joins;
        } else {
            return Err(anyhow!("Cannot use `join` without `from`"));
        }
    }

    // Split the pipeline into before & after the aggregate
    let aggregate_position = table
        .pipeline
        .iter()
        .position(|t| matches!(t.kind, TransformKind::Aggregate { .. }))
        .unwrap_or(table.pipeline.len());
    let (before, after) = table.pipeline.split_at(aggregate_position);

    // Find the filters that come before the aggregation.
    let where_ = filter_of_pipeline(before, dialect)?;
    let having = filter_of_pipeline(after, dialect)?;

    let takes = table
        .pipeline
        .iter()
        .filter_map(|t| match &t.kind {
            TransformKind::Take { range, .. } => Some(range.clone()),
            _ => None,
        })
        .collect();
    let take = range_of_ranges(takes)?;
    let offset = take.start.map(|s| s - 1).unwrap_or(0);
    let limit = take.end.map(|e| e - offset);

    let offset = if offset == 0 {
        None
    } else {
        Some(sqlparser::ast::Offset {
            value: translate_expr_kind(ExprKind::Literal(Literal::Integer(offset)), dialect)?,
            rows: sqlparser::ast::OffsetRows::None,
        })
    };

    // Use sorting from the frame
    let order_by = (frame.sort)
        .into_iter()
        .map(|s| translate_column_sort(s, dialect))
        .try_collect()?;

    let aggregate = table.pipeline.get(aggregate_position);

    let group_bys: Vec<Expr> = match aggregate {
        Some(Transform {
            kind: TransformKind::Aggregate { by, .. },
            ..
        }) => by.clone(),
        None => vec![],
        _ => unreachable!("Expected an aggregate transformation"),
    };

    let distinct = table
        .pipeline
        .iter()
        .any(|t| matches!(t.kind, TransformKind::Unique));

    Ok(sql_ast::Query {
        body: Box::new(SetExpr::Select(Box::new(Select {
            distinct,
            top: if dialect.use_top() {
                limit.map(|l| top_of_i64(l, dialect))
            } else {
                None
            },
            projection: (frame.columns.into_iter())
                .map(|expr| translate_select_item(expr, dialect))
                .try_collect()?,
            into: None,
            from,
            lateral_views: vec![],
            selection: where_,
            group_by: try_into_exprs(group_bys, dialect)?,
            cluster_by: vec![],
            distribute_by: vec![],
            sort_by: vec![],
            having,
            qualify: None,
        }))),
        order_by,
        with: None,
        limit: if dialect.use_top() {
            None
        } else {
            limit.map(expr_of_i64)
        },
        offset,
        fetch: None,
        lock: None,
    })
}

/// Convert a pipeline into a number of pipelines which can each "fit" into a SELECT.
fn atomic_pipelines_of_pipeline(
    pipeline: Vec<Transform>,
    context: &mut MaterializationContext,
) -> Result<Vec<AtomicTable>> {
    // Insert a cut, when we find transformation that out of order:
    // - joins (no limit),
    // - filters (for WHERE)
    // - aggregate (max 1x)
    // - sort (no limit)
    // - filters (for HAVING)
    // - take (no limit)
    //
    // Select and derive should already be extracted during resolving phase.
    //
    // So we loop through the Pipeline, and cut it into cte-sized pipelines,
    // which we'll then compose together.
    let pipeline = Ok(pipeline)
        .and_then(un_group::un_group)
        .and_then(|x| distinct::take_to_distinct(x, context))?;

    let mut counts: HashMap<&str, u32> = HashMap::new();
    let mut splits = vec![0];
    for (i, transform) in pipeline.iter().enumerate() {
        let split = match transform.kind.as_ref() {
            "Join" => {
                counts.get("Filter").is_some()
                    || counts.get("Aggregate").is_some()
                    || counts.get("Sort").is_some()
                    || counts.get("Take").is_some()
            }
            "Aggregate" => {
                counts.get("Aggregate").is_some()
                    || counts.get("Sort").is_some()
                    || counts.get("Take").is_some()
            }
            "Sort" => counts.get("Take").is_some(),
            "Filter" => counts.get("Take").is_some() || transform.is_complex,

            // There can be many takes, but they have to be consecutive
            // For example `take 100 | sort a | take 10` can't be one CTE.
            // But this is enforced by transform order anyway.
            "Take" => false,

            _ => false,
        };

        if split {
            splits.push(i);
            counts.clear();
        }

        *counts.entry(transform.kind.as_ref()).or_insert(0) += 1;
    }

    splits.push(pipeline.len());
    let ctes = (0..splits.len() - 1)
        .map(|i| pipeline[splits[i]..splits[i + 1]].to_vec())
        .filter(|x| !x.is_empty())
        .map(|transforms| transforms.into())
        .collect();
    Ok(ctes)
}

/// Converts a series of tables into a series of atomic tables, by putting the
/// next pipeline's `from` as the current pipelines's table name.
fn atomic_tables_of_tables(
    tables: Vec<Table>,
    context: &mut MaterializationContext,
) -> Result<Vec<AtomicTable>> {
    let mut atomics = Vec::new();
    let mut index = 0;
    for table in tables {
        // split table into atomics
        let mut t_atomics: Vec<_> = atomic_pipelines_of_pipeline(table.pipeline, context)?;

        let (last, ctes) = t_atomics
            .split_last_mut()
            .ok_or_else(|| anyhow!("No pipelines?"))?;

        // generate table names for all but last table
        let mut prev_table_ref = None;
        for cte in ctes {
            try_prepend_with_from(&mut cte.pipeline, prev_table_ref);

            let name = format!("table_{index}");
            let id = context.declare_table(&name);

            cte.name = Some(TableRef {
                name,
                alias: None,
                declared_at: Some(id),
                ty: None,
            });
            index += 1;

            let last_frame = cte.pipeline.last().map(|t| t.ty.clone());
            prev_table_ref = cte.name.clone().zip(last_frame);
        }

        // use original table name
        try_prepend_with_from(&mut last.pipeline, prev_table_ref);
        last.name = Some(TableRef {
            name: table.name,
            alias: None,
            declared_at: table.id,
            ty: None,
        });

        atomics.extend(t_atomics);
    }
    Ok(atomics)
}

fn try_prepend_with_from(pipeline: &mut Vec<Transform>, table: Option<(TableRef, Frame)>) {
    if let Some((mut table_ref, frame)) = table {
        table_ref.ty = Some(Ty::Table(frame.clone()));
        let transform = Transform {
            ty: frame,
            kind: TransformKind::From(table_ref),
            is_complex: false,
            span: None,
        };
        pipeline.insert(0, transform);
    }
}

/// Aggregate several ordered ranges into one, computing the intersection.
///
/// Returns a tuple of `(start, end)`, where `end` is optional.
fn range_of_ranges(ranges: Vec<Range>) -> Result<Range<i64>> {
    let mut current = Range::default();
    for range in ranges {
        let mut range = range.into_int()?;

        // b = b + a.start -1 (take care of 1-based index!)
        range.start = range.start.or_map(current.start, |a, b| a + b - 1);
        range.end = range.end.map(|b| current.start.unwrap_or(1) + b - 1);

        // b.end = min(a.end, b.end)
        range.end = current.end.or_map(range.end, i64::min);
        current = range;
    }

    if current
        .start
        .zip(current.end)
        .map(|(s, e)| e < s)
        .unwrap_or(false)
    {
        bail!("Range end is before its start.");
    }
    Ok(current)
}

fn filter_of_pipeline(
    pipeline: &[Transform],
    dialect: &dyn DialectHandler,
) -> Result<Option<sql_ast::Expr>> {
    let filters: Vec<Expr> = pipeline
        .iter()
        .filter_map(|t| match &t.kind {
            TransformKind::Filter(filter) => Some(*filter.clone()),
            _ => None,
        })
        .collect();
    filter_of_filters(filters, dialect)
}

fn filter_of_filters(
    conditions: Vec<Expr>,
    dialect: &dyn DialectHandler,
) -> Result<Option<sql_ast::Expr>> {
    let mut condition = None;
    for filter in conditions {
        if let Some(left) = condition {
            condition = Some(Expr::from(ExprKind::Binary {
                op: BinOp::And,
                left: Box::new(left),
                right: Box::new(filter),
            }))
        } else {
            condition = Some(filter)
        }
    }

    condition
        .map(|n| translate_expr_kind(n.kind, dialect))
        .transpose()
}

fn expr_of_i64(number: i64) -> sql_ast::Expr {
    sql_ast::Expr::Value(Value::Number(
        number.to_string(),
        number.leading_zeros() < 32,
    ))
}

fn top_of_i64(take: i64, dialect: &dyn DialectHandler) -> Top {
    Top {
        quantity: Some(
            translate_expr_kind(ExprKind::Literal(Literal::Integer(take)), dialect).unwrap(),
        ),
        with_ties: false,
        percent: false,
    }
}
fn try_into_exprs(nodes: Vec<Expr>, dialect: &dyn DialectHandler) -> Result<Vec<sql_ast::Expr>> {
    nodes
        .into_iter()
        .map(|x| x.kind)
        .map(|item| translate_expr_kind(item, dialect))
        .try_collect()
}

fn translate_select_item(expr: Expr, dialect: &dyn DialectHandler) -> Result<SelectItem> {
    let inner = translate_expr_kind(expr.kind, dialect)?;

    Ok(match expr.alias {
        Some(alias) => SelectItem::ExprWithAlias {
            alias: translate_ident_part(alias, dialect),
            expr: inner,
        },
        None => SelectItem::UnnamedExpr(inner),
    })
}

fn translate_expr_kind(item: ExprKind, dialect: &dyn DialectHandler) -> Result<sql_ast::Expr> {
    Ok(match item {
        ExprKind::Ident(ident) => {
            sql_ast::Expr::CompoundIdentifier(translate_column(ident, dialect))
        }

        ExprKind::Binary { op, left, right } => {
            if let Some(is_null) = try_into_is_null(&op, &left, &right, dialect)? {
                is_null
            } else {
                let op = match op {
                    BinOp::Mul => BinaryOperator::Multiply,
                    BinOp::Div => BinaryOperator::Divide,
                    BinOp::Mod => BinaryOperator::Modulo,
                    BinOp::Add => BinaryOperator::Plus,
                    BinOp::Sub => BinaryOperator::Minus,
                    BinOp::Eq => BinaryOperator::Eq,
                    BinOp::Ne => BinaryOperator::NotEq,
                    BinOp::Gt => BinaryOperator::Gt,
                    BinOp::Lt => BinaryOperator::Lt,
                    BinOp::Gte => BinaryOperator::GtEq,
                    BinOp::Lte => BinaryOperator::LtEq,
                    BinOp::And => BinaryOperator::And,
                    BinOp::Or => BinaryOperator::Or,
                    BinOp::Coalesce => unreachable!(),
                };
                sql_ast::Expr::BinaryOp {
                    left: translate_operand(left.kind, op.binding_strength(), dialect)?,
                    right: translate_operand(right.kind, op.binding_strength(), dialect)?,
                    op,
                }
            }
        }

        ExprKind::Unary { op, expr } => {
            let op = match op {
                UnOp::Neg => UnaryOperator::Minus,
                UnOp::Not => UnaryOperator::Not,
            };
            let expr = translate_operand(expr.kind, op.binding_strength(), dialect)?;
            sql_ast::Expr::UnaryOp { op, expr }
        }

        ExprKind::Range(r) => {
            fn assert_bound(bound: Option<Box<Expr>>) -> Result<Expr, Error> {
                bound.map(|b| *b).ok_or_else(|| {
                    Error::new(Reason::Simple(
                        "range requires both bounds to be used this way".to_string(),
                    ))
                })
            }
            let start: sql_ast::Expr = translate_expr_kind(assert_bound(r.start)?.kind, dialect)?;
            let end: sql_ast::Expr = translate_expr_kind(assert_bound(r.end)?.kind, dialect)?;
            sql_ast::Expr::Identifier(sql_ast::Ident::new(format!("{} AND {}", start, end)))
        }
        // Fairly hacky — convert everything to a string, then concat it,
        // then convert to sql_ast::Expr. We can't use the `Item::sql_ast::Expr` code above
        // since we don't want to intersperse with spaces.
        ExprKind::SString(s_string_items) => {
            let string = s_string_items
                .into_iter()
                .map(|s_string_item| match s_string_item {
                    InterpolateItem::String(string) => Ok(string),
                    InterpolateItem::Expr(node) => {
                        translate_expr_kind(node.kind, dialect).map(|expr| expr.to_string())
                    }
                })
                .collect::<Result<Vec<String>>>()?
                .join("");
            sql_ast::Expr::Identifier(sql_ast::Ident::new(string))
        }
        ExprKind::FString(f_string_items) => {
            let args = f_string_items
                .into_iter()
                .map(|item| match item {
                    InterpolateItem::String(string) => {
                        Ok(sql_ast::Expr::Value(Value::SingleQuotedString(string)))
                    }
                    InterpolateItem::Expr(node) => translate_expr_kind(node.kind, dialect),
                })
                .map(|r| r.map(|e| FunctionArg::Unnamed(FunctionArgExpr::Expr(e))))
                .collect::<Result<Vec<_>>>()?;

            sql_ast::Expr::Function(Function {
                name: ObjectName(vec![sql_ast::Ident::new("CONCAT")]),
                args,
                distinct: false,
                over: None,
                special: false,
            })
        }
        ExprKind::Interval(interval) => {
            let sql_parser_datetime = match interval.unit.as_str() {
                "years" => DateTimeField::Year,
                "months" => DateTimeField::Month,
                "days" => DateTimeField::Day,
                "hours" => DateTimeField::Hour,
                "minutes" => DateTimeField::Minute,
                "seconds" => DateTimeField::Second,
                _ => bail!("Unsupported interval unit: {}", interval.unit),
            };
            sql_ast::Expr::Value(Value::Interval {
                value: Box::new(translate_expr_kind(
                    ExprKind::Literal(Literal::Integer(interval.n)),
                    dialect,
                )?),
                leading_field: Some(sql_parser_datetime),
                leading_precision: None,
                last_field: None,
                fractional_seconds_precision: None,
            })
        }
        ExprKind::Windowed(window) => {
            let expr = translate_expr_kind(window.expr.kind, dialect)?;

            let default_frame = if window.sort.is_empty() {
                (WindowKind::Rows, Range::unbounded())
            } else {
                (WindowKind::Range, Range::from_ints(None, Some(0)))
            };

            let window = WindowSpec {
                partition_by: try_into_exprs(window.group, dialect)?,
                order_by: (window.sort)
                    .into_iter()
                    .map(|s| translate_column_sort(s, dialect))
                    .try_collect()?,
                window_frame: if window.window == default_frame {
                    None
                } else {
                    Some(try_into_window_frame(window.window)?)
                },
            };

            sql_ast::Expr::Identifier(sql_ast::Ident::new(format!("{expr} OVER ({window})")))
        }
        ExprKind::Literal(l) => match l {
            Literal::Null => sql_ast::Expr::Value(Value::Null),
            Literal::String(s) => sql_ast::Expr::Value(Value::SingleQuotedString(s)),
            Literal::Boolean(b) => sql_ast::Expr::Value(Value::Boolean(b)),
            Literal::Float(f) => sql_ast::Expr::Value(Value::Number(format!("{f}"), false)),
            Literal::Integer(i) => sql_ast::Expr::Value(Value::Number(format!("{i}"), false)),
            Literal::Date(value) => sql_ast::Expr::TypedString {
                data_type: sql_ast::DataType::Date,
                value,
            },
            Literal::Time(value) => sql_ast::Expr::TypedString {
                data_type: sql_ast::DataType::Time,
                value,
            },
            Literal::Timestamp(value) => sql_ast::Expr::TypedString {
                data_type: sql_ast::DataType::Timestamp,
                value,
            },
        },
        _ => bail!("Can't convert to sql_ast::Expr; {item:?}"),
    })
}
fn try_into_is_null(
    op: &BinOp,
    a: &Expr,
    b: &Expr,
    dialect: &dyn DialectHandler,
) -> Result<Option<sql_ast::Expr>> {
    if matches!(op, BinOp::Eq) || matches!(op, BinOp::Ne) {
        let expr = if matches!(a.kind, ExprKind::Literal(Literal::Null)) {
            b.kind.clone()
        } else if matches!(b.kind, ExprKind::Literal(Literal::Null)) {
            a.kind.clone()
        } else {
            return Ok(None);
        };

        let min_strength =
            sql_ast::Expr::IsNull(Box::new(sql_ast::Expr::Value(Value::Null))).binding_strength();
        let expr = translate_operand(expr, min_strength, dialect)?;

        return Ok(Some(if matches!(op, BinOp::Eq) {
            sql_ast::Expr::IsNull(expr)
        } else {
            sql_ast::Expr::IsNotNull(expr)
        }));
    }

    Ok(None)
}
fn try_into_window_frame((kind, range): (WindowKind, Range)) -> Result<sql_ast::WindowFrame> {
    fn parse_bound(bound: Expr) -> Result<WindowFrameBound> {
        let as_int = bound.kind.into_literal()?.into_integer()?;
        Ok(match as_int {
            0 => WindowFrameBound::CurrentRow,
            1.. => WindowFrameBound::Following(Some(as_int as u64)),
            _ => WindowFrameBound::Preceding(Some((-as_int) as u64)),
        })
    }

    Ok(sql_ast::WindowFrame {
        units: match kind {
            WindowKind::Rows => sql_ast::WindowFrameUnits::Rows,
            WindowKind::Range => sql_ast::WindowFrameUnits::Range,
        },
        start_bound: if let Some(start) = range.start {
            parse_bound(*start)?
        } else {
            WindowFrameBound::Preceding(None)
        },
        end_bound: Some(if let Some(end) = range.end {
            parse_bound(*end)?
        } else {
            WindowFrameBound::Following(None)
        }),
    })
}

// I had an idea to for stdlib functions to have "native" keyword, which would prevent them from being
// resolved and materialized and would be passed to here. But that has little advantage over current approach.
// After some time when we know that current approach is good, this impl can be removed.
#[allow(dead_code)]
fn translate_func_call(func_call: FuncCall, dialect: &dyn DialectHandler) -> Result<Function> {
    let FuncCall { name, args, .. } = func_call;

    Ok(Function {
        name: ObjectName(vec![sql_ast::Ident::new(name.kind.into_ident().unwrap())]),
        args: args
            .into_iter()
            .map(|a| translate_expr_kind(a.kind, dialect))
            .map(|e| e.map(|a| FunctionArg::Unnamed(FunctionArgExpr::Expr(a))))
            .collect::<Result<Vec<_>>>()?,
        over: None,
        distinct: false,
        special: false,
    })
}
fn translate_column_sort(sort: ColumnSort, dialect: &dyn DialectHandler) -> Result<OrderByExpr> {
    Ok(OrderByExpr {
        expr: translate_expr_kind(sort.column.kind, dialect)?,
        asc: if matches!(sort.direction, SortDirection::Asc) {
            None // default order is ASC, so there is no need to emit it
        } else {
            Some(false)
        },
        nulls_first: None,
    })
}

fn translate_join(t: &TransformKind, dialect: &dyn DialectHandler) -> Result<Join> {
    match t {
        TransformKind::Join { side, with, filter } => {
            let constraint = match filter {
                JoinFilter::On(nodes) => JoinConstraint::On(
                    filter_of_filters(nodes.clone(), dialect)?
                        .unwrap_or(sql_ast::Expr::Value(Value::Boolean(true))),
                ),
                JoinFilter::Using(nodes) => JoinConstraint::Using(
                    nodes
                        .iter()
                        .map(|x| translate_ident(x.kind.clone().into_ident()?, dialect).into_only())
                        .collect::<Result<Vec<_>>>()
                        .map_err(|_| {
                            Error::new(Reason::Expected {
                                who: Some("join".to_string()),
                                expected: "An identifier with only one part; no `.`".to_string(),
                                // TODO: Add in the actual item (but I couldn't
                                // get the error types to agree)
                                found: "A multipart identifier".to_string(),
                            })
                        })?,
                ),
            };

            Ok(Join {
                relation: table_factor_of_table_ref(with, dialect),
                join_operator: match *side {
                    JoinSide::Inner => JoinOperator::Inner(constraint),
                    JoinSide::Left => JoinOperator::LeftOuter(constraint),
                    JoinSide::Right => JoinOperator::RightOuter(constraint),
                    JoinSide::Full => JoinOperator::FullOuter(constraint),
                },
            })
        }
        _ => unreachable!(),
    }
}

/// Translate a column name. We need to special-case this for BigQuery
// Ref #852
fn translate_column(ident: String, dialect: &dyn DialectHandler) -> Vec<sql_ast::Ident> {
    match dialect.dialect() {
        Dialect::BigQuery => {
            if let Some((prefix, column)) = ident.rsplit_once('.') {
                // If there's a table definition, pass it to `translate_ident` to
                // be surrounded by quotes; without surrounding the table name
                // with quotes.
                translate_ident(prefix.to_string(), dialect)
                    .into_iter()
                    .chain(translate_ident(column.to_string(), dialect))
                    .collect()
            } else {
                translate_ident(ident, dialect)
            }
        }
        _ => translate_ident(ident, dialect),
    }
}

/// Translate a PRQL Ident to a Vec of SQL Idents.
// We return a vec of SQL Idents because sqlparser sometimes uses
// [ObjectName](sql_ast::ObjectName) and sometimes uses
// [sql_ast::Expr::CompoundIdentifier](sql_ast::Expr::CompoundIdentifier), each of which
// contains `Vec<Ident>`.
fn translate_ident(ident: String, dialect: &dyn DialectHandler) -> Vec<sql_ast::Ident> {
    let is_jinja = ident.starts_with("{{") && ident.ends_with("}}");

    if is_jinja {
        return vec![sql_ast::Ident::new(ident)];
    }

    match dialect.dialect() {
        // BigQuery has some unusual rules around quoting idents #852 (it includes the
        // project, which may be a cause). I'm not 100% it's watertight, but we'll see.
        Dialect::BigQuery => {
            if ident.split('.').count() > 2 {
                vec![sql_ast::Ident::with_quote(dialect.ident_quote(), ident)]
            } else {
                vec![sql_ast::Ident::new(ident)]
            }
        }
        _ => ident
            .split('.')
            .map(|x| translate_ident_part(x.to_string(), dialect))
            .collect(),
    }
}

fn translate_ident_part(ident: String, dialect: &dyn DialectHandler) -> sql_ast::Ident {
    // TODO: can probably represent these with a single regex
    fn starting_forbidden(c: char) -> bool {
        !(('a'..='z').contains(&c) || matches!(c, '_' | '$'))
    }
    fn subsequent_forbidden(c: char) -> bool {
        !(('a'..='z').contains(&c) || ('0'..='9').contains(&c) || matches!(c, '_' | '$'))
    }

    let is_asterisk = ident == "*";

    if !is_asterisk
        && (ident.is_empty()
            || ident.starts_with(starting_forbidden)
            || (ident.chars().count() > 1 && ident.contains(subsequent_forbidden)))
    {
        sql_ast::Ident::with_quote(dialect.ident_quote(), ident)
    } else {
        sql_ast::Ident::new(ident)
    }
}

/// Wraps into parenthesis if binding strength would be less than min_strength
fn translate_operand(
    expr: ExprKind,
    min_strength: i32,
    dialect: &dyn DialectHandler,
) -> Result<Box<sql_ast::Expr>> {
    let expr = Box::new(translate_expr_kind(expr, dialect)?);

    Ok(if expr.binding_strength() < min_strength {
        Box::new(sql_ast::Expr::Nested(expr))
    } else {
        expr
    })
}

trait SQLExpression {
    /// Returns binding strength of an SQL expression
    /// https://www.postgresql.org/docs/14/sql-syntax-lexical.html#id-1.5.3.5.13.2
    /// https://docs.microsoft.com/en-us/sql/t-sql/language-elements/operator-precedence-transact-sql?view=sql-server-ver16
    fn binding_strength(&self) -> i32;
}
impl SQLExpression for sql_ast::Expr {
    fn binding_strength(&self) -> i32 {
        // Strength of an expression depends only on the top-level operator, because all
        // other nested expressions can only have lower strength
        match self {
            sql_ast::Expr::BinaryOp { op, .. } => op.binding_strength(),

            sql_ast::Expr::UnaryOp { op, .. } => op.binding_strength(),

            sql_ast::Expr::Like { .. } | sql_ast::Expr::ILike { .. } => 7,

            sql_ast::Expr::IsNull(_) | sql_ast::Expr::IsNotNull(_) => 5,

            // all other items types bind stronger (function calls, literals, ...)
            _ => 20,
        }
    }
}
impl SQLExpression for BinaryOperator {
    fn binding_strength(&self) -> i32 {
        use BinaryOperator::*;
        match self {
            Modulo | Multiply | Divide => 11,
            Minus | Plus => 10,

            Gt | Lt | GtEq | LtEq | Eq | NotEq => 6,

            And => 3,
            Or => 2,

            _ => 9,
        }
    }
}
impl SQLExpression for UnaryOperator {
    fn binding_strength(&self) -> i32 {
        match self {
            UnaryOperator::Minus | UnaryOperator::Plus => 13,
            UnaryOperator::Not => 4,
            _ => 9,
        }
    }
}

impl From<Vec<Transform>> for AtomicTable {
    fn from(pipeline: Vec<Transform>) -> Self {
        AtomicTable {
            name: None,
            pipeline,
            frame: None,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{parser::parse, semantic::resolve};
    use insta::assert_yaml_snapshot;
    use serde_yaml::from_str;

    #[test]
    fn test_range_of_ranges() -> Result<()> {
        let range1 = Range::from_ints(Some(1), Some(10));
        let range2 = Range::from_ints(Some(5), Some(6));
        let range3 = Range::from_ints(Some(5), None);
        let range4 = Range::from_ints(None, Some(8));
        let range5 = Range::from_ints(Some(5), Some(5));

        assert!(range_of_ranges(vec![range1.clone()])?.end.is_some());

        assert_yaml_snapshot!(range_of_ranges(vec![range1.clone()])?, @r###"
        ---
        start: 1
        end: 10
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range1.clone(), range1.clone()])?, @r###"
        ---
        start: 1
        end: 10
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range1.clone(), range2.clone()])?, @r###"
        ---
        start: 5
        end: 6
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range2.clone(), range1.clone()])?, @r###"
        ---
        start: 5
        end: 6
        "###);

        // We can't get 5..6 from 5..6.
        assert!(range_of_ranges(vec![range2.clone(), range2.clone()]).is_err());

        assert_yaml_snapshot!(range_of_ranges(vec![range3.clone(), range3.clone()])?, @r###"
        ---
        start: 9
        end: ~
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range1, range3])?, @r###"
        ---
        start: 5
        end: 10
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range2, range4.clone()])?, @r###"
        ---
        start: 5
        end: 6
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range4.clone(), range4])?, @r###"
        ---
        start: ~
        end: 8
        "###);

        assert_yaml_snapshot!(range_of_ranges(vec![range5])?, @r###"
        ---
        start: 5
        end: 5
        "###);

        Ok(())
    }

    fn parse_and_resolve(prql: &str) -> Result<Vec<Transform>> {
        Ok(resolve(parse(prql)?, None)?.0.main_pipeline)
    }

    #[test]
    fn test_ctes_of_pipeline() {
        let mut context = MaterializationContext::default();

        // One aggregate, take at the end
        let prql: &str = r###"
        from employees
        filter country == "USA"
        aggregate [sal = average salary]
        sort sal
        take 20
        "###;

        let pipeline = parse_and_resolve(prql).unwrap();
        let queries = atomic_pipelines_of_pipeline(pipeline, &mut context).unwrap();
        assert_eq!(queries.len(), 1);

        // One aggregate, but take at the top
        let prql: &str = r###"
        from employees
        take 20
        filter country == "USA"
        aggregate [sal = average salary]
        sort sal
        "###;

        let pipeline = parse_and_resolve(prql).unwrap();
        let queries = atomic_pipelines_of_pipeline(pipeline, &mut context).unwrap();
        assert_eq!(queries.len(), 2);

        // A take, then two aggregates
        let prql: &str = r###"
        from employees
        take 20
        filter country == "USA"
        aggregate [sal = average salary]
        aggregate [sal = average sal]
        sort sal
        "###;

        let pipeline = parse_and_resolve(prql).unwrap();
        let queries = atomic_pipelines_of_pipeline(pipeline, &mut context).unwrap();
        assert_eq!(queries.len(), 3);

        // A take, then a select
        let prql: &str = r###"
        from employees
        take 20
        select first_name
        "###;

        let pipeline = parse_and_resolve(prql).unwrap();
        let queries = atomic_pipelines_of_pipeline(pipeline, &mut context).unwrap();
        assert_eq!(queries.len(), 1);
    }
    #[test]
    fn test_try_from_s_string_to_expr() -> Result<()> {
        let dialect = Dialect::Generic.handler();
        let ast: Expr = from_str(
            r"
        SString:
        - String: SUM(
        - Expr:
            Ident: col
        - String: )
        ",
        )?;
        let expr: sql_ast::Expr = translate_expr_kind(ast.kind, dialect.as_ref())?;
        assert_yaml_snapshot!(
            expr, @r###"
        ---
        Identifier:
          value: SUM(col)
          quote_style: ~
        "###
        );
        Ok(())
    }
}
