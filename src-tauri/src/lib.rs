pub mod cli;
mod integration;
mod model;
mod sources;
mod storage;
mod updater;

use std::{path::PathBuf, sync::Mutex, thread, time::Duration};

use chrono::Duration as ChronoDuration;
use model::{AppSnapshot, DecisionStatus, DocumentReference, MessageKind};
use sources::TupleClient;
use storage::{SessionRecord, Store};
use tauri::{Emitter, Manager, State};

struct RuntimeState {
    store: Result<Store, String>,
    update_guard: Mutex<()>,
}

impl RuntimeState {
    fn store(&self) -> Result<&Store, String> {
        self.store.as_ref().map_err(Clone::clone)
    }
}

#[tauri::command]
fn get_state(state: State<'_, RuntimeState>) -> Result<AppSnapshot, String> {
    state.store()?.snapshot()
}

#[tauri::command]
fn mark_read(
    through_id: Option<String>,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let _guard = state
        .update_guard
        .lock()
        .map_err(|_| "app state lock was poisoned".to_string())?;
    let store = state.store()?;
    let session = selected_session(store)?;
    store.mark_read_through(&session.id, through_id.as_deref())?;
    refresh(&app, store)
}

#[tauri::command]
fn review_decision(
    id: String,
    status: DecisionStatus,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let _guard = state
        .update_guard
        .lock()
        .map_err(|_| "app state lock was poisoned".to_string())?;
    let store = state.store()?;
    let session = selected_session(store)?;
    store.review_decision(&session.id, &id, status)?;
    refresh(&app, store)
}

#[tauri::command]
fn report_stale_reference(
    message_id: String,
    locator: DocumentReference,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let store = state.store()?;
    let session = selected_session(store)?;
    store.report_stale_reference(&session.id, &message_id, &locator)
}

#[tauri::command]
fn open_file_reference(
    path: String,
    line: Option<u32>,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let session = selected_session(state.store()?)?;
    let repo = session
        .repo
        .ok_or_else(|| "planning-scribe has not attached a repository".to_string())?;
    let relative = storage::parse_file_spec(&path)?.0;
    let absolute = repo.join(relative);
    let mut url = url::Url::parse("phpstorm://open")
        .map_err(|error| format!("cannot build PhpStorm URL: {error}"))?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("file", &absolute.to_string_lossy());
        if let Some(line) = line {
            query.append_pair("line", &line.to_string());
        }
    }
    tauri_plugin_opener::open_url(url.as_str(), None::<&str>)
        .map_err(|error| format!("cannot open file in PhpStorm: {error}"))
}

#[tauri::command]
fn select_session(
    id: String,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let store = state.store()?;
    store.select_session(&id)?;
    refresh(&app, store)
}

#[tauri::command]
fn delete_session(
    id: String,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let store = state.store()?;
    store.delete_session(&id)?;
    refresh(&app, store)
}

#[tauri::command]
fn select_chronicle(
    id: String,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let store = state.store()?;
    let session = selected_session(store)?;
    store.select_chronicle(&session.id, &id)?;
    sources::collect_chronicle(store, &session)?;
    refresh(&app, store)
}

#[tauri::command]
fn choose_chronicle_folder(
    path: String,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let store = state.store()?;
    let root = PathBuf::from(path);
    sources::validate_chronicle_root(&root)?;
    store.set_chronicle_root(&root)?;
    if let Some(session) = store.current_session()? {
        sources::discover_chronicle(store, &session)?;
        sources::collect_chronicle(store, &session)?;
    }
    refresh(&app, store)
}

#[tauri::command]
fn export_notes(
    destination: String,
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    let store = state.store()?;
    let session = selected_session(store)?;
    store.export_notes(&session.id, &PathBuf::from(destination))?;
    refresh(&app, store)
}

#[tauri::command]
fn install_claude_integration(
    state: State<'_, RuntimeState>,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let store = state.store()?;
    let skill = integration::install(store)?;
    refresh(&app, store)?;
    Ok(skill.to_string_lossy().into_owned())
}

fn selected_session(store: &Store) -> Result<SessionRecord, String> {
    store
        .selected_session()?
        .ok_or_else(|| "no Scribe session is selected".to_string())
}

fn refresh(app: &tauri::AppHandle, store: &Store) -> Result<(), String> {
    let snapshot = store.snapshot()?;
    set_badge(app, &snapshot);
    app.emit("state_changed", snapshot)
        .map_err(|error| format!("cannot update app window: {error}"))
}

fn set_badge(app: &tauri::AppHandle, snapshot: &AppSnapshot) {
    let unread = snapshot
        .messages
        .iter()
        .filter(|message| {
            !message.read && matches!(message.kind, MessageKind::Message | MessageKind::Decision)
        })
        .count() as i64;
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_badge_count((unread > 0).then_some(unread));
    }
}

fn start_collector(app: tauri::AppHandle, store: Store) {
    thread::spawn(move || {
        let tuple = TupleClient::discover();
        let mut previous = None;
        loop {
            let _ = sources::collect_once(&store, &tuple, "2s");
            if let Ok(snapshot) = store.snapshot() {
                let fingerprint = serde_json::to_string(&snapshot).ok();
                if fingerprint != previous {
                    set_badge(&app, &snapshot);
                    let _ = app.emit("state_changed", snapshot);
                    previous = fingerprint;
                }
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let store = Store::open();
    if let Ok(store) = &store {
        let _ = store.interrupt_stale_sessions(ChronoDuration::hours(12));
        let _ = store.prune();
        let _ = store.clear_terminal_selection_for_launch();
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(RuntimeState {
            store,
            update_guard: Mutex::new(()),
        })
        .manage(updater::UpdateManager::default())
        .invoke_handler(tauri::generate_handler![
            get_state,
            updater::get_update_state,
            updater::check_for_update,
            updater::install_update,
            mark_read,
            review_decision,
            report_stale_reference,
            open_file_reference,
            select_session,
            delete_session,
            select_chronicle,
            choose_chronicle_folder,
            export_notes,
            install_claude_integration,
        ])
        .setup(|app| {
            if let Ok(store) = app.state::<RuntimeState>().store.clone() {
                if let Ok(snapshot) = store.snapshot() {
                    set_badge(app.handle(), &snapshot);
                }
                start_collector(app.handle().clone(), store);
            }
            updater::start(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Scribe");
}
