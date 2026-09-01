use std::sync::Mutex;

use serde::Serialize;
use tauri::{Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSnapshot {
    status: UpdateStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
enum UpdateStatus {
    Checking,
    UpToDate,
    Available,
    Installing,
    Restarting,
    Error,
}

impl UpdateSnapshot {
    fn checking() -> Self {
        Self {
            status: UpdateStatus::Checking,
            version: None,
            error: None,
        }
    }

    fn up_to_date() -> Self {
        Self {
            status: UpdateStatus::UpToDate,
            version: None,
            error: None,
        }
    }

    fn available(version: String) -> Self {
        Self {
            status: UpdateStatus::Available,
            version: Some(version),
            error: None,
        }
    }

    fn installing(version: String) -> Self {
        Self {
            status: UpdateStatus::Installing,
            version: Some(version),
            error: None,
        }
    }

    fn restarting(version: String) -> Self {
        Self {
            status: UpdateStatus::Restarting,
            version: Some(version),
            error: None,
        }
    }

    fn failed(error: String, version: Option<String>) -> Self {
        Self {
            status: UpdateStatus::Error,
            version,
            error: Some(error),
        }
    }
}

pub struct UpdateManager {
    snapshot: Mutex<UpdateSnapshot>,
}

impl Default for UpdateManager {
    fn default() -> Self {
        Self {
            snapshot: Mutex::new(UpdateSnapshot::up_to_date()),
        }
    }
}

impl UpdateManager {
    fn snapshot(&self) -> Result<UpdateSnapshot, String> {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| "update state lock was poisoned".to_string())
    }

    fn replace(&self, snapshot: UpdateSnapshot) -> Result<(), String> {
        *self
            .snapshot
            .lock()
            .map_err(|_| "update state lock was poisoned".to_string())? = snapshot;
        Ok(())
    }

    fn begin_check(&self) -> Result<bool, String> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| "update state lock was poisoned".to_string())?;
        if matches!(
            snapshot.status,
            UpdateStatus::Checking | UpdateStatus::Installing | UpdateStatus::Restarting
        ) {
            return Ok(false);
        }
        *snapshot = UpdateSnapshot::checking();
        Ok(true)
    }

    fn begin_install(&self) -> Result<Option<String>, String> {
        let mut snapshot = self
            .snapshot
            .lock()
            .map_err(|_| "update state lock was poisoned".to_string())?;
        let version = match snapshot.status {
            UpdateStatus::Available => snapshot.version.clone(),
            UpdateStatus::Error if snapshot.version.is_some() => snapshot.version.clone(),
            UpdateStatus::Installing | UpdateStatus::Restarting => return Ok(None),
            _ => return Err("no Scribe update is available to install".to_string()),
        };
        if let Some(version) = &version {
            *snapshot = UpdateSnapshot::installing(version.clone());
        }
        Ok(version)
    }
}

#[tauri::command]
pub fn get_update_state(manager: State<'_, UpdateManager>) -> Result<UpdateSnapshot, String> {
    manager.snapshot()
}

fn publish(
    app: &tauri::AppHandle,
    manager: &UpdateManager,
    snapshot: UpdateSnapshot,
) -> Result<(), String> {
    manager.replace(snapshot.clone())?;
    app.emit("update_state_changed", snapshot)
        .map_err(|error| format!("cannot update the updater control: {error}"))
}

async fn discover_update(app: &tauri::AppHandle, manager: &UpdateManager) -> Result<(), String> {
    if !manager.begin_check()? {
        return Ok(());
    }
    app.emit("update_state_changed", manager.snapshot()?)
        .map_err(|error| format!("cannot update the updater control: {error}"))?;

    let result = match app.updater() {
        Ok(updater) => updater.check().await,
        Err(error) => {
            let message = format!("Could not initialize updates: {error}");
            publish(app, manager, UpdateSnapshot::failed(message, None))?;
            return Ok(());
        }
    };
    match result {
        Ok(Some(update)) => publish(app, manager, UpdateSnapshot::available(update.version)),
        Ok(None) => publish(app, manager, UpdateSnapshot::up_to_date()),
        Err(error) => publish(
            app,
            manager,
            UpdateSnapshot::failed(format!("Could not check for updates: {error}"), None),
        ),
    }
}

#[tauri::command]
pub async fn check_for_update(
    app: tauri::AppHandle,
    manager: State<'_, UpdateManager>,
) -> Result<(), String> {
    discover_update(&app, &manager).await
}

#[tauri::command]
pub async fn install_update(
    app: tauri::AppHandle,
    manager: State<'_, UpdateManager>,
) -> Result<(), String> {
    let Some(expected_version) = manager.begin_install()? else {
        return Ok(());
    };
    app.emit("update_state_changed", manager.snapshot()?)
        .map_err(|error| format!("cannot update the updater control: {error}"))?;

    let update = match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(update)) => update,
            Ok(None) => {
                publish(&app, &manager, UpdateSnapshot::up_to_date())?;
                return Ok(());
            }
            Err(error) => {
                publish(
                    &app,
                    &manager,
                    UpdateSnapshot::failed(
                        format!("Could not refresh update {expected_version}: {error}"),
                        Some(expected_version),
                    ),
                )?;
                return Ok(());
            }
        },
        Err(error) => {
            publish(
                &app,
                &manager,
                UpdateSnapshot::failed(
                    format!("Could not initialize update {expected_version}: {error}"),
                    Some(expected_version),
                ),
            )?;
            return Ok(());
        }
    };
    let version = update.version.clone();
    publish(&app, &manager, UpdateSnapshot::installing(version.clone()))?;
    if let Err(error) = update.download_and_install(|_, _| {}, || {}).await {
        publish(
            &app,
            &manager,
            UpdateSnapshot::failed(
                format!("Could not install update {version}: {error}"),
                Some(version),
            ),
        )?;
        return Ok(());
    }
    publish(&app, &manager, UpdateSnapshot::restarting(version))?;
    #[cfg(not(target_os = "windows"))]
    {
        app.restart()
    }
    #[cfg(target_os = "windows")]
    {
        Ok(())
    }
}

pub fn start(app: tauri::AppHandle) {
    if cfg!(debug_assertions) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let manager = app.state::<UpdateManager>();
        if let Err(error) = discover_update(&app, &manager).await {
            eprintln!("cannot check for updates: {error}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_state_covers_discovery_outcomes() {
        assert_eq!(UpdateSnapshot::checking().status, UpdateStatus::Checking);
        assert_eq!(UpdateSnapshot::up_to_date().status, UpdateStatus::UpToDate);
        assert_eq!(
            UpdateSnapshot::available("1.2.3".to_string()).status,
            UpdateStatus::Available
        );
        let failed = UpdateSnapshot::failed("offline".to_string(), None);
        assert_eq!(failed.status, UpdateStatus::Error);
        assert_eq!(failed.error.as_deref(), Some("offline"));
    }

    #[test]
    fn only_available_or_failed_install_can_begin() {
        let manager = UpdateManager::default();
        assert!(manager.begin_install().is_err());

        manager
            .replace(UpdateSnapshot::available("1.2.3".to_string()))
            .unwrap();
        assert_eq!(manager.begin_install().unwrap().as_deref(), Some("1.2.3"));
        assert_eq!(manager.snapshot().unwrap().status, UpdateStatus::Installing);
        assert_eq!(manager.begin_install().unwrap(), None);

        manager
            .replace(UpdateSnapshot::failed(
                "download failed".to_string(),
                Some("1.2.3".to_string()),
            ))
            .unwrap();
        assert_eq!(manager.begin_install().unwrap().as_deref(), Some("1.2.3"));
    }

    #[test]
    fn checks_do_not_interrupt_installation() {
        let manager = UpdateManager::default();
        assert!(manager.begin_check().unwrap());
        assert!(!manager.begin_check().unwrap());
        manager
            .replace(UpdateSnapshot::installing("1.2.3".to_string()))
            .unwrap();
        assert!(!manager.begin_check().unwrap());
        manager
            .replace(UpdateSnapshot::restarting("1.2.3".to_string()))
            .unwrap();
        assert!(!manager.begin_check().unwrap());
    }
}
