//! Notices when a call app starts using the microphone (a Zoom, Teams or
//! Meet call) and offers to record it, so nobody has to remember to press
//! record. It only looks at which apps hold the microphone, never at what
//! they capture.
//!
//! macOS 14+ lists the audio clients and whether each is taking input
//! (Core Audio process objects, by bundle id). Windows keeps the apps using
//! the microphone in the CapabilityAccessManager consent store: an entry
//! whose last stop time is 0 is in use right now.

use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_store::StoreExt;

const POLL_EVERY: Duration = Duration::from_secs(3);
/// Polls in a row an app must hold the microphone before it counts as a
/// call: a voice memo or a quick dictation shouldn't trigger a prompt.
const POLLS_TO_START: u32 = 2;
/// Polls in a row without the microphone before a call counts as over.
const POLLS_TO_END: u32 = 3;

const STORE_FILE: &str = "call-detection.json";
const SETTINGS_KEY: &str = "settings";

/// A call app, matched by bundle id on macOS and executable name on Windows.
pub struct CallApp {
    pub key: &'static str,
    pub name: &'static str,
    /// macOS bundle ids; an entry ending in '.' matches any id with that prefix
    /// (browser helper processes carry their own ids).
    mac: &'static [&'static str],
    /// Windows executable names, lowercase.
    windows: &'static [&'static str],
}

pub const CALL_APPS: &[CallApp] = &[
    CallApp { key: "zoom", name: "Zoom", mac: &["us.zoom.xos", "us.zoom."], windows: &["zoom.exe"] },
    CallApp {
        key: "teams",
        name: "Microsoft Teams",
        mac: &["com.microsoft.teams2", "com.microsoft.teams"],
        windows: &["ms-teams.exe", "teams.exe"],
    },
    CallApp { key: "slack", name: "Slack", mac: &["com.tinyspeck.slackmacgap"], windows: &["slack.exe"] },
    CallApp { key: "discord", name: "Discord", mac: &["com.hnc.Discord"], windows: &["discord.exe"] },
    CallApp { key: "webex", name: "Webex", mac: &["Cisco-Systems.Spark", "com.webex."], windows: &["webexmta.exe", "ciscocollabhost.exe"] },
    CallApp { key: "facetime", name: "FaceTime", mac: &["com.apple.FaceTime"], windows: &[] },
    CallApp { key: "whatsapp", name: "WhatsApp", mac: &["net.whatsapp.WhatsApp", "desktop.WhatsApp"], windows: &["whatsapp.exe"] },
    CallApp {
        key: "browser",
        name: "A browser call (Meet or another web call)",
        mac: &[
            "com.google.Chrome",
            "com.google.Chrome.",
            "org.mozilla.firefox",
            "com.apple.Safari",
            "com.apple.WebKit.GPU",
            "com.microsoft.edgemac",
            "com.microsoft.edgemac.",
            "company.thebrowser.Browser",
            "com.brave.Browser",
            "com.brave.Browser.",
        ],
        windows: &["chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "arc.exe"],
    },
];

fn matches(patterns: &[&str], id: &str) -> bool {
    patterns.iter().any(|p| if p.ends_with('.') { id.starts_with(p) } else { id == *p })
}

/// The call app a microphone user belongs to, by its bundle id (macOS) or
/// executable name (Windows).
pub fn classify(id: &str) -> Option<&'static CallApp> {
    let lower = id.to_ascii_lowercase();
    CALL_APPS.iter().find(|app| {
        if cfg!(target_os = "windows") {
            app.windows.iter().any(|exe| lower.ends_with(exe))
        } else {
            matches(app.mac, id)
        }
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallDetectionSettings {
    pub enabled: bool,
    /// Start recording without asking (it still shows that it did).
    pub auto_start: bool,
    /// Keys of call apps not to watch.
    pub ignored_apps: Vec<String>,
}

impl Default for CallDetectionSettings {
    fn default() -> Self {
        // Asking is harmless, so it is on; recording without asking is opt-in.
        Self { enabled: true, auto_start: false, ignored_apps: Vec::new() }
    }
}

pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> CallDetectionSettings {
    app.store(STORE_FILE)
        .ok()
        .and_then(|store| store.get(SETTINGS_KEY))
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
pub struct CallApplication {
    pub key: String,
    pub name: String,
}

#[tauri::command]
pub fn get_call_detection_settings<R: Runtime>(app: AppHandle<R>) -> CallDetectionSettings {
    load_settings(&app)
}

#[tauri::command]
pub fn set_call_detection_settings<R: Runtime>(app: AppHandle<R>, settings: CallDetectionSettings) -> Result<(), String> {
    let store = app.store(STORE_FILE).map_err(|e| e.to_string())?;
    store.set(SETTINGS_KEY, serde_json::to_value(&settings).map_err(|e| e.to_string())?);
    store.save().map_err(|e| e.to_string())
}

/// The call apps the detector knows, for the settings list.
#[tauri::command]
pub fn list_call_apps() -> Vec<CallApplication> {
    CALL_APPS.iter().map(|a| CallApplication { key: a.key.into(), name: a.name.into() }).collect()
}

#[derive(Debug, Clone, Serialize)]
pub struct CallEvent {
    pub key: String,
    pub name: String,
    /// The app started recording by itself (auto_start).
    pub auto_started: bool,
}

/// Ids of the apps taking microphone input right now, other than this one.
#[cfg(target_os = "macos")]
fn microphone_users() -> Vec<String> {
    use cidre::core_audio as ca;
    let me = std::process::id() as i32;
    let Ok(processes) = ca::System::processes() else { return Vec::new() };
    processes
        .iter()
        .filter(|p| p.is_running_input().unwrap_or(false))
        .filter(|p| p.pid().map_or(true, |pid| pid != me))
        .filter_map(|p| p.bundle_id().ok().map(|id| id.to_string()))
        .collect()
}

#[cfg(target_os = "windows")]
fn microphone_users() -> Vec<String> {
    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;
    const STORE: &str = r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone";
    let me = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "#").to_ascii_lowercase());
    // FILETIME (100 ns since 1601) of 12 hours ago. Windows leaves the stop
    // time at 0 when an app crashes holding the microphone, so an entry that
    // "started" long ago is a leftover, not a call.
    const FILETIME_UNIX_OFFSET: u64 = 116_444_736_000_000_000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 / 100 + FILETIME_UNIX_OFFSET)
        .unwrap_or(0);
    let recent = now.saturating_sub(12 * 3600 * 10_000_000);
    let in_use = |key: &RegKey| key.get_value::<u64, _>("LastUsedTimeStop").map_or(false, |stop| stop == 0)
        && key.get_value::<u64, _>("LastUsedTimeStart").map_or(false, |start| start > recent);
    let mut users = Vec::new();
    let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(STORE) else { return users };
    // Store apps sit directly under the key; desktop apps under NonPackaged,
    // named by their path with '\' written as '#'.
    for name in root.enum_keys().flatten() {
        if name == "NonPackaged" {
            if let Ok(non_packaged) = root.open_subkey(&name) {
                for exe in non_packaged.enum_keys().flatten() {
                    let lower = exe.to_ascii_lowercase();
                    if Some(&lower) == me.as_ref() {
                        continue;
                    }
                    if non_packaged.open_subkey(&exe).is_ok_and(|k| in_use(&k)) {
                        users.push(lower.replace('#', "\\"));
                    }
                }
            }
        } else if root.open_subkey(&name).is_ok_and(|k| in_use(&k)) {
            // Packaged apps (new Teams: "MSTeams_8wekyb3d8bbwe") map by name.
            let lower = name.to_ascii_lowercase();
            if lower.starts_with("msteams") {
                users.push("ms-teams.exe".into());
            } else if lower.contains("whatsapp") {
                users.push("whatsapp.exe".into());
            }
        }
    }
    users
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn microphone_users() -> Vec<String> {
    Vec::new()
}

/// Turns each poll's call apps into start and end events, with the
/// debouncing above. Pure, so it can be tested without a microphone.
#[derive(Default)]
struct Tracker {
    seen: HashMap<&'static str, u32>,
    missing: HashMap<&'static str, u32>,
    active: HashSet<&'static str>,
}

impl Tracker {
    /// Returns (started, ended) app keys for this poll.
    fn poll(&mut self, present: &HashSet<&'static str>) -> (Vec<&'static str>, Vec<&'static str>) {
        let mut started = Vec::new();
        let mut ended = Vec::new();
        for &key in present {
            self.missing.remove(key);
            let count = self.seen.entry(key).or_insert(0);
            *count += 1;
            if *count >= POLLS_TO_START && self.active.insert(key) {
                started.push(key);
            }
        }
        let gone: Vec<&'static str> = self.seen.keys().copied().filter(|k| !present.contains(k)).collect();
        for key in gone {
            let misses = self.missing.entry(key).or_insert(0);
            *misses += 1;
            if *misses >= POLLS_TO_END {
                self.seen.remove(key);
                self.missing.remove(key);
                if self.active.remove(key) {
                    ended.push(key);
                }
            }
        }
        (started, ended)
    }
}

/// Through the notification manager, so the user's notification consent
/// and Do Not Disturb apply.
async fn notify<R: Runtime>(app: &AppHandle<R>, title: &str, body: &str) {
    use crate::notifications::commands::NotificationManagerState;
    use crate::notifications::types::{Notification, NotificationType};
    use tauri::Manager;
    let state = app.state::<NotificationManagerState<R>>();
    let manager = state.read().await;
    let Some(manager) = manager.as_ref() else { return };
    if let Err(e) = manager.show_notification(Notification::new(title, body, NotificationType::CallDetected)).await {
        warn!("Call detection: notification failed: {}", e);
    }
}

/// The call a running recording was started for (by auto-start or the
/// prompt's Record), so "stop recording?" is offered only for that one.
static RECORDING_FOR_CALL: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn recording_for_call() -> Option<String> {
    RECORDING_FOR_CALL.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn set_recording_for_call(key: Option<String>) {
    *RECORDING_FOR_CALL.lock().unwrap_or_else(|e| e.into_inner()) = key;
}

/// The prompt's Record: remembers which call the recording is for.
#[tauri::command]
pub fn mark_recording_for_call(key: String) {
    set_recording_for_call(Some(key));
}

/// The call-ended prompt's Stop: stops only a recording started for that
/// call, never one started by hand.
#[tauri::command]
pub async fn stop_recording_for_call<R: Runtime>(app: AppHandle<R>, key: String) -> bool {
    if recording_for_call().as_deref() != Some(key.as_str()) || !crate::is_recording().await {
        return false;
    }
    set_recording_for_call(None);
    crate::tray::stop_recording_from_detection(app).await;
    true
}

async fn onboarding_done<R: Runtime>(app: &AppHandle<R>) -> bool {
    crate::onboarding::load_onboarding_status(app).await.map(|s| s.completed).unwrap_or(false)
}

/// Starts the background watcher. It runs for the app's lifetime and reads
/// the settings on every poll, so changes apply without a restart.
pub fn start<R: Runtime>(app: AppHandle<R>) {
    if cfg!(not(any(target_os = "macos", target_os = "windows"))) {
        return;
    }
    tauri::async_runtime::spawn(async move {
        let mut tracker = Tracker::default();
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            let settings = load_settings(&app);
            if !settings.enabled {
                tracker = Tracker::default();
                continue;
            }
            let users = tokio::task::spawn_blocking(microphone_users).await.unwrap_or_default();
            // Every call app is tracked; the ignore list only silences
            // prompts, so unticking an app mid-call doesn't look like the
            // call ended.
            let present: HashSet<&'static str> = users.iter().filter_map(|id| classify(id)).map(|a| a.key).collect();
            let (started, ended) = tracker.poll(&present);
            let recording = crate::is_recording().await;
            if !recording {
                set_recording_for_call(None);
            }
            for key in started {
                let Some(call_app) = CALL_APPS.iter().find(|a| a.key == key) else { continue };
                if recording || settings.ignored_apps.iter().any(|k| k == key) || !onboarding_done(&app).await {
                    continue;
                }
                info!("Call detection: {} started using the microphone", call_app.name);
                let auto_started = settings.auto_start;
                if auto_started {
                    // The recording-started notification says it began.
                    set_recording_for_call(Some(key.to_string()));
                    crate::tray::start_recording_from_detection(&app);
                } else {
                    notify(&app, "Call detected", &format!("{} is using your microphone. Open Meeting Notes to record it.", call_app.name)).await;
                }
                let _ = app.emit("call-detected", CallEvent { key: key.into(), name: call_app.name.into(), auto_started });
            }
            for key in ended {
                let Some(call_app) = CALL_APPS.iter().find(|a| a.key == key) else { continue };
                info!("Call detection: {} stopped using the microphone", call_app.name);
                if recording && recording_for_call().as_deref() == Some(key) {
                    let _ = app.emit("call-ended", CallEvent { key: key.into(), name: call_app.name.into(), auto_started: false });
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(keys: &[&'static str]) -> HashSet<&'static str> {
        keys.iter().copied().collect()
    }

    #[test]
    fn a_call_starts_after_two_polls_and_ends_after_three_without() {
        let mut t = Tracker::default();
        assert_eq!(t.poll(&set(&["zoom"])), (vec![], vec![]));
        assert_eq!(t.poll(&set(&["zoom"])), (vec!["zoom"], vec![]));
        assert_eq!(t.poll(&set(&["zoom"])), (vec![], vec![]));
        assert_eq!(t.poll(&set(&[])), (vec![], vec![]));
        assert_eq!(t.poll(&set(&[])), (vec![], vec![]));
        assert_eq!(t.poll(&set(&[])), (vec![], vec!["zoom"]));
        // A brief gap inside a call doesn't end it.
        t.poll(&set(&["teams"]));
        t.poll(&set(&["teams"]));
        t.poll(&set(&[]));
        assert_eq!(t.poll(&set(&["teams"])), (vec![], vec![]));
    }

    #[test]
    fn a_short_mic_use_never_counts_as_a_call() {
        let mut t = Tracker::default();
        t.poll(&set(&["slack"]));
        for _ in 0..3 {
            assert_eq!(t.poll(&set(&[])), (vec![], vec![]));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn bundle_ids_map_to_call_apps() {
        assert_eq!(classify("us.zoom.xos").map(|a| a.key), Some("zoom"));
        assert_eq!(classify("com.google.Chrome.helper").map(|a| a.key), Some("browser"));
        assert_eq!(classify("com.microsoft.teams2").map(|a| a.key), Some("teams"));
        assert_eq!(classify("com.apple.VoiceMemos").map(|a| a.key), None);
        // "us.zoom." is a prefix; "us.zoomer" is not Zoom.
        assert_eq!(classify("us.zoomer.app").map(|a| a.key), None);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod live {
    /// Lists the apps taking microphone input right now:
    ///   cargo test --lib call_detect::live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn print_microphone_users() {
        println!("microphone users: {:?}", super::microphone_users());
    }
}
