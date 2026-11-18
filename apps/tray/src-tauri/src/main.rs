//! `fleet-tray`: a tray icon and a small window for mapping fleet projects to local folders on a
//! node, instead of running `fleet project map` by hand.
//!
//! The node launches it with `--data-dir` and `--node-pid`; it lives in the tray until that
//! node stops. Started by hand, it opens its window at once and runs until "Quit tray".

// Without this a console window opens next to the tray on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod node_watch;
mod process;
mod state;

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};

use clap::{Parser, error::ErrorKind};
use node_watch::NodePid;
use tauri::{
    AppHandle, Manager, State, WindowEvent,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};
use tauri_plugin_opener::OpenerExt;

const WINDOW: &str = "main";

#[derive(Parser)]
#[command(
    name = "fleet-tray",
    version,
    about = "Manage a fleet node's local projects"
)]
struct Cli {
    /// The node's data directory (holds node.json). Same default as the `fleet` CLI.
    #[arg(long, env = "FLEET_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Exit once this process (the node) has stopped.
    #[arg(long)]
    node_pid: Option<u32>,
}

/// The data directory being served. One tray serves one data directory at a time; a later
/// launch can switch it (see the single-instance hook in `main`).
pub struct DataDir(Mutex<PathBuf>);

impl DataDir {
    pub fn get(&self) -> PathBuf {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set(&self, dir: PathBuf) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = dir;
    }
}

/// Runs file work off the main thread, which also drives the window and the tray.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, String> {
    match tauri::async_runtime::spawn_blocking(work).await {
        // Plain strings, so the window can show the whole error chain inline.
        Ok(result) => result.map_err(|e| format!("{e:#}")),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
async fn get_state(dir: State<'_, DataDir>) -> Result<state::TrayState, String> {
    let dir = dir.get();
    blocking(move || state::get_state(&dir)).await
}

#[tauri::command]
async fn map_project(dir: State<'_, DataDir>, slug: String, path: String) -> Result<(), String> {
    let dir = dir.get();
    blocking(move || state::map_project(&dir, &slug, Path::new(&path))).await
}

#[tauri::command]
async fn unmap_project(dir: State<'_, DataDir>, slug: String) -> Result<(), String> {
    let dir = dir.get();
    blocking(move || state::unmap_project(&dir, &slug)).await
}

#[tauri::command]
async fn open_path(app: AppHandle, dir: State<'_, DataDir>, path: String) -> Result<(), String> {
    let dir = dir.get();
    blocking(move || {
        let path = state::mapped_dir(&dir, &path)?;
        app.opener()
            .open_path(path.display().to_string(), None::<&str>)?;
        Ok(())
    })
    .await
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.as_ref().show();
        let _ = window.set_focus();
    }
}

/// Hiding the window alone leaves WebView2 thinking the page is on screen
/// (`document.visibilityState` stays "visible", measured), so the page would keep polling.
/// Hiding the webview too marks the page hidden and lets WebView2 throttle it.
fn hide_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(WINDOW) {
        let _ = window.as_ref().hide();
        let _ = window.hide();
    }
}

fn toggle_window(app: &AppHandle) {
    // A double click also delivers two single clicks; without this the window would flash
    // open and shut before the double click shows it.
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
            return;
        }
        *last = Some(Instant::now());
    }
    let Some(window) = app.get_webview_window(WINDOW) else {
        return;
    };
    // Clicking the tray takes focus from the window, so visibility (not focus) decides.
    if window.is_visible().unwrap_or(false) && !window.is_minimized().unwrap_or(false) {
        hide_window(app);
    } else {
        show_window(app);
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit tray", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &quit])?;
    let mut tray = TrayIconBuilder::with_id("fleet")
        .tooltip("fleet projects")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } => toggle_window(tray.app_handle()),
            TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => show_window(tray.app_handle()),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// A later launch of the tray, forwarded by the single-instance plugin.
fn relaunched(app: &AppHandle, node: &NodePid, args: Vec<String>, cwd: &str) {
    let Ok(cli) = Cli::try_parse_from(args) else {
        show_window(app);
        return;
    };
    // Only argv and the working directory are forwarded, not the environment, so a launch
    // without `--data-dir` resolves the default in this process's environment.
    let dir = resolve_data_dir(cli.data_dir, Path::new(cwd));
    let data_dir = app.state::<DataDir>();
    let current = data_dir.get();
    match cli.node_pid {
        // A node knows which data directory it uses, so its launch always wins.
        Some(pid) => {
            if dir != current {
                let _ = state::log(&dir, &format!("node {pid} switched the tray to this dir"));
                data_dir.set(dir);
            }
            node.set(pid);
        }
        None => {
            if dir != current {
                if node.is_set() {
                    let _ = state::log(
                        &current,
                        &format!(
                            "kept serving this dir for the running node; ignored a launch for {}",
                            dir.display()
                        ),
                    );
                } else {
                    data_dir.set(dir);
                }
            }
            show_window(app);
        }
    }
}

fn resolve_data_dir(arg: Option<PathBuf>, cwd: &Path) -> PathBuf {
    // `join` keeps an absolute argument as is and anchors a relative one at the launch's cwd.
    arg.map(|d| cwd.join(d)).unwrap_or_else(default_data_dir)
}

/// Mirrors `default_data_dir` in `crates/fleet/src/main.rs`, which is private to that binary.
/// Keep the two in step: the tray must edit the same node.json the node reads.
fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(|p| PathBuf::from(p).join("fleet"))
            .unwrap_or_else(|| PathBuf::from("fleet-data"))
    } else {
        std::env::var_os("HOME")
            .map(|p| PathBuf::from(p).join(".local/share/fleet"))
            .unwrap_or_else(|| PathBuf::from("fleet-data"))
    }
}

/// Parses the command line. There is no console on Windows, so a bad command line is written to
/// the log instead, and the tray exits non-zero.
fn parse_cli() -> Cli {
    match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) => {
            e.exit()
        }
        Err(e) => {
            let msg = format!("bad command line: {e}");
            let dir = std::env::var_os("FLEET_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(default_data_dir);
            if state::log(&dir, &msg).is_err() {
                let _ = state::log(&std::env::temp_dir().join("fleet-tray"), &msg);
            }
            eprintln!("{e}");
            std::process::exit(2);
        }
    }
}

fn main() {
    let cli = parse_cli();
    let cwd = std::env::current_dir().unwrap_or_default();
    let data_dir = resolve_data_dir(cli.data_dir, &cwd);
    let node = NodePid::default();
    if let Some(pid) = cli.node_pid {
        node.set(pid);
    }
    let relaunch_node = node.clone();

    tauri::Builder::default()
        // Registered first, as the plugin requires, so a second launch exits before doing work.
        .plugin(tauri_plugin_single_instance::init(move |app, args, cwd| {
            relaunched(app, &relaunch_node, args, &cwd)
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(DataDir(Mutex::new(data_dir)))
        .invoke_handler(tauri::generate_handler![
            get_state,
            map_project,
            unmap_project,
            open_path
        ])
        .on_window_event(|window, event| {
            // Closing the window only hides it; the tray menu's "Quit tray" ends the app.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide_window(window.app_handle());
            }
        })
        .setup(move |app| {
            build_tray(app.handle())?;
            if cli.node_pid.is_none() {
                show_window(app.handle());
            }
            node_watch::spawn(app.handle().clone(), node);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("fleet-tray failed to start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_data_dir_is_anchored_at_the_launch_cwd() {
        let cwd = std::env::temp_dir();
        assert_eq!(
            resolve_data_dir(Some("d".into()), &cwd),
            cwd.join("d"),
            "relative"
        );
        let abs = cwd.join("abs");
        assert_eq!(
            resolve_data_dir(Some(abs.clone()), Path::new("elsewhere")),
            abs
        );
    }
}
