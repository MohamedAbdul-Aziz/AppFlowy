use crate::{
    dart_notification::{send_dart_notification, FolderNotification},
    errors::FlowyError,
    manager::FolderManager,
    services::{get_current_workspace, read_local_workspace_apps, WorkspaceController},
};
use flowy_folder_data_model::entities::{
    app::RepeatedApp,
    view::View,
    workspace::{CurrentWorkspaceSetting, RepeatedWorkspace, WorkspaceId, *},
};
use lib_dispatch::prelude::{data_result, AppData, Data, DataResult};
use std::{convert::TryInto, sync::Arc};

#[tracing::instrument(level = "debug", skip(data, controller), err)]
pub(crate) async fn create_workspace_handler(
    data: Data<CreateWorkspacePayload>,
    controller: AppData<Arc<WorkspaceController>>,
) -> DataResult<Workspace, FlowyError> {
    let controller = controller.get_ref().clone();
    let params: CreateWorkspaceParams = data.into_inner().try_into()?;
    let workspace_rev = controller.create_workspace_from_params(params).await?;
    data_result(workspace_rev.into())
}

#[tracing::instrument(level = "debug", skip(controller), err)]
pub(crate) async fn read_workspace_apps_handler(
    controller: AppData<Arc<WorkspaceController>>,
) -> DataResult<RepeatedApp, FlowyError> {
    let items = controller
        .read_current_workspace_apps()
        .await?
        .into_iter()
        .map(|app_rev| app_rev.into())
        .collect();
    let repeated_app = RepeatedApp { items };
    data_result(repeated_app)
}

#[tracing::instrument(level = "debug", skip(data, controller), err)]
pub(crate) async fn open_workspace_handler(
    data: Data<WorkspaceId>,
    controller: AppData<Arc<WorkspaceController>>,
) -> DataResult<Workspace, FlowyError> {
    let params: WorkspaceId = data.into_inner();
    let workspaces = controller.open_workspace(params).await?;
    data_result(workspaces)
}

#[tracing::instrument(level = "debug", skip(data, folder), err)]
pub(crate) async fn read_workspaces_handler(
    data: Data<WorkspaceId>,
    folder: AppData<Arc<FolderManager>>,
) -> DataResult<RepeatedWorkspace, FlowyError> {
    let params: WorkspaceId = data.into_inner();
    let user_id = folder.user.user_id()?;
    let workspace_controller = folder.workspace_controller.clone();

    let trash_controller = folder.trash_controller.clone();
    let workspaces = folder
        .persistence
        .begin_transaction(|transaction| {
            let mut workspaces =
                workspace_controller.read_local_workspaces(params.value.clone(), &user_id, &transaction)?;
            for workspace in workspaces.iter_mut() {
                let apps = read_local_workspace_apps(&workspace.id, trash_controller.clone(), &transaction)?
                    .into_iter()
                    .map(|app_rev| app_rev.into())
                    .collect();
                workspace.apps.items = apps;
            }
            Ok(workspaces)
        })
        .await?;
    let _ = read_workspaces_on_server(folder, user_id, params);
    data_result(workspaces)
}

#[tracing::instrument(level = "debug", skip(folder), err)]
pub async fn read_cur_workspace_handler(
    folder: AppData<Arc<FolderManager>>,
) -> DataResult<CurrentWorkspaceSetting, FlowyError> {
    let workspace_id = get_current_workspace()?;
    let user_id = folder.user.user_id()?;
    let params = WorkspaceId {
        value: Some(workspace_id.clone()),
    };

    let workspace = folder
        .persistence
        .begin_transaction(|transaction| {
            folder
                .workspace_controller
                .read_local_workspace(workspace_id, &user_id, &transaction)
        })
        .await?;

    let latest_view: Option<View> = folder
        .view_controller
        .latest_visit_view()
        .await
        .unwrap_or(None)
        .map(|view_rev| view_rev.into());
    let setting = CurrentWorkspaceSetting { workspace, latest_view };
    let _ = read_workspaces_on_server(folder, user_id, params);
    data_result(setting)
}

#[tracing::instrument(level = "trace", skip(folder_manager), err)]
fn read_workspaces_on_server(
    folder_manager: AppData<Arc<FolderManager>>,
    user_id: String,
    params: WorkspaceId,
) -> Result<(), FlowyError> {
    let (token, server) = (folder_manager.user.token()?, folder_manager.cloud_service.clone());
    let persistence = folder_manager.persistence.clone();

    tokio::spawn(async move {
        let workspace_revs = server.read_workspace(&token, params).await?;
        let _ = persistence
            .begin_transaction(|transaction| {
                tracing::trace!("Save {} workspace", workspace_revs.len());
                for workspace_rev in &workspace_revs {
                    let m_workspace = workspace_rev.clone();
                    let app_revs = m_workspace.apps.clone();
                    let _ = transaction.create_workspace(&user_id, m_workspace)?;
                    tracing::trace!("Save {} apps", app_revs.len());
                    for app_rev in app_revs {
                        let view_revs = app_rev.belongings.clone();
                        match transaction.create_app(app_rev) {
                            Ok(_) => {}
                            Err(e) => log::error!("create app failed: {:?}", e),
                        }

                        tracing::trace!("Save {} views", view_revs.len());
                        for view_rev in view_revs {
                            match transaction.create_view(view_rev) {
                                Ok(_) => {}
                                Err(e) => log::error!("create view failed: {:?}", e),
                            }
                        }
                    }
                }
                Ok(())
            })
            .await?;

        let repeated_workspace = RepeatedWorkspace {
            items: workspace_revs
                .into_iter()
                .map(|workspace_rev| workspace_rev.into())
                .collect(),
        };

        send_dart_notification(&token, FolderNotification::WorkspaceListUpdated)
            .payload(repeated_workspace)
            .send();
        Result::<(), FlowyError>::Ok(())
    });

    Ok(())
}
