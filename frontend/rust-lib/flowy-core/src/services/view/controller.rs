use bytes::Bytes;
use flowy_collaboration::entities::{
    doc::{DocumentDelta, DocumentId},
    revision::{RepeatedRevision, Revision},
};
use flowy_database::SqliteConnection;
use futures::{FutureExt, StreamExt};
use std::{collections::HashSet, sync::Arc};

use crate::{
    entities::{
        trash::{RepeatedTrashId, TrashType},
        view::{CreateViewParams, RepeatedView, UpdateViewParams, View, ViewId},
    },
    errors::{FlowyError, FlowyResult},
    module::{WorkspaceDatabase, WorkspaceUser},
    notify::{send_dart_notification, WorkspaceNotification},
    services::{
        server::Server,
        view::sql::{ViewTable, ViewTableChangeset, ViewTableSql},
        TrashController,
        TrashEvent,
    },
};
use flowy_core_data_model::entities::share::{ExportData, ExportParams};
use flowy_database::kv::KV;
use flowy_document::context::DocumentContext;
use lib_infra::uuid_string;

const LATEST_VIEW_ID: &str = "latest_view_id";

pub(crate) struct ViewController {
    user: Arc<dyn WorkspaceUser>,
    server: Server,
    database: Arc<dyn WorkspaceDatabase>,
    trash_controller: Arc<TrashController>,
    document_ctx: Arc<DocumentContext>,
}

impl ViewController {
    pub(crate) fn new(
        user: Arc<dyn WorkspaceUser>,
        database: Arc<dyn WorkspaceDatabase>,
        server: Server,
        trash_can: Arc<TrashController>,
        document_ctx: Arc<DocumentContext>,
    ) -> Self {
        Self {
            user,
            server,
            database,
            trash_controller: trash_can,
            document_ctx,
        }
    }

    pub(crate) fn init(&self) -> Result<(), FlowyError> {
        let _ = self.document_ctx.init()?;
        self.listen_trash_can_event();
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, params), fields(name = %params.name), err)]
    pub(crate) async fn create_view_from_params(&self, params: CreateViewParams) -> Result<View, FlowyError> {
        let delta_data = Bytes::from(params.view_data.clone());
        let user_id = self.user.user_id()?;
        let repeated_revision: RepeatedRevision =
            Revision::initial_revision(&user_id, &params.view_id, delta_data).into();
        let _ = self
            .document_ctx
            .controller
            .save_document(&params.view_id, repeated_revision)
            .await?;
        let view = self.create_view_on_server(params).await?;
        let _ = self.create_view_on_local(view.clone()).await?;

        Ok(view)
    }

    pub(crate) async fn create_view_on_local(&self, view: View) -> Result<(), FlowyError> {
        let conn = &*self.database.db_connection()?;
        let trash_can = self.trash_controller.clone();

        conn.immediate_transaction::<_, FlowyError, _>(|| {
            let belong_to_id = view.belong_to_id.clone();
            let _ = self.save_view(view, conn)?;
            let _ = notify_views_changed(&belong_to_id, trash_can, &conn)?;

            Ok(())
        })?;

        Ok(())
    }

    pub(crate) fn save_view(&self, view: View, conn: &SqliteConnection) -> Result<(), FlowyError> {
        let view_table = ViewTable::new(view);
        let _ = ViewTableSql::create_view(view_table, conn)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, params), fields(view_id = %params.view_id), err)]
    pub(crate) async fn read_view(&self, params: ViewId) -> Result<View, FlowyError> {
        let conn = self.database.db_connection()?;
        let view_table = ViewTableSql::read_view(&params.view_id, &*conn)?;

        let trash_ids = self.trash_controller.read_trash_ids(&conn)?;
        if trash_ids.contains(&view_table.id) {
            return Err(FlowyError::record_not_found());
        }

        let view: View = view_table.into();
        let _ = self.read_view_on_server(params);
        Ok(view)
    }

    pub(crate) fn read_view_tables(&self, ids: Vec<String>) -> Result<Vec<ViewTable>, FlowyError> {
        let conn = &*self.database.db_connection()?;
        let mut view_tables = vec![];
        conn.immediate_transaction::<_, FlowyError, _>(|| {
            for view_id in ids {
                view_tables.push(ViewTableSql::read_view(&view_id, conn)?);
            }
            Ok(())
        })?;

        Ok(view_tables)
    }

    #[tracing::instrument(level = "debug", skip(self, params), fields(doc_id = %params.doc_id), err)]
    pub(crate) async fn open_view(&self, params: DocumentId) -> Result<DocumentDelta, FlowyError> {
        let doc_id = params.doc_id.clone();
        let editor = self.document_ctx.controller.open(&params.doc_id).await?;

        KV::set_str(LATEST_VIEW_ID, doc_id.clone());
        let document_json = editor.document_json().await?;
        Ok(DocumentDelta {
            doc_id,
            delta_json: document_json,
        })
    }

    #[tracing::instrument(level = "debug", skip(self,params), fields(doc_id = %params.doc_id), err)]
    pub(crate) async fn close_view(&self, params: DocumentId) -> Result<(), FlowyError> {
        let _ = self.document_ctx.controller.close(&params.doc_id)?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self,params), fields(doc_id = %params.doc_id), err)]
    pub(crate) async fn delete_view(&self, params: DocumentId) -> Result<(), FlowyError> {
        if let Some(view_id) = KV::get_str(LATEST_VIEW_ID) {
            if view_id == params.doc_id {
                let _ = KV::remove(LATEST_VIEW_ID);
            }
        }
        let _ = self.document_ctx.controller.close(&params.doc_id)?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, params), fields(doc_id = %params.doc_id), err)]
    pub(crate) async fn duplicate_view(&self, params: DocumentId) -> Result<(), FlowyError> {
        let view: View = ViewTableSql::read_view(&params.doc_id, &*self.database.db_connection()?)?.into();
        let editor = self.document_ctx.controller.open(&params.doc_id).await?;
        let document_json = editor.document_json().await?;
        let duplicate_params = CreateViewParams {
            belong_to_id: view.belong_to_id.clone(),
            name: format!("{} (copy)", &view.name),
            desc: view.desc.clone(),
            thumbnail: "".to_owned(),
            view_type: view.view_type.clone(),
            view_data: document_json,
            view_id: uuid_string(),
        };

        let _ = self.create_view_from_params(duplicate_params).await?;
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip(self, params), err)]
    pub(crate) async fn export_doc(&self, params: ExportParams) -> Result<ExportData, FlowyError> {
        let editor = self.document_ctx.controller.open(&params.doc_id).await?;
        let delta_json = editor.document_json().await?;
        Ok(ExportData {
            data: delta_json,
            export_type: params.export_type,
        })
    }

    // belong_to_id will be the app_id or view_id.
    #[tracing::instrument(level = "debug", skip(self), err)]
    pub(crate) async fn read_views_belong_to(&self, belong_to_id: &str) -> Result<RepeatedView, FlowyError> {
        // TODO: read from server
        let conn = self.database.db_connection()?;
        let repeated_view = read_belonging_views_on_local(belong_to_id, self.trash_controller.clone(), &conn)?;
        Ok(repeated_view)
    }

    #[tracing::instrument(level = "debug", skip(self, params), err)]
    pub(crate) async fn update_view(&self, params: UpdateViewParams) -> Result<View, FlowyError> {
        let conn = &*self.database.db_connection()?;
        let changeset = ViewTableChangeset::new(params.clone());
        let view_id = changeset.id.clone();

        let updated_view = conn.immediate_transaction::<_, FlowyError, _>(|| {
            let _ = ViewTableSql::update_view(changeset, conn)?;
            let view: View = ViewTableSql::read_view(&view_id, conn)?.into();
            Ok(view)
        })?;
        send_dart_notification(&view_id, WorkspaceNotification::ViewUpdated)
            .payload(updated_view.clone())
            .send();

        //
        let _ = notify_views_changed(&updated_view.belong_to_id, self.trash_controller.clone(), conn)?;
        let _ = self.update_view_on_server(params);
        Ok(updated_view)
    }

    pub(crate) async fn receive_document_delta(&self, params: DocumentDelta) -> Result<DocumentDelta, FlowyError> {
        let doc = self.document_ctx.controller.apply_document_delta(params).await?;
        Ok(doc)
    }

    pub(crate) fn latest_visit_view(&self) -> FlowyResult<Option<View>> {
        match KV::get_str(LATEST_VIEW_ID) {
            None => Ok(None),
            Some(view_id) => {
                let conn = self.database.db_connection()?;
                let view_table = ViewTableSql::read_view(&view_id, &*conn)?;
                Ok(Some(view_table.into()))
            },
        }
    }

    pub(crate) fn set_latest_view(&self, view: &View) { KV::set_str(LATEST_VIEW_ID, view.id.clone()); }
}

impl ViewController {
    #[tracing::instrument(skip(self), err)]
    async fn create_view_on_server(&self, params: CreateViewParams) -> Result<View, FlowyError> {
        let token = self.user.token()?;
        let view = self.server.create_view(&token, params).await?;
        Ok(view)
    }

    #[tracing::instrument(skip(self), err)]
    fn update_view_on_server(&self, params: UpdateViewParams) -> Result<(), FlowyError> {
        let token = self.user.token()?;
        let server = self.server.clone();
        tokio::spawn(async move {
            match server.update_view(&token, params).await {
                Ok(_) => {},
                Err(e) => {
                    // TODO: retry?
                    log::error!("Update view failed: {:?}", e);
                },
            }
        });
        Ok(())
    }

    #[tracing::instrument(skip(self), err)]
    fn read_view_on_server(&self, params: ViewId) -> Result<(), FlowyError> {
        let token = self.user.token()?;
        let server = self.server.clone();
        let pool = self.database.db_pool()?;
        // TODO: Retry with RetryAction?
        tokio::spawn(async move {
            match server.read_view(&token, params).await {
                Ok(Some(view)) => match pool.get() {
                    Ok(conn) => {
                        let view_table = ViewTable::new(view.clone());
                        let result = ViewTableSql::create_view(view_table, &conn);
                        match result {
                            Ok(_) => {
                                send_dart_notification(&view.id, WorkspaceNotification::ViewUpdated)
                                    .payload(view.clone())
                                    .send();
                            },
                            Err(e) => log::error!("Save view failed: {:?}", e),
                        }
                    },
                    Err(e) => log::error!("Require db connection failed: {:?}", e),
                },
                Ok(None) => {},
                Err(e) => log::error!("Read view failed: {:?}", e),
            }
        });
        Ok(())
    }

    fn listen_trash_can_event(&self) {
        let mut rx = self.trash_controller.subscribe();
        let database = self.database.clone();
        let document = self.document_ctx.clone();
        let trash_can = self.trash_controller.clone();
        let _ = tokio::spawn(async move {
            loop {
                let mut stream = Box::pin(rx.recv().into_stream().filter_map(|result| async move {
                    match result {
                        Ok(event) => event.select(TrashType::View),
                        Err(_e) => None,
                    }
                }));

                if let Some(event) = stream.next().await {
                    handle_trash_event(database.clone(), document.clone(), trash_can.clone(), event).await
                }
            }
        });
    }
}

#[tracing::instrument(level = "trace", skip(database, context, trash_can))]
async fn handle_trash_event(
    database: Arc<dyn WorkspaceDatabase>,
    context: Arc<DocumentContext>,
    trash_can: Arc<TrashController>,
    event: TrashEvent,
) {
    let db_result = database.db_connection();

    match event {
        TrashEvent::NewTrash(identifiers, ret) => {
            let result = || {
                let conn = &*db_result?;
                let view_tables = read_view_tables(identifiers, conn)?;
                for view_table in view_tables {
                    let _ = notify_views_changed(&view_table.belong_to_id, trash_can.clone(), conn)?;
                    notify_dart(view_table, WorkspaceNotification::ViewDeleted);
                }
                Ok::<(), FlowyError>(())
            };
            let _ = ret.send(result()).await;
        },
        TrashEvent::Putback(identifiers, ret) => {
            let result = || {
                let conn = &*db_result?;
                let view_tables = read_view_tables(identifiers, conn)?;
                for view_table in view_tables {
                    let _ = notify_views_changed(&view_table.belong_to_id, trash_can.clone(), conn)?;
                    notify_dart(view_table, WorkspaceNotification::ViewRestored);
                }
                Ok::<(), FlowyError>(())
            };
            let _ = ret.send(result()).await;
        },
        TrashEvent::Delete(identifiers, ret) => {
            let result = || {
                let conn = &*db_result?;
                let _ = conn.immediate_transaction::<_, FlowyError, _>(|| {
                    let mut notify_ids = HashSet::new();
                    for identifier in identifiers.items {
                        let view_table = ViewTableSql::read_view(&identifier.id, conn)?;
                        let _ = ViewTableSql::delete_view(&identifier.id, conn)?;
                        let _ = context.controller.delete(&identifier.id)?;
                        notify_ids.insert(view_table.belong_to_id);
                    }

                    for notify_id in notify_ids {
                        let _ = notify_views_changed(&notify_id, trash_can.clone(), conn)?;
                    }

                    Ok(())
                })?;
                Ok::<(), FlowyError>(())
            };
            let _ = ret.send(result()).await;
        },
    }
}

fn read_view_tables(identifiers: RepeatedTrashId, conn: &SqliteConnection) -> Result<Vec<ViewTable>, FlowyError> {
    let mut view_tables = vec![];
    let _ = conn.immediate_transaction::<_, FlowyError, _>(|| {
        for identifier in identifiers.items {
            let view_table = ViewTableSql::read_view(&identifier.id, conn)?;
            view_tables.push(view_table);
        }
        Ok(())
    })?;
    Ok(view_tables)
}

fn notify_dart(view_table: ViewTable, notification: WorkspaceNotification) {
    let view: View = view_table.into();
    send_dart_notification(&view.id, notification).payload(view).send();
}

#[tracing::instrument(skip(belong_to_id, trash_controller, conn), fields(view_count), err)]
fn notify_views_changed(
    belong_to_id: &str,
    trash_controller: Arc<TrashController>,
    conn: &SqliteConnection,
) -> FlowyResult<()> {
    let repeated_view = read_belonging_views_on_local(belong_to_id, trash_controller.clone(), conn)?;
    tracing::Span::current().record("view_count", &format!("{}", repeated_view.len()).as_str());
    send_dart_notification(&belong_to_id, WorkspaceNotification::AppViewsChanged)
        .payload(repeated_view)
        .send();
    Ok(())
}

fn read_belonging_views_on_local(
    belong_to_id: &str,
    trash_controller: Arc<TrashController>,
    conn: &SqliteConnection,
) -> FlowyResult<RepeatedView> {
    let mut view_tables = ViewTableSql::read_views(belong_to_id, conn)?;
    let trash_ids = trash_controller.read_trash_ids(conn)?;
    view_tables.retain(|view_table| !trash_ids.contains(&view_table.id));

    let views = view_tables
        .into_iter()
        .map(|view_table| view_table.into())
        .collect::<Vec<View>>();

    Ok(RepeatedView { items: views })
}
