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

use core::fmt;
use std::ops::ControlFlow;

use sqlparser::ast::helpers::attached_token::AttachedToken;
use sqlparser::ast::{
    self, visit_expressions_mut, LimitClause, OrderByKind, SelectFlavor,
};

#[derive(Clone)]
pub struct QueryBuilder {
    with: Option<ast::With>,
    body: Option<Box<ast::SetExpr>>,
    order_by_kind: Option<OrderByKind>,
    limit: Option<ast::Expr>,
    limit_by: Vec<ast::Expr>,
    offset: Option<ast::Offset>,
    fetch: Option<ast::Fetch>,
    locks: Vec<ast::LockClause>,
    for_clause: Option<ast::ForClause>,
    // If true, we need to unparse LogicalPlan::Union as a SQL `UNION` rather than a `UNION ALL`.
    distinct_union: bool,
}

#[allow(dead_code)]
impl QueryBuilder {
    pub fn with(&mut self, value: Option<ast::With>) -> &mut Self {
        self.with = value;
        self
    }
    pub fn body(&mut self, value: Box<ast::SetExpr>) -> &mut Self {
        self.body = Some(value);
        self
    }
    pub fn take_body(&mut self) -> Option<Box<ast::SetExpr>> {
        self.body.take()
    }
    pub fn order_by(&mut self, value: OrderByKind) -> &mut Self {
        self.order_by_kind = Some(value);
        self
    }
    pub fn limit(&mut self, value: Option<ast::Expr>) -> &mut Self {
        self.limit = value;
        self
    }
    pub fn limit_by(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.limit_by = value;
        self
    }
    pub fn offset(&mut self, value: Option<ast::Offset>) -> &mut Self {
        self.offset = value;
        self
    }
    pub fn fetch(&mut self, value: Option<ast::Fetch>) -> &mut Self {
        self.fetch = value;
        self
    }
    pub fn locks(&mut self, value: Vec<ast::LockClause>) -> &mut Self {
        self.locks = value;
        self
    }
    pub fn for_clause(&mut self, value: Option<ast::ForClause>) -> &mut Self {
        self.for_clause = value;
        self
    }
    pub fn distinct_union(&mut self) -> &mut Self {
        self.distinct_union = true;
        self
    }
    pub fn is_distinct_union(&self) -> bool {
        self.distinct_union
    }
    pub fn build(&self) -> Result<ast::Query, BuilderError> {
        let order_by = self
            .order_by_kind
            .as_ref()
            .map(|order_by_kind| ast::OrderBy {
                kind: order_by_kind.clone(),
                interpolate: None,
            });

        Ok(ast::Query {
            with: self.with.clone(),
            body: match self.body {
                Some(ref value) => value.clone(),
                None => return Err(Into::into(UninitializedFieldError::from("body"))),
            },
            order_by,
            limit_clause: Some(LimitClause::LimitOffset {
                limit: self.limit.clone(),
                offset: self.offset.clone(),
                limit_by: self.limit_by.clone(),
            }),
            fetch: self.fetch.clone(),
            locks: self.locks.clone(),
            for_clause: self.for_clause.clone(),
            settings: None,
            format_clause: None,
        })
    }
    fn create_empty() -> Self {
        Self {
            with: Default::default(),
            body: Default::default(),
            order_by_kind: Default::default(),
            limit: Default::default(),
            limit_by: Default::default(),
            offset: Default::default(),
            fetch: Default::default(),
            locks: Default::default(),
            for_clause: Default::default(),
            distinct_union: false,
        }
    }
}
impl Default for QueryBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

#[derive(Clone)]
pub struct SelectBuilder {
    distinct: Option<ast::Distinct>,
    top: Option<ast::Top>,
    projection: Vec<ast::SelectItem>,
    into: Option<ast::SelectInto>,
    from: Vec<TableWithJoinsBuilder>,
    lateral_views: Vec<ast::LateralView>,
    selection: Option<ast::Expr>,
    group_by: Option<ast::GroupByExpr>,
    cluster_by: Vec<ast::Expr>,
    distribute_by: Vec<ast::Expr>,
    sort_by: Vec<ast::Expr>,
    having: Option<ast::Expr>,
    named_window: Vec<ast::NamedWindowDefinition>,
    qualify: Option<ast::Expr>,
    value_table_mode: Option<ast::ValueTableMode>,
    flavor: Option<SelectFlavor>,
}

#[allow(dead_code)]
impl SelectBuilder {
    pub fn distinct(&mut self, value: Option<ast::Distinct>) -> &mut Self {
        self.distinct = value;
        self
    }
    pub fn top(&mut self, value: Option<ast::Top>) -> &mut Self {
        self.top = value;
        self
    }
    pub fn projection(&mut self, value: Vec<ast::SelectItem>) -> &mut Self {
        self.projection = value;
        self
    }
    pub fn pop_projections(&mut self) -> Vec<ast::SelectItem> {
        let ret = self.projection.clone();
        self.projection.clear();
        ret
    }
    pub fn already_projected(&self) -> bool {
        !self.projection.is_empty()
    }
    pub fn into(&mut self, value: Option<ast::SelectInto>) -> &mut Self {
        self.into = value;
        self
    }
    pub fn from(&mut self, value: Vec<TableWithJoinsBuilder>) -> &mut Self {
        self.from = value;
        self
    }
    pub fn push_from(&mut self, value: TableWithJoinsBuilder) -> &mut Self {
        self.from.push(value);
        self
    }
    pub fn pop_from(&mut self) -> Option<TableWithJoinsBuilder> {
        self.from.pop()
    }
    pub fn lateral_views(&mut self, value: Vec<ast::LateralView>) -> &mut Self {
        self.lateral_views = value;
        self
    }

    /// Replaces the selection with a new value.
    ///
    /// This function is used to replace a specific expression within the selection.
    /// Unlike the `selection` method which combines existing and new selections with AND,
    /// this method searches for and replaces occurrences of a specific expression.
    ///
    /// This method is primarily used to modify LEFT MARK JOIN expressions.
    /// When processing a LEFT MARK JOIN, we need to replace the placeholder expression
    /// with the actual join condition in the selection clause.
    ///
    /// # Arguments
    ///
    /// * `existing_expr` - The expression to replace
    /// * `value` - The new expression to set as the selection
    pub fn replace_mark(
        &mut self,
        existing_expr: &ast::Expr,
        value: &ast::Expr,
    ) -> &mut Self {
        if let Some(selection) = &mut self.selection {
            let _ = visit_expressions_mut(selection, |expr| {
                if expr == existing_expr {
                    *expr = value.clone();
                }
                ControlFlow::<()>::Continue(())
            });
        }
        self
    }

    pub fn selection(&mut self, value: Option<ast::Expr>) -> &mut Self {
        // With filter pushdown optimization, the LogicalPlan can have filters defined as part of `TableScan` and `Filter` nodes.
        // To avoid overwriting one of the filters, we combine the existing filter with the additional filter.
        // Example:                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                       |
        // |  Projection: customer.c_phone AS cntrycode, customer.c_acctbal                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                      |
        // |   Filter: CAST(customer.c_acctbal AS Decimal128(38, 6)) > (<subquery>)                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                              |
        // |     Subquery:
        // |     ..                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
        // |     TableScan: customer, full_filters=[customer.c_mktsegment = Utf8("BUILDING")]
        match (&self.selection, value) {
            (Some(existing_selection), Some(new_selection)) => {
                self.selection = Some(ast::Expr::BinaryOp {
                    left: Box::new(existing_selection.clone()),
                    op: ast::BinaryOperator::And,
                    right: Box::new(new_selection),
                });
            }
            (None, Some(new_selection)) => {
                self.selection = Some(new_selection);
            }
            (_, None) => (),
        }

        self
    }
    pub fn group_by(&mut self, value: ast::GroupByExpr) -> &mut Self {
        self.group_by = Some(value);
        self
    }
    pub fn cluster_by(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.cluster_by = value;
        self
    }
    pub fn distribute_by(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.distribute_by = value;
        self
    }
    pub fn sort_by(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.sort_by = value;
        self
    }
    pub fn having(&mut self, value: Option<ast::Expr>) -> &mut Self {
        self.having = value;
        self
    }
    pub fn named_window(&mut self, value: Vec<ast::NamedWindowDefinition>) -> &mut Self {
        self.named_window = value;
        self
    }
    pub fn qualify(&mut self, value: Option<ast::Expr>) -> &mut Self {
        self.qualify = value;
        self
    }
    pub fn value_table_mode(&mut self, value: Option<ast::ValueTableMode>) -> &mut Self {
        self.value_table_mode = value;
        self
    }
    pub fn build(&self) -> Result<ast::Select, BuilderError> {
        Ok(ast::Select {
            distinct: self.distinct.clone(),
            top_before_distinct: false,
            top: self.top.clone(),
            projection: self.projection.clone(),
            into: self.into.clone(),
            from: self
                .from
                .iter()
                .filter_map(|b| b.build().transpose())
                .collect::<Result<Vec<_>, BuilderError>>()?,
            lateral_views: self.lateral_views.clone(),
            selection: self.selection.clone(),
            group_by: match self.group_by {
                Some(ref value) => value.clone(),
                None => {
                    return Err(Into::into(UninitializedFieldError::from("group_by")))
                }
            },
            cluster_by: self.cluster_by.clone(),
            distribute_by: self.distribute_by.clone(),
            sort_by: self.sort_by.clone(),
            having: self.having.clone(),
            named_window: self.named_window.clone(),
            qualify: self.qualify.clone(),
            value_table_mode: self.value_table_mode,
            connect_by: None,
            window_before_qualify: false,
            prewhere: None,
            select_token: AttachedToken::empty(),
            flavor: match self.flavor {
                Some(ref value) => value.clone(),
                None => return Err(Into::into(UninitializedFieldError::from("flavor"))),
            },
        })
    }
    fn create_empty() -> Self {
        Self {
            distinct: Default::default(),
            top: Default::default(),
            projection: Default::default(),
            into: Default::default(),
            from: Default::default(),
            lateral_views: Default::default(),
            selection: Default::default(),
            group_by: Some(ast::GroupByExpr::Expressions(Vec::new(), Vec::new())),
            cluster_by: Default::default(),
            distribute_by: Default::default(),
            sort_by: Default::default(),
            having: Default::default(),
            named_window: Default::default(),
            qualify: Default::default(),
            value_table_mode: Default::default(),
            flavor: Some(SelectFlavor::Standard),
        }
    }
}
impl Default for SelectBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

#[derive(Clone)]
pub struct TableWithJoinsBuilder {
    relation: Option<RelationBuilder>,
    joins: Vec<ast::Join>,
}

#[allow(dead_code)]
impl TableWithJoinsBuilder {
    pub fn relation(&mut self, value: RelationBuilder) -> &mut Self {
        self.relation = Some(value);
        self
    }

    pub fn joins(&mut self, value: Vec<ast::Join>) -> &mut Self {
        self.joins = value;
        self
    }
    pub fn push_join(&mut self, value: ast::Join) -> &mut Self {
        self.joins.push(value);
        self
    }

    pub fn build(&self) -> Result<Option<ast::TableWithJoins>, BuilderError> {
        match self.relation {
            Some(ref value) => match value.build()? {
                Some(relation) => Ok(Some(ast::TableWithJoins {
                    relation,
                    joins: self.joins.clone(),
                })),
                None => Ok(None),
            },
            None => Err(Into::into(UninitializedFieldError::from("relation"))),
        }
    }
    fn create_empty() -> Self {
        Self {
            relation: Default::default(),
            joins: Default::default(),
        }
    }
}
impl Default for TableWithJoinsBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

#[derive(Clone)]
pub struct RelationBuilder {
    relation: Option<TableFactorBuilder>,
}

#[allow(dead_code)]
#[derive(Clone)]
#[allow(clippy::large_enum_variant)]
enum TableFactorBuilder {
    Table(TableRelationBuilder),
    Derived(DerivedRelationBuilder),
    Unnest(UnnestRelationBuilder),
    Empty,
}

#[allow(dead_code)]
impl RelationBuilder {
    pub fn has_relation(&self) -> bool {
        self.relation.is_some()
    }
    pub fn table(&mut self, value: TableRelationBuilder) -> &mut Self {
        self.relation = Some(TableFactorBuilder::Table(value));
        self
    }
    pub fn derived(&mut self, value: DerivedRelationBuilder) -> &mut Self {
        self.relation = Some(TableFactorBuilder::Derived(value));
        self
    }

    pub fn unnest(&mut self, value: UnnestRelationBuilder) -> &mut Self {
        self.relation = Some(TableFactorBuilder::Unnest(value));
        self
    }

    pub fn empty(&mut self) -> &mut Self {
        self.relation = Some(TableFactorBuilder::Empty);
        self
    }
    pub fn alias(&mut self, value: Option<ast::TableAlias>) -> &mut Self {
        let new = self;
        match new.relation {
            Some(TableFactorBuilder::Table(ref mut rel_builder)) => {
                rel_builder.alias = value;
            }
            Some(TableFactorBuilder::Derived(ref mut rel_builder)) => {
                rel_builder.alias = value;
            }
            Some(TableFactorBuilder::Unnest(ref mut rel_builder)) => {
                rel_builder.alias = value;
            }
            Some(TableFactorBuilder::Empty) => (),
            None => (),
        }
        new
    }
    pub fn build(&self) -> Result<Option<ast::TableFactor>, BuilderError> {
        Ok(match self.relation {
            Some(TableFactorBuilder::Table(ref value)) => Some(value.build()?),
            Some(TableFactorBuilder::Derived(ref value)) => Some(value.build()?),
            Some(TableFactorBuilder::Unnest(ref value)) => Some(value.build()?),
            Some(TableFactorBuilder::Empty) => None,
            None => return Err(Into::into(UninitializedFieldError::from("relation"))),
        })
    }
    fn create_empty() -> Self {
        Self {
            relation: Default::default(),
        }
    }
}
impl Default for RelationBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

#[derive(Clone)]
pub struct TableRelationBuilder {
    name: Option<ast::ObjectName>,
    alias: Option<ast::TableAlias>,
    args: Option<Vec<ast::FunctionArg>>,
    with_hints: Vec<ast::Expr>,
    version: Option<ast::TableVersion>,
    partitions: Vec<ast::Ident>,
    index_hints: Vec<ast::TableIndexHints>,
}

#[allow(dead_code)]
impl TableRelationBuilder {
    pub fn name(&mut self, value: ast::ObjectName) -> &mut Self {
        self.name = Some(value);
        self
    }
    pub fn alias(&mut self, value: Option<ast::TableAlias>) -> &mut Self {
        self.alias = value;
        self
    }
    pub fn args(&mut self, value: Option<Vec<ast::FunctionArg>>) -> &mut Self {
        self.args = value;
        self
    }
    pub fn with_hints(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.with_hints = value;
        self
    }
    pub fn version(&mut self, value: Option<ast::TableVersion>) -> &mut Self {
        self.version = value;
        self
    }
    pub fn partitions(&mut self, value: Vec<ast::Ident>) -> &mut Self {
        self.partitions = value;
        self
    }
    pub fn index_hints(&mut self, value: Vec<ast::TableIndexHints>) -> &mut Self {
        self.index_hints = value;
        self
    }
    pub fn build(&self) -> Result<ast::TableFactor, BuilderError> {
        Ok(ast::TableFactor::Table {
            name: match self.name {
                Some(ref value) => value.clone(),
                None => return Err(Into::into(UninitializedFieldError::from("name"))),
            },
            alias: self.alias.clone(),
            args: self.args.clone().map(|args| ast::TableFunctionArgs {
                args,
                settings: None,
            }),
            with_hints: self.with_hints.clone(),
            version: self.version.clone(),
            partitions: self.partitions.clone(),
            with_ordinality: false,
            json_path: None,
            sample: None,
            index_hints: self.index_hints.clone(),
        })
    }
    fn create_empty() -> Self {
        Self {
            name: Default::default(),
            alias: Default::default(),
            args: Default::default(),
            with_hints: Default::default(),
            version: Default::default(),
            partitions: Default::default(),
            index_hints: Default::default(),
        }
    }
}
impl Default for TableRelationBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}
#[derive(Clone)]
pub struct DerivedRelationBuilder {
    lateral: Option<bool>,
    subquery: Option<Box<ast::Query>>,
    alias: Option<ast::TableAlias>,
}

#[allow(dead_code)]
impl DerivedRelationBuilder {
    pub fn lateral(&mut self, value: bool) -> &mut Self {
        self.lateral = Some(value);
        self
    }
    pub fn subquery(&mut self, value: Box<ast::Query>) -> &mut Self {
        self.subquery = Some(value);
        self
    }
    pub fn alias(&mut self, value: Option<ast::TableAlias>) -> &mut Self {
        self.alias = value;
        self
    }
    fn build(&self) -> Result<ast::TableFactor, BuilderError> {
        Ok(ast::TableFactor::Derived {
            lateral: match self.lateral {
                Some(ref value) => *value,
                None => return Err(Into::into(UninitializedFieldError::from("lateral"))),
            },
            subquery: match self.subquery {
                Some(ref value) => value.clone(),
                None => {
                    return Err(Into::into(UninitializedFieldError::from("subquery")))
                }
            },
            alias: self.alias.clone(),
        })
    }
    fn create_empty() -> Self {
        Self {
            lateral: Default::default(),
            subquery: Default::default(),
            alias: Default::default(),
        }
    }
}
impl Default for DerivedRelationBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

#[derive(Clone)]
pub struct UnnestRelationBuilder {
    pub alias: Option<ast::TableAlias>,
    pub array_exprs: Vec<ast::Expr>,
    with_offset: bool,
    with_offset_alias: Option<ast::Ident>,
    with_ordinality: bool,
}

#[allow(dead_code)]
impl UnnestRelationBuilder {
    pub fn alias(&mut self, value: Option<ast::TableAlias>) -> &mut Self {
        self.alias = value;
        self
    }
    pub fn array_exprs(&mut self, value: Vec<ast::Expr>) -> &mut Self {
        self.array_exprs = value;
        self
    }

    pub fn with_offset(&mut self, value: bool) -> &mut Self {
        self.with_offset = value;
        self
    }

    pub fn with_offset_alias(&mut self, value: Option<ast::Ident>) -> &mut Self {
        self.with_offset_alias = value;
        self
    }

    pub fn with_ordinality(&mut self, value: bool) -> &mut Self {
        self.with_ordinality = value;
        self
    }

    pub fn build(&self) -> Result<ast::TableFactor, BuilderError> {
        Ok(ast::TableFactor::UNNEST {
            alias: self.alias.clone(),
            array_exprs: self.array_exprs.clone(),
            with_offset: self.with_offset,
            with_offset_alias: self.with_offset_alias.clone(),
            with_ordinality: self.with_ordinality,
        })
    }

    fn create_empty() -> Self {
        Self {
            alias: Default::default(),
            array_exprs: Default::default(),
            with_offset: Default::default(),
            with_offset_alias: Default::default(),
            with_ordinality: Default::default(),
        }
    }
}

impl Default for UnnestRelationBuilder {
    fn default() -> Self {
        Self::create_empty()
    }
}

/// Runtime error when a `build()` method is called and one or more required fields
/// do not have a value.
#[derive(Debug, Clone)]
pub struct UninitializedFieldError(&'static str);

impl UninitializedFieldError {
    /// Create a new `UninitializedFieldError` for the specified field name.
    pub fn new(field_name: &'static str) -> Self {
        UninitializedFieldError(field_name)
    }

    /// Get the name of the first-declared field that wasn't initialized
    pub fn field_name(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for UninitializedFieldError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Field not initialized: {}", self.0)
    }
}

impl From<&'static str> for UninitializedFieldError {
    fn from(field_name: &'static str) -> Self {
        Self::new(field_name)
    }
}
impl std::error::Error for UninitializedFieldError {}

#[derive(Debug)]
pub enum BuilderError {
    UninitializedField(&'static str),
    ValidationError(String),
}
impl From<UninitializedFieldError> for BuilderError {
    fn from(s: UninitializedFieldError) -> Self {
        Self::UninitializedField(s.field_name())
    }
}
impl From<String> for BuilderError {
    fn from(s: String) -> Self {
        Self::ValidationError(s)
    }
}
impl fmt::Display for BuilderError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::UninitializedField(ref field) => {
                write!(f, "`{field}` must be initialized")
            }
            Self::ValidationError(ref error) => write!(f, "{error}"),
        }
    }
}
impl std::error::Error for BuilderError {}
