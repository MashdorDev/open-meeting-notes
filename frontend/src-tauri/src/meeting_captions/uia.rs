//! Reads Meet's caption region from the browser through Windows UI Automation.

use super::{parse_caption_texts, CaptionLog, CaptionPiece};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::UI::Accessibility::*;

/// Captions show up within a second of the words; a read takes a few ms.
const READ_EVERY: Duration = Duration::from_millis(700);
/// Looking for a Meet tab walks every browser window, so it is not done on every read.
const SEARCH_EVERY: Duration = Duration::from_secs(5);
/// Top-level windows of Firefox and of Chromium browsers (Chrome, Edge).
const BROWSER_CLASSES: [&str; 2] = ["MozillaWindowClass", "Chrome_WidgetWin_1"];

static RUNNING: Mutex<Option<(Arc<AtomicBool>, JoinHandle<Vec<CaptionPiece>>)>> = Mutex::new(None);

pub fn start() {
    let stop_flag = Arc::new(AtomicBool::new(false));
    let flag = stop_flag.clone();
    let handle = std::thread::spawn(move || {
        // SAFETY: COM is set up and torn down on this thread, which owns
        // every UI Automation object it creates.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let pieces = match CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
                Ok(automation) => capture(&automation, &flag),
                Err(e) => {
                    log::warn!("Meeting captions off: UI Automation unavailable: {}", e);
                    Vec::new()
                }
            };
            CoUninitialize();
            pieces
        }
    });
    let previous = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).replace((stop_flag, handle));
    if let Some((flag, handle)) = previous {
        flag.store(true, Ordering::SeqCst);
        let _ = handle.join();
    }
}

pub fn stop() -> Vec<CaptionPiece> {
    let running = RUNNING.lock().unwrap_or_else(|p| p.into_inner()).take();
    let Some((flag, handle)) = running else { return Vec::new() };
    flag.store(true, Ordering::SeqCst);
    handle.join().unwrap_or_else(|_| {
        log::warn!("Meeting caption reader panicked");
        Vec::new()
    })
}

unsafe fn capture(automation: &IUIAutomation, stop: &AtomicBool) -> Vec<CaptionPiece> {
    let started = Instant::now();
    let mut log = CaptionLog::default();
    let mut captions: Option<IUIAutomationElement> = None;
    let mut last_search: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        if captions.is_none() && last_search.is_none_or(|t| t.elapsed() >= SEARCH_EVERY) {
            last_search = Some(Instant::now());
            captions = find_meet_captions(automation).ok().flatten();
            if captions.is_some() {
                log::info!("Reading speaker names from Meet's captions");
            }
        }
        if let Some(region) = &captions {
            match caption_texts(automation, region) {
                Ok(texts) => log.observe("meet", started.elapsed().as_secs_f64(), &parse_caption_texts(&texts)),
                // The tab closed or captions were turned off.
                Err(_) => captions = None,
            }
        }
        std::thread::sleep(READ_EVERY);
    }
    let pieces = log.into_pieces();
    log::info!("Captured {} caption pieces", pieces.len());
    pieces
}

/// The "Captions" group of a Meet tab in a browser window, when captions are on.
unsafe fn find_meet_captions(automation: &IUIAutomation) -> windows::core::Result<Option<IUIAutomationElement>> {
    let documents = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0))?;
    let region = automation.CreateAndCondition(
        &automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_GroupControlTypeId.0))?,
        &automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from("Captions"))?,
    )?;
    let windows = automation.GetRootElement()?.FindAll(TreeScope_Children, &automation.CreateTrueCondition()?)?;
    for i in 0..windows.Length()? {
        let window = windows.GetElement(i)?;
        if !BROWSER_CLASSES.contains(&window.CurrentClassName()?.to_string().as_str()) {
            continue;
        }
        let docs = window.FindAll(TreeScope_Descendants, &documents)?;
        for j in 0..docs.Length()? {
            let doc = docs.GetElement(j)?;
            // "Meet - abc-defg-hij": the tab title of a Meet call.
            if !doc.CurrentName()?.to_string().starts_with("Meet ") {
                continue;
            }
            if let Ok(found) = doc.FindFirst(TreeScope_Descendants, &region) {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}

/// The texts under the caption region in tree order, with their depth.
unsafe fn caption_texts(automation: &IUIAutomation, region: &IUIAutomationElement) -> windows::core::Result<Vec<(usize, String)>> {
    // Fails once the element is gone, which tells the caller to search again.
    region.CurrentName()?;
    let walker = automation.ControlViewWalker()?;
    let mut texts = Vec::new();
    walk(&walker, region, 0, &mut texts)?;
    Ok(texts)
}

unsafe fn walk(
    walker: &IUIAutomationTreeWalker,
    parent: &IUIAutomationElement,
    depth: usize,
    texts: &mut Vec<(usize, String)>,
) -> windows::core::Result<()> {
    let mut child = walker.GetFirstChildElement(parent).ok();
    while let Some(element) = child {
        if element.CurrentControlType()? == UIA_TextControlTypeId {
            texts.push((depth, element.CurrentName()?.to_string()));
        }
        walk(walker, &element, depth + 1, texts)?;
        child = walker.GetNextSiblingElement(&element).ok();
    }
    Ok(())
}

#[cfg(test)]
mod live {
    use super::*;

    /// Prints what the search sees; run during a Meet call with captions on:
    /// cargo test --lib meeting_captions::uia::live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn search_steps() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
            let documents = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0)).unwrap();
            let windows = automation.GetRootElement().unwrap().FindAll(TreeScope_Children, &automation.CreateTrueCondition().unwrap()).unwrap();
            for i in 0..windows.Length().unwrap() {
                let w = windows.GetElement(i).unwrap();
                let class = w.CurrentClassName().unwrap().to_string();
                if !BROWSER_CLASSES.contains(&class.as_str()) { continue; }
                let docs = w.FindAll(TreeScope_Descendants, &documents);
                let n = docs.as_ref().map(|d| d.Length().unwrap_or(-1)).unwrap_or(-2);
                println!("window [{}] {} documents: {}", class, w.CurrentName().unwrap(), n);
                if let Ok(docs) = docs {
                    for j in 0..n { println!("   doc: {:?}", docs.GetElement(j).unwrap().CurrentName().unwrap().to_string()); }
                }
            }
            println!("found: {:?}", find_meet_captions(&automation).map(|f| f.is_some()));
        }
    }
}
