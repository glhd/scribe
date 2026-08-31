pub mod cli;
mod model;
mod storage;

use std::{
    fs,
    path::Path,
    sync::Mutex,
    thread,
    time::{Duration, SystemTime},
};

use model::{AppSnapshot, DecisionStatus, DocumentReference, MessageKind};
use storage::SessionPaths;
use tauri::{Emitter, Manager, State};

struct RuntimeState {
    session: Result<SessionPaths, String>,
    update_guard: Mutex<()>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileFingerprint {
    exists: bool,
    modified: Option<SystemTime>,
    length: u64,
}

fn fingerprint(path: &Path) -> FileFingerprint {
    match fs::metadata(path) {
        Ok(metadata) => FileFingerprint {
            exists: true,
            modified: metadata.modified().ok(),
            length: metadata.len(),
        },
        Err(_) => FileFingerprint {
            exists: false,
            modified: None,
            length: 0,
        },
    }
}

#[tauri::command]
fn get_state(state: State<'_, RuntimeState>) -> Result<AppSnapshot, String> {
    storage::snapshot(state.session.as_ref().map_err(Clone::clone)?)
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
    let session = state.session.as_ref().map_err(Clone::clone)?;
    storage::mark_read_through(session, through_id.as_deref())?;
    refresh(&app, session)
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
    let session = state.session.as_ref().map_err(Clone::clone)?;
    storage::review_decision(session, &id, status)?;
    refresh(&app, session)
}

#[tauri::command]
fn report_stale_reference(
    message_id: String,
    locator: DocumentReference,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let session = state.session.as_ref().map_err(Clone::clone)?;
    storage::report_stale_reference(session, &message_id, &locator)
}

#[tauri::command]
fn open_file_reference(
    path: String,
    line: Option<u32>,
    state: State<'_, RuntimeState>,
) -> Result<(), String> {
    let session = state.session.as_ref().map_err(Clone::clone)?;
    let relative = storage::parse_file_spec(&path)?.0;
    let absolute = session.repo.join(relative);
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

fn refresh(app: &tauri::AppHandle, session: &SessionPaths) -> Result<(), String> {
    let snapshot = storage::snapshot(session)?;
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

fn start_watcher(app: tauri::AppHandle, session: SessionPaths) {
    thread::spawn(move || {
        let mut notes = fingerprint(&session.notes);
        let mut chat = fingerprint(&session.chat);
        loop {
            thread::sleep(Duration::from_millis(300));
            let next_notes = fingerprint(&session.notes);
            let next_chat = fingerprint(&session.chat);
            if notes != next_notes || chat != next_chat {
                notes = next_notes;
                chat = next_chat;
                // Atomic rewrites and rapid editor saves can produce two adjacent
                // changes. A short settle period prevents rendering the transient one.
                thread::sleep(Duration::from_millis(50));
                let _ = refresh(&app, &session);
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let session = storage::resolve_session();
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(RuntimeState {
            session,
            update_guard: Mutex::new(()),
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            mark_read,
            review_decision,
            report_stale_reference,
            open_file_reference
        ])
        .setup(|app| {
            if let Ok(session) = app.state::<RuntimeState>().session.clone() {
                if let Ok(snapshot) = storage::snapshot(&session) {
                    set_badge(app.handle(), &snapshot);
                }
                start_watcher(app.handle().clone(), session);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Scribe");
}
