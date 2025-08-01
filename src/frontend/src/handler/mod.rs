// Copyright 2025 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use itertools::Itertools;
use pgwire::pg_field_descriptor::PgFieldDescriptor;
use pgwire::pg_response::StatementType::{self, ABORT, BEGIN, COMMIT, ROLLBACK, START_TRANSACTION};
use pgwire::pg_response::{PgResponse, PgResponseBuilder, RowSetResult};
use pgwire::pg_server::BoxedError;
use pgwire::types::{Format, Row};
use risingwave_common::catalog::AlterDatabaseParam;
use risingwave_common::types::Fields;
use risingwave_common::util::iter_util::ZipEqFast;
use risingwave_common::{bail, bail_not_implemented};
use risingwave_pb::meta::PbThrottleTarget;
use risingwave_sqlparser::ast::*;
use thiserror_ext::AsReport;
use util::get_table_catalog_by_table_name;

use self::util::{DataChunkToRowSetAdapter, SourceSchemaCompatExt};
use crate::catalog::table_catalog::TableType;
use crate::error::{ErrorCode, Result};
use crate::handler::cancel_job::handle_cancel;
use crate::handler::kill_process::handle_kill;
use crate::scheduler::{DistributedQueryStream, LocalQueryStream};
use crate::session::SessionImpl;
use crate::utils::WithOptions;

mod alter_database_param;
mod alter_mv;
mod alter_owner;
mod alter_parallelism;
mod alter_rename;
mod alter_resource_group;
mod alter_secret;
mod alter_set_schema;
mod alter_sink_props;
mod alter_source_column;
mod alter_source_props;
mod alter_source_with_sr;
mod alter_streaming_enable_unaligned_join;
mod alter_streaming_rate_limit;
mod alter_swap_rename;
mod alter_system;
mod alter_table_column;
pub mod alter_table_drop_connector;
mod alter_table_with_sr;
pub mod alter_user;
pub mod cancel_job;
pub mod close_cursor;
mod comment;
pub mod create_aggregate;
pub mod create_connection;
mod create_database;
pub mod create_function;
pub mod create_index;
pub mod create_mv;
pub mod create_schema;
pub mod create_secret;
pub mod create_sink;
pub mod create_source;
pub mod create_sql_function;
pub mod create_subscription;
pub mod create_table;
pub mod create_table_as;
pub mod create_user;
pub mod create_view;
pub mod declare_cursor;
pub mod describe;
pub mod discard;
mod drop_connection;
mod drop_database;
pub mod drop_function;
mod drop_index;
pub mod drop_mv;
mod drop_schema;
pub mod drop_secret;
pub mod drop_sink;
pub mod drop_source;
pub mod drop_subscription;
pub mod drop_table;
pub mod drop_user;
mod drop_view;
pub mod explain;
pub mod explain_analyze_stream_job;
pub mod extended_handle;
pub mod fetch_cursor;
mod flush;
pub mod handle_privilege;
pub mod kill_process;
mod prepared_statement;
pub mod privilege;
pub mod query;
mod recover;
pub mod show;
mod transaction;
mod use_db;
pub mod util;
pub mod vacuum;
pub mod variable;
mod wait;

pub use alter_table_column::{get_new_table_definition_for_cdc_table, get_replace_table_plan};

/// The [`PgResponseBuilder`] used by RisingWave.
pub type RwPgResponseBuilder = PgResponseBuilder<PgResponseStream>;

/// The [`PgResponse`] used by RisingWave.
pub type RwPgResponse = PgResponse<PgResponseStream>;

#[easy_ext::ext(RwPgResponseBuilderExt)]
impl RwPgResponseBuilder {
    /// Append rows to the response.
    pub fn rows<T: Fields>(self, rows: impl IntoIterator<Item = T>) -> Self {
        let fields = T::fields();
        self.values(
            rows.into_iter()
                .map(|row| {
                    Row::new(
                        row.into_owned_row()
                            .into_iter()
                            .zip_eq_fast(&fields)
                            .map(|(datum, (_, ty))| {
                                datum.map(|scalar| {
                                    scalar.as_scalar_ref_impl().text_format(ty).into()
                                })
                            })
                            .collect(),
                    )
                })
                .collect_vec()
                .into(),
            fields_to_descriptors(fields),
        )
    }
}

pub fn fields_to_descriptors(
    fields: Vec<(&str, risingwave_common::types::DataType)>,
) -> Vec<PgFieldDescriptor> {
    fields
        .iter()
        .map(|(name, ty)| PgFieldDescriptor::new(name.to_string(), ty.to_oid(), ty.type_len()))
        .collect()
}

pub enum PgResponseStream {
    LocalQuery(DataChunkToRowSetAdapter<LocalQueryStream>),
    DistributedQuery(DataChunkToRowSetAdapter<DistributedQueryStream>),
    Rows(BoxStream<'static, RowSetResult>),
}

impl Stream for PgResponseStream {
    type Item = std::result::Result<Vec<Row>, BoxedError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match &mut *self {
            PgResponseStream::LocalQuery(inner) => inner.poll_next_unpin(cx),
            PgResponseStream::DistributedQuery(inner) => inner.poll_next_unpin(cx),
            PgResponseStream::Rows(inner) => inner.poll_next_unpin(cx),
        }
    }
}

impl From<Vec<Row>> for PgResponseStream {
    fn from(rows: Vec<Row>) -> Self {
        Self::Rows(stream::iter(vec![Ok(rows)]).boxed())
    }
}

#[derive(Clone)]
pub struct HandlerArgs {
    pub session: Arc<SessionImpl>,
    pub sql: Arc<str>,
    pub normalized_sql: String,
    pub with_options: WithOptions,
}

impl HandlerArgs {
    pub fn new(session: Arc<SessionImpl>, stmt: &Statement, sql: Arc<str>) -> Result<Self> {
        Ok(Self {
            session,
            sql,
            with_options: WithOptions::try_from(stmt)?,
            normalized_sql: Self::normalize_sql(stmt),
        })
    }

    /// Get normalized SQL from the statement.
    ///
    /// - Generally, the normalized SQL is the unparsed (and formatted) result of the statement.
    /// - For `CREATE` statements, the clauses like `OR REPLACE` and `IF NOT EXISTS` are removed to
    ///   make it suitable for the `SHOW CREATE` statements.
    fn normalize_sql(stmt: &Statement) -> String {
        let mut stmt = stmt.clone();
        match &mut stmt {
            Statement::CreateView {
                or_replace,
                if_not_exists,
                ..
            } => {
                *or_replace = false;
                *if_not_exists = false;
            }
            Statement::CreateTable {
                or_replace,
                if_not_exists,
                ..
            } => {
                *or_replace = false;
                *if_not_exists = false;
            }
            Statement::CreateIndex { if_not_exists, .. } => {
                *if_not_exists = false;
            }
            Statement::CreateSource {
                stmt: CreateSourceStatement { if_not_exists, .. },
                ..
            } => {
                *if_not_exists = false;
            }
            Statement::CreateSink {
                stmt: CreateSinkStatement { if_not_exists, .. },
            } => {
                *if_not_exists = false;
            }
            Statement::CreateSubscription {
                stmt: CreateSubscriptionStatement { if_not_exists, .. },
            } => {
                *if_not_exists = false;
            }
            Statement::CreateConnection {
                stmt: CreateConnectionStatement { if_not_exists, .. },
            } => {
                *if_not_exists = false;
            }
            _ => {}
        }
        stmt.to_string()
    }
}

pub async fn handle(
    session: Arc<SessionImpl>,
    stmt: Statement,
    sql: Arc<str>,
    formats: Vec<Format>,
) -> Result<RwPgResponse> {
    session.clear_cancel_query_flag();
    let _guard = session.txn_begin_implicit();
    let handler_args = HandlerArgs::new(session, &stmt, sql)?;

    check_ban_ddl_for_iceberg_engine_table(handler_args.session.clone(), &stmt)?;

    match stmt {
        Statement::Explain {
            statement,
            analyze,
            options,
        } => explain::handle_explain(handler_args, *statement, options, analyze).await,
        Statement::ExplainAnalyzeStreamJob {
            target,
            duration_secs,
        } => {
            explain_analyze_stream_job::handle_explain_analyze_stream_job(
                handler_args,
                target,
                duration_secs,
            )
            .await
        }
        Statement::CreateSource { stmt } => {
            create_source::handle_create_source(handler_args, stmt).await
        }
        Statement::CreateSink { stmt } => {
            create_sink::handle_create_sink(handler_args, stmt, false).await
        }
        Statement::CreateSubscription { stmt } => {
            create_subscription::handle_create_subscription(handler_args, stmt).await
        }
        Statement::CreateConnection { stmt } => {
            create_connection::handle_create_connection(handler_args, stmt).await
        }
        Statement::CreateSecret { stmt } => {
            create_secret::handle_create_secret(handler_args, stmt).await
        }
        Statement::CreateFunction {
            or_replace,
            temporary,
            if_not_exists,
            name,
            args,
            returns,
            params,
            with_options,
        } => {
            // For general udf, `language` clause could be ignored
            // refer: https://github.com/risingwavelabs/risingwave/pull/10608
            if params.language.is_none()
                || !params
                    .language
                    .as_ref()
                    .unwrap()
                    .real_value()
                    .eq_ignore_ascii_case("sql")
            {
                create_function::handle_create_function(
                    handler_args,
                    or_replace,
                    temporary,
                    if_not_exists,
                    name,
                    args,
                    returns,
                    params,
                    with_options,
                )
                .await
            } else {
                create_sql_function::handle_create_sql_function(
                    handler_args,
                    or_replace,
                    temporary,
                    if_not_exists,
                    name,
                    args,
                    returns,
                    params,
                )
                .await
            }
        }
        Statement::CreateAggregate {
            or_replace,
            if_not_exists,
            name,
            args,
            returns,
            params,
            ..
        } => {
            create_aggregate::handle_create_aggregate(
                handler_args,
                or_replace,
                if_not_exists,
                name,
                args,
                returns,
                params,
            )
            .await
        }
        Statement::CreateTable {
            name,
            columns,
            wildcard_idx,
            constraints,
            query,
            with_options: _, // It is put in OptimizerContext
            // Not supported things
            or_replace,
            temporary,
            if_not_exists,
            format_encode,
            source_watermarks,
            append_only,
            on_conflict,
            with_version_column,
            cdc_table_info,
            include_column_options,
            webhook_info,
            engine,
        } => {
            if or_replace {
                bail_not_implemented!("CREATE OR REPLACE TABLE");
            }
            if temporary {
                bail_not_implemented!("CREATE TEMPORARY TABLE");
            }
            if let Some(query) = query {
                return create_table_as::handle_create_as(
                    handler_args,
                    name,
                    if_not_exists,
                    query,
                    columns,
                    append_only,
                    on_conflict,
                    with_version_column.map(|x| x.real_value()),
                    engine,
                )
                .await;
            }
            let format_encode = format_encode.map(|s| s.into_v2_with_warning());
            create_table::handle_create_table(
                handler_args,
                name,
                columns,
                wildcard_idx,
                constraints,
                if_not_exists,
                format_encode,
                source_watermarks,
                append_only,
                on_conflict,
                with_version_column.map(|x| x.real_value()),
                cdc_table_info,
                include_column_options,
                webhook_info,
                engine,
            )
            .await
        }
        Statement::CreateDatabase {
            db_name,
            if_not_exists,
            owner,
            resource_group,
            barrier_interval_ms,
            checkpoint_frequency,
        } => {
            create_database::handle_create_database(
                handler_args,
                db_name,
                if_not_exists,
                owner,
                resource_group,
                barrier_interval_ms,
                checkpoint_frequency,
            )
            .await
        }
        Statement::CreateSchema {
            schema_name,
            if_not_exists,
            owner,
        } => {
            create_schema::handle_create_schema(handler_args, schema_name, if_not_exists, owner)
                .await
        }
        Statement::CreateUser(stmt) => create_user::handle_create_user(handler_args, stmt).await,
        Statement::DeclareCursor { stmt } => {
            declare_cursor::handle_declare_cursor(handler_args, stmt).await
        }
        Statement::FetchCursor { stmt } => {
            fetch_cursor::handle_fetch_cursor(handler_args, stmt, &formats).await
        }
        Statement::CloseCursor { stmt } => {
            close_cursor::handle_close_cursor(handler_args, stmt).await
        }
        Statement::AlterUser(stmt) => alter_user::handle_alter_user(handler_args, stmt).await,
        Statement::Grant { .. } => {
            handle_privilege::handle_grant_privilege(handler_args, stmt).await
        }
        Statement::Revoke { .. } => {
            handle_privilege::handle_revoke_privilege(handler_args, stmt).await
        }
        Statement::Describe { name, kind } => match kind {
            DescribeKind::Fragments => {
                describe::handle_describe_fragments(handler_args, name).await
            }
            DescribeKind::Plain => describe::handle_describe(handler_args, name),
        },
        Statement::DescribeFragment { fragment_id } => {
            describe::handle_describe_fragment(handler_args, fragment_id).await
        }
        Statement::Discard(..) => discard::handle_discard(handler_args),
        Statement::ShowObjects {
            object: show_object,
            filter,
        } => show::handle_show_object(handler_args, show_object, filter).await,
        Statement::ShowCreateObject { create_type, name } => {
            show::handle_show_create_object(handler_args, create_type, name)
        }
        Statement::ShowTransactionIsolationLevel => {
            transaction::handle_show_isolation_level(handler_args)
        }
        Statement::Drop(DropStatement {
            object_type,
            object_name,
            if_exists,
            drop_mode,
        }) => {
            let cascade = if let AstOption::Some(DropMode::Cascade) = drop_mode {
                match object_type {
                    ObjectType::MaterializedView
                    | ObjectType::View
                    | ObjectType::Sink
                    | ObjectType::Source
                    | ObjectType::Subscription
                    | ObjectType::Index
                    | ObjectType::Table
                    | ObjectType::Schema
                    | ObjectType::Connection => true,
                    ObjectType::Database | ObjectType::User | ObjectType::Secret => {
                        bail_not_implemented!("DROP CASCADE");
                    }
                }
            } else {
                false
            };
            match object_type {
                ObjectType::Table => {
                    drop_table::handle_drop_table(handler_args, object_name, if_exists, cascade)
                        .await
                }
                ObjectType::MaterializedView => {
                    drop_mv::handle_drop_mv(handler_args, object_name, if_exists, cascade).await
                }
                ObjectType::Index => {
                    drop_index::handle_drop_index(handler_args, object_name, if_exists, cascade)
                        .await
                }
                ObjectType::Source => {
                    drop_source::handle_drop_source(handler_args, object_name, if_exists, cascade)
                        .await
                }
                ObjectType::Sink => {
                    drop_sink::handle_drop_sink(handler_args, object_name, if_exists, cascade).await
                }
                ObjectType::Subscription => {
                    drop_subscription::handle_drop_subscription(
                        handler_args,
                        object_name,
                        if_exists,
                        cascade,
                    )
                    .await
                }
                ObjectType::Database => {
                    drop_database::handle_drop_database(handler_args, object_name, if_exists).await
                }
                ObjectType::Schema => {
                    drop_schema::handle_drop_schema(handler_args, object_name, if_exists, cascade)
                        .await
                }
                ObjectType::User => {
                    drop_user::handle_drop_user(handler_args, object_name, if_exists).await
                }
                ObjectType::View => {
                    drop_view::handle_drop_view(handler_args, object_name, if_exists, cascade).await
                }
                ObjectType::Connection => {
                    drop_connection::handle_drop_connection(
                        handler_args,
                        object_name,
                        if_exists,
                        cascade,
                    )
                    .await
                }
                ObjectType::Secret => {
                    drop_secret::handle_drop_secret(handler_args, object_name, if_exists).await
                }
            }
        }
        // XXX: should we reuse Statement::Drop for DROP FUNCTION?
        Statement::DropFunction {
            if_exists,
            func_desc,
            option,
        } => {
            drop_function::handle_drop_function(handler_args, if_exists, func_desc, option, false)
                .await
        }
        Statement::DropAggregate {
            if_exists,
            func_desc,
            option,
        } => {
            drop_function::handle_drop_function(handler_args, if_exists, func_desc, option, true)
                .await
        }
        Statement::Query(_)
        | Statement::Insert { .. }
        | Statement::Delete { .. }
        | Statement::Update { .. } => query::handle_query(handler_args, stmt, formats).await,
        Statement::CreateView {
            materialized,
            if_not_exists,
            name,
            columns,
            query,
            with_options: _, // It is put in OptimizerContext
            or_replace,      // not supported
            emit_mode,
        } => {
            if or_replace {
                bail_not_implemented!("CREATE OR REPLACE VIEW");
            }
            if materialized {
                create_mv::handle_create_mv(
                    handler_args,
                    if_not_exists,
                    name,
                    *query,
                    columns,
                    emit_mode,
                )
                .await
            } else {
                create_view::handle_create_view(handler_args, if_not_exists, name, columns, *query)
                    .await
            }
        }
        Statement::Flush => flush::handle_flush(handler_args).await,
        Statement::Wait => wait::handle_wait(handler_args).await,
        Statement::Recover => recover::handle_recover(handler_args).await,
        Statement::SetVariable {
            local: _,
            variable,
            value,
        } => {
            // special handle for `use database`
            if variable.real_value().eq_ignore_ascii_case("database") {
                let x = variable::set_var_to_param_str(&value);
                let res = use_db::handle_use_db(
                    handler_args,
                    ObjectName::from(vec![Ident::from_real_value(
                        x.as_deref().unwrap_or("default"),
                    )]),
                )?;
                let mut builder = RwPgResponse::builder(StatementType::SET_VARIABLE);
                for notice in res.notices() {
                    builder = builder.notice(notice);
                }
                return Ok(builder.into());
            }
            variable::handle_set(handler_args, variable, value)
        }
        Statement::SetTimeZone { local: _, value } => {
            variable::handle_set_time_zone(handler_args, value)
        }
        Statement::ShowVariable { variable } => variable::handle_show(handler_args, variable),
        Statement::CreateIndex {
            name,
            table_name,
            columns,
            include,
            distributed_by,
            unique,
            if_not_exists,
        } => {
            if unique {
                bail_not_implemented!("create unique index");
            }

            create_index::handle_create_index(
                handler_args,
                if_not_exists,
                name,
                table_name,
                columns.to_vec(),
                include,
                distributed_by,
            )
            .await
        }
        Statement::AlterDatabase { name, operation } => match operation {
            AlterDatabaseOperation::RenameDatabase { database_name } => {
                alter_rename::handle_rename_database(handler_args, name, database_name).await
            }
            AlterDatabaseOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_DATABASE,
                )
                .await
            }
            AlterDatabaseOperation::SetParam(config_param) => {
                let ConfigParam { param, value } = config_param;

                let database_param = match param.real_value().to_uppercase().as_str() {
                    "BARRIER_INTERVAL_MS" => {
                        let barrier_interval_ms = match value {
                            SetVariableValue::Default => None,
                            SetVariableValue::Single(SetVariableValueSingle::Literal(
                                Value::Number(num),
                            )) => {
                                let num = num.parse::<u32>().map_err(|e| {
                                    ErrorCode::InvalidInputSyntax(format!(
                                        "barrier_interval_ms must be a u32 integer: {}",
                                        e.as_report()
                                    ))
                                })?;
                                Some(num)
                            }
                            _ => {
                                return Err(ErrorCode::InvalidInputSyntax(
                                    "barrier_interval_ms must be a u32 integer or DEFAULT"
                                        .to_owned(),
                                )
                                .into());
                            }
                        };
                        AlterDatabaseParam::BarrierIntervalMs(barrier_interval_ms)
                    }
                    "CHECKPOINT_FREQUENCY" => {
                        let checkpoint_frequency = match value {
                            SetVariableValue::Default => None,
                            SetVariableValue::Single(SetVariableValueSingle::Literal(
                                Value::Number(num),
                            )) => {
                                let num = num.parse::<u64>().map_err(|e| {
                                    ErrorCode::InvalidInputSyntax(format!(
                                        "checkpoint_frequency must be a u64 integer: {}",
                                        e.as_report()
                                    ))
                                })?;
                                Some(num)
                            }
                            _ => {
                                return Err(ErrorCode::InvalidInputSyntax(
                                    "checkpoint_frequency must be a u64 integer or DEFAULT"
                                        .to_owned(),
                                )
                                .into());
                            }
                        };
                        AlterDatabaseParam::CheckpointFrequency(checkpoint_frequency)
                    }
                    _ => {
                        return Err(ErrorCode::InvalidInputSyntax(format!(
                            "Unsupported database config parameter: {}",
                            param.real_value()
                        ))
                        .into());
                    }
                };

                alter_database_param::handle_alter_database_param(
                    handler_args,
                    name,
                    database_param,
                )
                .await
            }
        },
        Statement::AlterSchema { name, operation } => match operation {
            AlterSchemaOperation::RenameSchema { schema_name } => {
                alter_rename::handle_rename_schema(handler_args, name, schema_name).await
            }
            AlterSchemaOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_SCHEMA,
                )
                .await
            }
            AlterSchemaOperation::SwapRenameSchema { target_schema } => {
                alter_swap_rename::handle_swap_rename(
                    handler_args,
                    name,
                    target_schema,
                    StatementType::ALTER_SCHEMA,
                )
                .await
            }
        },
        Statement::AlterTable { name, operation } => match operation {
            AlterTableOperation::AddColumn { .. }
            | AlterTableOperation::DropColumn { .. }
            | AlterTableOperation::AlterColumn { .. } => {
                alter_table_column::handle_alter_table_column(handler_args, name, operation).await
            }
            AlterTableOperation::RenameTable { table_name } => {
                alter_rename::handle_rename_table(handler_args, TableType::Table, name, table_name)
                    .await
            }
            AlterTableOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_TABLE,
                )
                .await
            }
            AlterTableOperation::SetParallelism {
                parallelism,
                deferred,
            } => {
                alter_parallelism::handle_alter_parallelism(
                    handler_args,
                    name,
                    parallelism,
                    StatementType::ALTER_TABLE,
                    deferred,
                )
                .await
            }
            AlterTableOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_TABLE,
                    None,
                )
                .await
            }
            AlterTableOperation::RefreshSchema => {
                alter_table_with_sr::handle_refresh_schema(handler_args, name).await
            }
            AlterTableOperation::SetSourceRateLimit { rate_limit } => {
                alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                    handler_args,
                    PbThrottleTarget::TableWithSource,
                    name,
                    rate_limit,
                )
                .await
            }
            AlterTableOperation::DropConnector => {
                alter_table_drop_connector::handle_alter_table_drop_connector(handler_args, name)
                    .await
            }
            AlterTableOperation::SetDmlRateLimit { rate_limit } => {
                alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                    handler_args,
                    PbThrottleTarget::TableDml,
                    name,
                    rate_limit,
                )
                .await
            }
            AlterTableOperation::SetBackfillRateLimit { rate_limit } => {
                alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                    handler_args,
                    PbThrottleTarget::CdcTable,
                    name,
                    rate_limit,
                )
                .await
            }
            AlterTableOperation::SwapRenameTable { target_table } => {
                alter_swap_rename::handle_swap_rename(
                    handler_args,
                    name,
                    target_table,
                    StatementType::ALTER_TABLE,
                )
                .await
            }
            AlterTableOperation::AlterConnectorProps { alter_props } => {
                // If exists a associated source, it should be of the same name.
                crate::handler::alter_source_props::handle_alter_table_connector_props(
                    handler_args,
                    name,
                    alter_props,
                )
                .await
            }
            AlterTableOperation::AddConstraint { .. }
            | AlterTableOperation::DropConstraint { .. }
            | AlterTableOperation::RenameColumn { .. }
            | AlterTableOperation::ChangeColumn { .. }
            | AlterTableOperation::RenameConstraint { .. } => {
                bail_not_implemented!(
                    "Unhandled statement: {}",
                    Statement::AlterTable { name, operation }
                )
            }
        },
        Statement::AlterIndex { name, operation } => match operation {
            AlterIndexOperation::RenameIndex { index_name } => {
                alter_rename::handle_rename_index(handler_args, name, index_name).await
            }
            AlterIndexOperation::SetParallelism {
                parallelism,
                deferred,
            } => {
                alter_parallelism::handle_alter_parallelism(
                    handler_args,
                    name,
                    parallelism,
                    StatementType::ALTER_INDEX,
                    deferred,
                )
                .await
            }
        },
        Statement::AlterView {
            materialized,
            name,
            operation,
        } => {
            let statement_type = if materialized {
                StatementType::ALTER_MATERIALIZED_VIEW
            } else {
                StatementType::ALTER_VIEW
            };
            match operation {
                AlterViewOperation::RenameView { view_name } => {
                    if materialized {
                        alter_rename::handle_rename_table(
                            handler_args,
                            TableType::MaterializedView,
                            name,
                            view_name,
                        )
                        .await
                    } else {
                        alter_rename::handle_rename_view(handler_args, name, view_name).await
                    }
                }
                AlterViewOperation::SetParallelism {
                    parallelism,
                    deferred,
                } => {
                    if !materialized {
                        bail_not_implemented!("ALTER VIEW SET PARALLELISM");
                    }
                    alter_parallelism::handle_alter_parallelism(
                        handler_args,
                        name,
                        parallelism,
                        statement_type,
                        deferred,
                    )
                    .await
                }
                AlterViewOperation::SetResourceGroup {
                    resource_group,
                    deferred,
                } => {
                    if !materialized {
                        bail_not_implemented!("ALTER VIEW SET RESOURCE GROUP");
                    }
                    alter_resource_group::handle_alter_resource_group(
                        handler_args,
                        name,
                        resource_group,
                        statement_type,
                        deferred,
                    )
                    .await
                }
                AlterViewOperation::ChangeOwner { new_owner_name } => {
                    alter_owner::handle_alter_owner(
                        handler_args,
                        name,
                        new_owner_name,
                        statement_type,
                    )
                    .await
                }
                AlterViewOperation::SetSchema { new_schema_name } => {
                    alter_set_schema::handle_alter_set_schema(
                        handler_args,
                        name,
                        new_schema_name,
                        statement_type,
                        None,
                    )
                    .await
                }
                AlterViewOperation::SetBackfillRateLimit { rate_limit } => {
                    if !materialized {
                        bail_not_implemented!("ALTER VIEW SET BACKFILL RATE LIMIT");
                    }
                    alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                        handler_args,
                        PbThrottleTarget::Mv,
                        name,
                        rate_limit,
                    )
                    .await
                }
                AlterViewOperation::SwapRenameView { target_view } => {
                    alter_swap_rename::handle_swap_rename(
                        handler_args,
                        name,
                        target_view,
                        statement_type,
                    )
                    .await
                }
                AlterViewOperation::SetStreamingEnableUnalignedJoin { enable } => {
                    if !materialized {
                        bail!(
                            "ALTER VIEW SET STREAMING_ENABLE_UNALIGNED_JOIN is not supported. Only supported for materialized views"
                        );
                    }
                    alter_streaming_enable_unaligned_join::handle_alter_streaming_enable_unaligned_join(handler_args, name, enable).await
                }
                AlterViewOperation::AsQuery { query } => {
                    if !materialized {
                        bail_not_implemented!("ALTER VIEW AS QUERY");
                    }
                    // `ALTER MATERIALIZED VIEW AS QUERY` is only available in development build now.
                    if !cfg!(debug_assertions) {
                        bail_not_implemented!("ALTER MATERIALIZED VIEW AS QUERY");
                    }
                    alter_mv::handle_alter_mv(handler_args, name, query).await
                }
            }
        }

        Statement::AlterSink { name, operation } => match operation {
            AlterSinkOperation::AlterConnectorProps {
                alter_props: changed_props,
            } => alter_sink_props::handle_alter_sink_props(handler_args, name, changed_props).await,
            AlterSinkOperation::RenameSink { sink_name } => {
                alter_rename::handle_rename_sink(handler_args, name, sink_name).await
            }
            AlterSinkOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_SINK,
                )
                .await
            }
            AlterSinkOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_SINK,
                    None,
                )
                .await
            }
            AlterSinkOperation::SetParallelism {
                parallelism,
                deferred,
            } => {
                alter_parallelism::handle_alter_parallelism(
                    handler_args,
                    name,
                    parallelism,
                    StatementType::ALTER_SINK,
                    deferred,
                )
                .await
            }
            AlterSinkOperation::SwapRenameSink { target_sink } => {
                alter_swap_rename::handle_swap_rename(
                    handler_args,
                    name,
                    target_sink,
                    StatementType::ALTER_SINK,
                )
                .await
            }
            AlterSinkOperation::SetSinkRateLimit { rate_limit } => {
                alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                    handler_args,
                    PbThrottleTarget::Sink,
                    name,
                    rate_limit,
                )
                .await
            }
            AlterSinkOperation::SetStreamingEnableUnalignedJoin { enable } => {
                alter_streaming_enable_unaligned_join::handle_alter_streaming_enable_unaligned_join(
                    handler_args,
                    name,
                    enable,
                )
                .await
            }
        },
        Statement::AlterSubscription { name, operation } => match operation {
            AlterSubscriptionOperation::RenameSubscription { subscription_name } => {
                alter_rename::handle_rename_subscription(handler_args, name, subscription_name)
                    .await
            }
            AlterSubscriptionOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_SUBSCRIPTION,
                )
                .await
            }
            AlterSubscriptionOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_SUBSCRIPTION,
                    None,
                )
                .await
            }
            AlterSubscriptionOperation::SwapRenameSubscription {
                target_subscription,
            } => {
                alter_swap_rename::handle_swap_rename(
                    handler_args,
                    name,
                    target_subscription,
                    StatementType::ALTER_SUBSCRIPTION,
                )
                .await
            }
        },
        Statement::AlterSource { name, operation } => match operation {
            AlterSourceOperation::AlterConnectorProps { alter_props } => {
                alter_source_props::handle_alter_source_connector_props(
                    handler_args,
                    name,
                    alter_props,
                )
                .await
            }
            AlterSourceOperation::RenameSource { source_name } => {
                alter_rename::handle_rename_source(handler_args, name, source_name).await
            }
            AlterSourceOperation::AddColumn { .. } => {
                alter_source_column::handle_alter_source_column(handler_args, name, operation).await
            }
            AlterSourceOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_SOURCE,
                )
                .await
            }
            AlterSourceOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_SOURCE,
                    None,
                )
                .await
            }
            AlterSourceOperation::FormatEncode { format_encode } => {
                alter_source_with_sr::handle_alter_source_with_sr(handler_args, name, format_encode)
                    .await
            }
            AlterSourceOperation::RefreshSchema => {
                alter_source_with_sr::handler_refresh_schema(handler_args, name).await
            }
            AlterSourceOperation::SetSourceRateLimit { rate_limit } => {
                alter_streaming_rate_limit::handle_alter_streaming_rate_limit(
                    handler_args,
                    PbThrottleTarget::Source,
                    name,
                    rate_limit,
                )
                .await
            }
            AlterSourceOperation::SwapRenameSource { target_source } => {
                alter_swap_rename::handle_swap_rename(
                    handler_args,
                    name,
                    target_source,
                    StatementType::ALTER_SOURCE,
                )
                .await
            }
            AlterSourceOperation::SetParallelism {
                parallelism,
                deferred,
            } => {
                alter_parallelism::handle_alter_parallelism(
                    handler_args,
                    name,
                    parallelism,
                    StatementType::ALTER_SOURCE,
                    deferred,
                )
                .await
            }
        },
        Statement::AlterFunction {
            name,
            args,
            operation,
        } => match operation {
            AlterFunctionOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_FUNCTION,
                    args,
                )
                .await
            }
        },
        Statement::AlterConnection { name, operation } => match operation {
            AlterConnectionOperation::SetSchema { new_schema_name } => {
                alter_set_schema::handle_alter_set_schema(
                    handler_args,
                    name,
                    new_schema_name,
                    StatementType::ALTER_CONNECTION,
                    None,
                )
                .await
            }
            AlterConnectionOperation::ChangeOwner { new_owner_name } => {
                alter_owner::handle_alter_owner(
                    handler_args,
                    name,
                    new_owner_name,
                    StatementType::ALTER_CONNECTION,
                )
                .await
            }
        },
        Statement::AlterSystem { param, value } => {
            alter_system::handle_alter_system(handler_args, param, value).await
        }
        Statement::AlterSecret {
            name,
            with_options,
            operation,
        } => alter_secret::handle_alter_secret(handler_args, name, with_options, operation).await,
        Statement::AlterFragment {
            fragment_id,
            operation: AlterFragmentOperation::AlterBackfillRateLimit { rate_limit },
        } => {
            alter_streaming_rate_limit::handle_alter_streaming_rate_limit_by_id(
                &handler_args.session,
                PbThrottleTarget::Fragment,
                fragment_id,
                rate_limit,
                StatementType::SET_VARIABLE,
            )
            .await
        }
        Statement::AlterDefaultPrivileges { .. } => {
            handle_privilege::handle_alter_default_privileges(handler_args, stmt).await
        }
        Statement::StartTransaction { modes } => {
            transaction::handle_begin(handler_args, START_TRANSACTION, modes).await
        }
        Statement::Begin { modes } => transaction::handle_begin(handler_args, BEGIN, modes).await,
        Statement::Commit { chain } => {
            transaction::handle_commit(handler_args, COMMIT, chain).await
        }
        Statement::Abort => transaction::handle_rollback(handler_args, ABORT, false).await,
        Statement::Rollback { chain } => {
            transaction::handle_rollback(handler_args, ROLLBACK, chain).await
        }
        Statement::SetTransaction {
            modes,
            snapshot,
            session,
        } => transaction::handle_set(handler_args, modes, snapshot, session).await,
        Statement::CancelJobs(jobs) => handle_cancel(handler_args, jobs).await,
        Statement::Kill(worker_process_id) => handle_kill(handler_args, worker_process_id).await,
        Statement::Comment {
            object_type,
            object_name,
            comment,
        } => comment::handle_comment(handler_args, object_type, object_name, comment).await,
        Statement::Use { db_name } => use_db::handle_use_db(handler_args, db_name),
        Statement::Prepare {
            name,
            data_types,
            statement,
        } => prepared_statement::handle_prepare(name, data_types, statement).await,
        Statement::Deallocate { name, prepare } => {
            prepared_statement::handle_deallocate(name, prepare).await
        }
        Statement::Vacuum { object_name } => vacuum::handle_vacuum(handler_args, object_name).await,
        _ => bail_not_implemented!("Unhandled statement: {}", stmt),
    }
}

fn check_ban_ddl_for_iceberg_engine_table(
    session: Arc<SessionImpl>,
    stmt: &Statement,
) -> Result<()> {
    match stmt {
        Statement::AlterTable {
            name,
            operation:
                operation @ (AlterTableOperation::AddColumn { .. }
                | AlterTableOperation::DropColumn { .. }),
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE {} is not supported for iceberg table: {}.{}",
                    operation,
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::RenameTable { .. },
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE RENAME is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::ChangeOwner { .. },
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE CHANGE OWNER is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::SetParallelism { .. },
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE SET PARALLELISM is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::SetSchema { .. },
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE SET SCHEMA is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::RefreshSchema,
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE REFRESH SCHEMA is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        Statement::AlterTable {
            name,
            operation: AlterTableOperation::SetSourceRateLimit { .. },
        } => {
            let (table, schema_name) = get_table_catalog_by_table_name(session.as_ref(), name)?;
            if table.is_iceberg_engine_table() {
                bail!(
                    "ALTER TABLE SET SOURCE RATE LIMIT is not supported for iceberg table: {}.{}",
                    schema_name,
                    name
                );
            }
        }

        _ => {}
    }

    Ok(())
}
