//! Reads Meet's caption region from the browser through Windows UI Automation.

use super::{capture, parse_caption_texts, CaptionPiece};
use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::*;

/// Top-level windows of Firefox and of Chromium browsers (Chrome, Edge).
const BROWSER_CLASSES: [&str; 2] = ["MozillaWindowClass", "Chrome_WidgetWin_1"];
/// Chromium's window class is shared by Electron apps (VS Code, Slack,
/// Discord), whose trees are large, so only these programs are searched.
const BROWSER_PROGRAMS: [&str; 7] = ["chrome.exe", "msedge.exe", "brave.exe", "firefox.exe", "arc.exe", "vivaldi.exe", "opera.exe"];
/// The caption region's label in an English Meet. Other languages are
/// recognised by the shape of what the region holds.
const CAPTIONS_LABEL: &str = "Captions";

/// Reads captions until `stop` is set; runs on its own thread.
pub fn run(stop: &AtomicBool) -> Vec<CaptionPiece> {
    // SAFETY: COM is set up and torn down on this thread, which owns
    // every UI Automation object it creates.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let pieces = match CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(automation) => capture(
                stop,
                || find_meet_captions(&automation, stop, false).ok().flatten(),
                |region| caption_texts(&automation, region).ok(),
            ),
            Err(e) => {
                log::warn!("Meeting captions off: UI Automation unavailable: {}", e);
                Vec::new()
            }
        };
        CoUninitialize();
        pieces
    }
}

/// Whether the window belongs to one of the browsers in `BROWSER_PROGRAMS`.
unsafe fn is_browser(window: &IUIAutomationElement) -> bool {
    let Ok(pid) = window.CurrentProcessId() else { return false };
    let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid as u32) else { return false };
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    let read = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len);
    let _ = CloseHandle(process);
    if read.is_err() {
        return false;
    }
    let path = String::from_utf16_lossy(&path[..len as usize]).to_lowercase();
    BROWSER_PROGRAMS.contains(&path.rsplit('\\').next().unwrap_or(""))
}

/// The top-level window that holds keyboard focus, if any.
unsafe fn focused_window(automation: &IUIAutomation) -> Option<IUIAutomationElement> {
    let root = automation.GetRootElement().ok()?;
    let walker = automation.RawViewWalker().ok()?;
    let mut element = automation.GetFocusedElement().ok()?;
    loop {
        let parent = walker.GetParentElement(&element).ok()?;
        if automation.CompareElements(&parent, &root).ok()?.as_bool() {
            return Some(element);
        }
        element = parent;
    }
}

/// The caption region of a Meet tab, when captions are on. With several
/// Meet tabs, the one in the focused window and on screen wins; otherwise
/// the first, and the log says which. Gives up as soon as `stop` is set.
unsafe fn find_meet_captions(automation: &IUIAutomation, stop: &AtomicBool, trace: bool) -> windows::core::Result<Option<IUIAutomationElement>> {
    let documents = automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_DocumentControlTypeId.0))?;
    let windows = automation.GetRootElement()?.FindAll(TreeScope_Children, &automation.CreateTrueCondition()?)?;
    let focused = focused_window(automation);
    let mut found: Vec<(u8, String, IUIAutomationElement)> = Vec::new();
    for i in 0..windows.Length()? {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let window = windows.GetElement(i)?;
        let class = window.CurrentClassName()?.to_string();
        if !BROWSER_CLASSES.contains(&class.as_str()) || !is_browser(&window) {
            continue;
        }
        let in_focus = match &focused {
            Some(f) => automation.CompareElements(f, &window)?.as_bool(),
            None => false,
        };
        let docs = window.FindAll(TreeScope_Descendants, &documents)?;
        if trace {
            println!("window [{}] {:?} focused: {} documents: {}", class, window.CurrentName()?.to_string(), in_focus, docs.Length()?);
        }
        for j in 0..docs.Length()? {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            let doc = docs.GetElement(j)?;
            let title = doc.CurrentName()?.to_string();
            if trace {
                println!("   doc: {:?}", title);
            }
            // "Meet - abc-defg-hij": the tab title of a Meet call.
            if !title.starts_with("Meet ") {
                continue;
            }
            if let Some(region) = caption_region(automation, &doc, stop, trace)? {
                // A background tab's document is off screen.
                let on_screen = !doc.CurrentIsOffscreen()?.as_bool();
                found.push((in_focus as u8 * 2 + on_screen as u8, title, region));
            }
        }
    }
    let best = found.iter().map(|(score, _, _)| *score).max();
    let Some(i) = found.iter().position(|(score, _, _)| Some(*score) == best) else { return Ok(None) };
    if found.len() > 1 {
        log::info!(
            "{} Meet tabs show captions; reading {:?} (focused window and on screen first)",
            found.len(),
            found[i].1
        );
    }
    Ok(Some(found.swap_remove(i).2))
}

/// The caption region in a Meet document: the region labelled "Captions",
/// or else the one landmark region whose texts read as speaker blocks.
unsafe fn caption_region(
    automation: &IUIAutomation,
    doc: &IUIAutomationElement,
    stop: &AtomicBool,
    trace: bool,
) -> windows::core::Result<Option<IUIAutomationElement>> {
    let labelled = automation.CreateAndCondition(
        &automation.CreatePropertyCondition(UIA_ControlTypePropertyId, &VARIANT::from(UIA_GroupControlTypeId.0))?,
        &automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from(CAPTIONS_LABEL))?,
    )?;
    // ARIA regions with a label are custom landmarks.
    let landmark = automation.CreatePropertyCondition(UIA_LandmarkTypePropertyId, &VARIANT::from(UIA_CustomLandmarkTypeId.0))?;
    let candidates = doc.FindAll(TreeScope_Descendants, &automation.CreateOrCondition(&labelled, &landmark)?)?;
    let mut shaped = Vec::new();
    for i in 0..candidates.Length()? {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let region = candidates.GetElement(i)?;
        let name = region.CurrentName()?.to_string();
        if name == CAPTIONS_LABEL {
            if trace {
                println!("      region {:?}: labelled", name);
            }
            return Ok(Some(region));
        }
        let blocks = parse_caption_texts(&caption_texts(automation, &region).unwrap_or_default()).len();
        if trace {
            println!("      region {:?}: {} speaker blocks", name, blocks);
        }
        if blocks > 0 {
            shaped.push(region);
        }
    }
    // Two regions with that shape (the chat panel can look alike): unsure.
    Ok(if shaped.len() == 1 { shaped.pop() } else { None })
}

/// The texts under the caption region in tree order, with their depth.
unsafe fn caption_texts(automation: &IUIAutomation, region: &IUIAutomationElement) -> windows::core::Result<Vec<(usize, String)>> {
    // Fails once the element is gone, which tells the caller to search again.
    region.CurrentName()?;
    let walker = automation.ControlViewWalker()?;
    let mut texts = Vec::new();
    walk(&walker, region, 0, &mut texts);
    Ok(texts)
}

/// An element that goes away mid-read (Meet replaces caption lines as it
/// revises) is skipped, not taken as the region being gone.
unsafe fn walk(walker: &IUIAutomationTreeWalker, parent: &IUIAutomationElement, depth: usize, texts: &mut Vec<(usize, String)>) {
    let mut child = walker.GetFirstChildElement(parent).ok();
    while let Some(element) = child {
        if element.CurrentControlType().is_ok_and(|t| t == UIA_TextControlTypeId) {
            if let Ok(name) = element.CurrentName() {
                texts.push((depth, name.to_string()));
            }
        }
        walk(walker, &element, depth + 1, texts);
        child = walker.GetNextSiblingElement(&element).ok();
    }
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
            let found = find_meet_captions(&automation, &AtomicBool::new(false), true);
            println!("found: {:?}", found.as_ref().map(|f| f.is_some()));
            if let Ok(Some(region)) = found {
                println!("texts: {:?}", caption_texts(&automation, &region));
            }
        }
    }
}
