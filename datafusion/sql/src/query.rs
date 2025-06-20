// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::sync::Arc;

use crate::planner::{ContextProvider, PlannerContext, SqlToRel};

use crate::stack::StackGuard;
use datafusion_common::{not_impl_err, Constraints, DFSchema, Result};
use datafusion_expr::expr::Sort;

use datafusion_expr::{
    CreateMemoryTable, DdlStatement, Distinct, Expr, LogicalPlan, LogicalPlanBuilder,
};
use sqlparser::ast::{
    Expr as SQLExpr, Ident, LimitClause, OrderBy, OrderByExpr, OrderByKind, Query,
    SelectInto, SetExpr,
};
use sqlparser::tokenizer::Span;

impl<S: ContextProvider> SqlToRel<'_, S> {
    /// Generate a logical plan from an SQL query/subquery
    pub(crate) fn query_to_plan(
        &self,
        query: Query,
        outer_planner_context: &mut PlannerContext,
    ) -> Result<LogicalPlan> {
        // Each query has its own planner context, including CTEs that are visible within that query.
        // It also inherits the CTEs from the outer query by cloning the outer planner context.
        let mut query_plan_context = outer_planner_context.clone();
        let planner_context = &mut query_plan_context;

        if let Some(with) = query.with {
            self.plan_with_clause(with, planner_context)?;
        }

        let set_expr = *query.body;
        match set_expr {
            SetExpr::Select(mut select) => {
                let select_into = select.into.take();
                let plan =
                    self.select_to_plan(*select, query.order_by, planner_context)?;
                let plan = self.limit(plan, query.limit_clause, planner_context)?;
                // Process the `SELECT INTO` after `LIMIT`.
                self.select_into(plan, select_into)
            }
            other => {
                // The functions called from `set_expr_to_plan()` need more than 128KB
                // stack in debug builds as investigated in:
                // https://github.com/apache/datafusion/pull/13310#discussion_r1836813902
                let plan = {
                    // scope for dropping _guard
                    let _guard = StackGuard::new(256 * 1024);
                    self.set_expr_to_plan(other, planner_context)
                }?;
                let oby_exprs = to_order_by_exprs(query.order_by)?;
                let order_by_rex = self.order_by_to_sort_expr(
                    oby_exprs,
                    plan.schema(),
                    planner_context,
                    true,
                    None,
                )?;
                let plan = self.order_by(plan, order_by_rex)?;
                self.limit(plan, query.limit_clause, planner_context)
            }
        }
    }

    /// Wrap a plan in a limit
    fn limit(
        &self,
        input: LogicalPlan,
        limit_clause: Option<LimitClause>,
        planner_context: &mut PlannerContext,
    ) -> Result<LogicalPlan> {
        let Some(limit_clause) = limit_clause else {
            return Ok(input);
        };

        let empty_schema = DFSchema::empty();

        let (skip, fetch, limit_by_exprs) = match limit_clause {
            LimitClause::LimitOffset {
                limit,
                offset,
                limit_by,
            } => {
                let skip = offset
                    .map(|o| self.sql_to_expr(o.value, &empty_schema, planner_context))
                    .transpose()?;

                let fetch = limit
                    .map(|e| self.sql_to_expr(e, &empty_schema, planner_context))
                    .transpose()?;

                let limit_by_exprs = limit_by
                    .into_iter()
                    .map(|e| self.sql_to_expr(e, &empty_schema, planner_context))
                    .collect::<Result<Vec<_>>>()?;

                (skip, fetch, limit_by_exprs)
            }
            LimitClause::OffsetCommaLimit { offset, limit } => {
                let skip =
                    Some(self.sql_to_expr(offset, &empty_schema, planner_context)?);
                let fetch =
                    Some(self.sql_to_expr(limit, &empty_schema, planner_context)?);
                (skip, fetch, vec![])
            }
        };

        if !limit_by_exprs.is_empty() {
            return not_impl_err!("LIMIT BY clause is not supported yet");
        }

        if skip.is_none() && fetch.is_none() {
            return Ok(input);
        }

        LogicalPlanBuilder::from(input)
            .limit_by_expr(skip, fetch)?
            .build()
    }

    /// Wrap the logical in a sort
    pub(super) fn order_by(
        &self,
        plan: LogicalPlan,
        order_by: Vec<Sort>,
    ) -> Result<LogicalPlan> {
        if order_by.is_empty() {
            return Ok(plan);
        }

        if let LogicalPlan::Distinct(Distinct::On(ref distinct_on)) = plan {
            // In case of `DISTINCT ON` we must capture the sort expressions since during the plan
            // optimization we're effectively doing a `first_value` aggregation according to them.
            let distinct_on = distinct_on.clone().with_sort_expr(order_by)?;
            Ok(LogicalPlan::Distinct(Distinct::On(distinct_on)))
        } else {
            LogicalPlanBuilder::from(plan).sort(order_by)?.build()
        }
    }

    /// Wrap the logical plan in a `SelectInto`
    fn select_into(
        &self,
        plan: LogicalPlan,
        select_into: Option<SelectInto>,
    ) -> Result<LogicalPlan> {
        match select_into {
            Some(into) => Ok(LogicalPlan::Ddl(DdlStatement::CreateMemoryTable(
                CreateMemoryTable {
                    name: self.object_name_to_table_reference(into.name)?,
                    constraints: Constraints::default(),
                    input: Arc::new(plan),
                    if_not_exists: false,
                    or_replace: false,
                    temporary: false,
                    column_defaults: vec![],
                },
            ))),
            _ => Ok(plan),
        }
    }
}

/// Returns the order by expressions from the query.
fn to_order_by_exprs(order_by: Option<OrderBy>) -> Result<Vec<OrderByExpr>> {
    to_order_by_exprs_with_select(order_by, None)
}

/// Returns the order by expressions from the query with the select expressions.
pub(crate) fn to_order_by_exprs_with_select(
    order_by: Option<OrderBy>,
    select_exprs: Option<&Vec<Expr>>,
) -> Result<Vec<OrderByExpr>> {
    let Some(OrderBy { kind, interpolate }) = order_by else {
        // If no order by, return an empty array.
        return Ok(vec![]);
    };
    if let Some(_interpolate) = interpolate {
        return not_impl_err!("ORDER BY INTERPOLATE is not supported");
    }
    match kind {
        OrderByKind::All(order_by_options) => {
            let Some(exprs) = select_exprs else {
                return Ok(vec![]);
            };
            let order_by_exprs = exprs
                .iter()
                .map(|select_expr| match select_expr {
                    Expr::Column(column) => Ok(OrderByExpr {
                        expr: SQLExpr::Identifier(Ident {
                            value: column.name.clone(),
                            quote_style: None,
                            span: Span::empty(),
                        }),
                        options: order_by_options.clone(),
                        with_fill: None,
                    }),
                    // TODO: Support other types of expressions
                    _ => not_impl_err!(
                        "ORDER BY ALL is not supported for non-column expressions"
                    ),
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(order_by_exprs)
        }
        OrderByKind::Expressions(order_by_exprs) => Ok(order_by_exprs),
    }
}
