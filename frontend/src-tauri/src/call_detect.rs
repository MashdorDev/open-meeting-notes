//! Notices when a call app starts using the microphone (a Zoom, Teams or
//! Meet call) and offers to record it, so nobody has to remember to press
//! record. It only looks at which apps hold the microphone, never at what
//! they capture.
//!
//! macOS 14+ lists the audio clients and whether each is taking input
//! (Core Audio process objects, by bundle id). On Windows, the audio
//! session list says which executables are capturing from a microphone.

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
        windows: &["ms-teams.exe", "msteams.exe", "teams.exe"],
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

/// The executables with an active capture session on any microphone,
/// virtual ones included (NVIDIA Broadcast, Krisp): the live audio session
/// list Windows itself uses for its "in use" icon. (The
/// CapabilityAccessManager registry history stopped updating on newer
/// Windows 11 builds, so a call there went unnoticed.)
#[cfg(target_os = "windows")]
fn microphone_users() -> Vec<String> {
    use windows::core::Interface;
    use windows::Win32::Media::Audio::{
        eCapture, AudioSessionStateActive, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator,
        MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

    let me = std::process::id();
    let mut pids: Vec<u32> = Vec::new();
    // SAFETY: plain COM calls on this thread; every interface is released
    // when it goes out of scope.
    unsafe {
        // Already initialized on this pool thread is fine (S_FALSE, or a
        // different apartment mode we can still call into).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(enumerator) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let Ok(devices) = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        for i in 0..devices.GetCount().unwrap_or(0) {
            let Ok(device) = devices.Item(i) else { continue };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(sessions) = manager.GetSessionEnumerator() else { continue };
            for j in 0..sessions.GetCount().unwrap_or(0) {
                let Ok(control) = sessions.GetSession(j) else { continue };
                let Ok(control) = control.cast::<IAudioSessionControl2>() else { continue };
                if control.GetState().map_or(true, |state| state != AudioSessionStateActive) {
                    continue;
                }
                if let Ok(pid) = control.GetProcessId() {
                    if pid != 0 && pid != me && !pids.contains(&pid) {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    let processes = windows_processes();
    pids.iter()
        .filter_map(|pid| {
            let (exe, parent) = processes.get(pid)?;
            // New Teams and other WebView2 apps capture in a WebView2 helper:
            // name the app that owns the helper.
            if exe == "msedgewebview2.exe" {
                let mut owner = *parent;
                for _ in 0..4 {
                    let (parent_exe, grand) = processes.get(&owner)?;
                    if parent_exe != "msedgewebview2.exe" {
                        return Some(parent_exe.clone());
                    }
                    owner = *grand;
                }
                return None;
            }
            Some(exe.clone())
        })
        .collect()
}

/// Every process's executable name (lowercase) and parent id, by id.
#[cfg(target_os = "windows")]
fn windows_processes() -> HashMap<u32, (String, u32)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };
    let mut map = HashMap::new();
    // SAFETY: the snapshot handle is closed below; entries are plain data.
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return map };
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(entry.szExeFile.len());
            let exe = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
            map.insert(entry.th32ProcessID, (exe, entry.th32ParentProcessID));
            ok = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
    }
    map
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

/// Which recording belongs to which call. Record (or auto-start) leaves a
/// pending mark; the next recording to start within a short wait claims it
/// by its session number. Stop acts only on that exact recording, so a
/// recording the user started by hand, even right after, is never ended.
#[derive(Debug, Clone, PartialEq)]
enum CallMark {
    Pending { key: String, at: std::time::Instant, after_session: u64 },
    Bound { key: String, session: u64 },
}

static CALL_MARK: std::sync::Mutex<Option<CallMark>> = std::sync::Mutex::new(None);

/// How long a pending mark waits for its recording: Record reloads the page
/// and the recording starts a few seconds later. A start after this is
/// someone pressing record by hand.
const MARK_WAITS_FOR_START: Duration = Duration::from_secs(20);

fn current_session() -> u64 {
    crate::audio::recording_commands::recording_session()
}

fn mark_pending(key: String) {
    info!("Call detection: the next recording is for the {} call", key);
    *CALL_MARK.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(CallMark::Pending { key, at: std::time::Instant::now(), after_session: current_session() });
}

/// Binds a pending mark to the recording that started for it, drops one
/// that waited too long, and returns the call the given recording is for.
fn mark_for_session(session: u64, started: Option<std::time::Instant>) -> Option<String> {
    let mut mark = CALL_MARK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(CallMark::Pending { key, at, after_session }) = mark.clone() {
        if session > after_session {
            // The first recording after the mark claims it, if it came in time.
            // Judged by when the recording started, not when this poll ran.
            // Only the first recording after the mark can claim it; when more
            // than one started between polls the start time is the latest's,
            // which is no earlier than the first's, so a late first start
            // still can't pass.
            let in_time = started.is_some_and(|t| t.saturating_duration_since(at) <= MARK_WAITS_FOR_START);
            if in_time {
                info!("Call detection: recording {} is the {} call's", after_session + 1, key);
            } else {
                info!("Call detection: no recording started for the {} call in time", key);
            }
            *mark = in_time.then(|| CallMark::Bound { key, session: after_session + 1 });
        } else if at.elapsed() > MARK_WAITS_FOR_START + POLL_EVERY * 2 {
            // Nothing started; give a start near the end of the wait time to
            // be seen by a poll before dropping the mark.
            *mark = None;
        }
    }
    match mark.as_ref() {
        Some(CallMark::Bound { key, session: bound }) if *bound == session => Some(key.clone()),
        _ => None,
    }
}

/// The call the running recording was started for, if any.
fn recording_for_call() -> Option<String> {
    mark_for_session(current_session(), crate::audio::recording_commands::recording_started_at())
}

/// The call-ended prompt's Stop: stops only a recording started for that
/// call, never one started by hand.
async fn stop_recording_for_call<R: Runtime>(app: AppHandle<R>, key: String) -> bool {
    if !crate::is_recording().await || recording_for_call().as_deref() != Some(key.as_str()) {
        info!("Call detection: Stop for {} refused, the running recording isn't that call's", key);
        return false;
    }
    info!("Call detection: stopping the {} call's recording", key);
    crate::tray::stop_recording_from_detection(app).await;
    true
}

/// The floating prompt (public/call-prompt.html): a small window over
/// whatever app is in front, as Spark shows it, since the call app has the
/// focus and the main window is usually hidden. It never takes the focus.
const PROMPT_LABEL: &str = "call-prompt";
const PROMPT_SECONDS: u64 = 30;
const PROMPT_WIDTH: f64 = 400.0;
const PROMPT_HEIGHT: f64 = 96.0;
/// Bumped per prompt, so an old prompt's timer can't close a newer one.
static PROMPT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn query_value(text: &str) -> String {
    text.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b == b'-' { (b as char).to_string() } else { format!("%{:02X}", b) })
        .collect()
}

fn close_prompt<R: Runtime>(app: &AppHandle<R>) {
    use tauri::Manager;
    if let Some(window) = app.get_webview_window(PROMPT_LABEL) {
        let _ = window.close();
    }
}

/// Shows the prompt at the top centre of the main screen: notification
/// banners stack in the top-right corner on macOS, above always-on-top
/// windows, and would cover it (Zoom posts its own banner as a call starts). `kind` is
/// "started" (offer to record) or "ended" (offer to stop).
fn show_prompt<R: Runtime>(app: &AppHandle<R>, key: &str, name: &str, kind: &'static str) {
    use std::sync::atomic::Ordering;
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    close_prompt(app);
    let generation = PROMPT_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let url = format!("call-prompt.html?key={}&name={}&kind={}", query_value(key), query_value(name), kind);
    // Below the macOS menu bar; near the top edge on Windows.
    let top_gap = if cfg!(target_os = "macos") { 34.0 } else { 12.0 };
    let (x, y) = match app.primary_monitor() {
        Ok(Some(monitor)) => {
            let scale = monitor.scale_factor();
            let size = monitor.size().to_logical::<f64>(scale);
            let origin = monitor.position().to_logical::<f64>(scale);
            (origin.x + (size.width - PROMPT_WIDTH) / 2.0, origin.y + top_gap)
        }
        _ => (40.0, top_gap),
    };
    let built = WebviewWindowBuilder::new(app, PROMPT_LABEL, WebviewUrl::App(url.into()))
        .title("Open Meeting Notes")
        .inner_size(PROMPT_WIDTH, PROMPT_HEIGHT)
        .position(x, y)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .always_on_top(true)
        .visible_on_all_workspaces(true)
        .skip_taskbar(true)
        .resizable(false)
        .focused(false)
        .build();
    if let Err(e) = built {
        warn!("Call detection: prompt window not shown: {}", e);
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(PROMPT_SECONDS)).await;
        if PROMPT_GENERATION.load(Ordering::SeqCst) == generation {
            close_prompt(&app);
        }
    });
}

/// A button on the floating prompt. Record starts a recording for the call
/// (unless one is running); Stop stops that call's recording.
#[tauri::command]
pub async fn call_prompt_answer<R: Runtime>(app: AppHandle<R>, key: String, kind: String, accept: bool) {
    close_prompt(&app);
    if !accept {
        return;
    }
    match kind.as_str() {
        "started" => {
            if crate::is_recording().await {
                return;
            }
            mark_pending(key);
            crate::tray::start_recording_from_detection(&app);
        }
        "ended" => {
            stop_recording_for_call(app, key).await;
        }
        _ => {}
    }
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
        let mut last_users: Vec<String> = Vec::new();
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            let settings = load_settings(&app);
            if !settings.enabled {
                tracker = Tracker::default();
                continue;
            }
            let mut users = tokio::task::spawn_blocking(microphone_users).await.unwrap_or_default();
            users.sort();
            // Which apps hold the microphone, when that changes: shows the
            // process name to add when a call app goes unnoticed.
            if users != last_users {
                info!("Call detection: microphone in use by {:?}", users);
                last_users = users.clone();
            }
            // Every call app is tracked; the ignore list only silences
            // prompts, so unticking an app mid-call doesn't look like the
            // call ended.
            let present: HashSet<&'static str> = users.iter().filter_map(|id| classify(id)).map(|a| a.key).collect();
            let (started, ended) = tracker.poll(&present);
            let recording = crate::is_recording().await;
            // Claim a pending mark as soon as its recording starts. Checking
            // only when the call ends would find the 20 s wait long over.
            let call_recording = if recording { recording_for_call() } else { None };
            for key in started {
                let Some(call_app) = CALL_APPS.iter().find(|a| a.key == key) else { continue };
                if recording || settings.ignored_apps.iter().any(|k| k == key) || !onboarding_done(&app).await {
                    continue;
                }
                info!("Call detection: {} started using the microphone", call_app.name);
                let auto_started = settings.auto_start;
                if auto_started {
                    // The recording-started notification says it began.
                    mark_pending(key.to_string());
                    crate::tray::start_recording_from_detection(&app);
                } else {
                    show_prompt(&app, key, call_app.name, "started");
                }
                let _ = app.emit("call-detected", CallEvent { key: key.into(), name: call_app.name.into(), auto_started });
            }
            for key in ended {
                let Some(call_app) = CALL_APPS.iter().find(|a| a.key == key) else { continue };
                info!("Call detection: {} stopped using the microphone", call_app.name);
                // A record prompt still up for this call has nothing left to offer.
                close_prompt(&app);
                if call_recording.as_deref() == Some(key) {
                    info!("Call detection: asking whether to stop the {} call's recording", key);
                    show_prompt(&app, key, call_app.name, "ended");
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
    fn a_mark_belongs_to_the_one_recording_that_started_for_it() {
        // Recording 4 is the user's, then Record marks the call.
        *CALL_MARK.lock().unwrap() = Some(CallMark::Pending { key: "zoom".into(), at: std::time::Instant::now(), after_session: 4 });
        // Waiting for its recording (the page reloads first).
        assert_eq!(mark_for_session(4, None), None);
        // Recording 5 starts for it.
        assert_eq!(mark_for_session(5, Some(std::time::Instant::now())).as_deref(), Some("zoom"));
        assert_eq!(mark_for_session(5, Some(std::time::Instant::now())).as_deref(), Some("zoom"));
        // The user stops it and starts recording 6 at once: not the call's.
        assert_eq!(mark_for_session(6, Some(std::time::Instant::now())), None);

        // A pending mark nobody claimed in time doesn't go to a later recording.
        *CALL_MARK.lock().unwrap() = Some(CallMark::Pending {
            key: "teams".into(),
            at: std::time::Instant::now() - MARK_WAITS_FOR_START - Duration::from_secs(1),
            after_session: 6,
        });
        assert_eq!(mark_for_session(7, Some(std::time::Instant::now())), None);

        // A recording that started 18 s after Record is in time even when
        // the poll that sees it runs at 21 s.
        let marked = std::time::Instant::now() - Duration::from_secs(21);
        *CALL_MARK.lock().unwrap() = Some(CallMark::Pending { key: "meet".into(), at: marked, after_session: 7 });
        assert_eq!(mark_for_session(8, Some(marked + Duration::from_secs(18))).as_deref(), Some("meet"));
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
