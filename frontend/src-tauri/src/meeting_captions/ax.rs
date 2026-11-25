//! Reads Meet's caption region from the browser through the macOS
//! Accessibility API (AXUIElement).

use super::{capture, parse_caption_texts, search_wait, CaptionPiece};
use cidre::{arc, ax, cf, ns, objc};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Browsers by bundle id, and whether they are Chromium-based: Chromium puts
/// web content in the accessibility tree only once a client asks for it.
/// Firefox does so when a client first queries it; Safari always does.
const BROWSERS: [(&str, bool); 6] = [
    ("com.google.Chrome", true),
    ("company.thebrowser.Browser", true), // Arc
    ("com.microsoft.edgemac", true),
    ("com.brave.Browser", true),
    ("org.mozilla.firefox", false),
    ("com.apple.Safari", false),
];
/// The caption region's label in an English Meet. Other languages are
/// recognised by the shape of what the region holds.
const CAPTIONS_LABEL: &str = "Captions";
/// Most elements visited in one walk; a Meet call's page has a few thousand.
const WALK_LIMIT: usize = 20_000;

static NO_PERMISSION_LOGGED: AtomicBool = AtomicBool::new(false);

/// How far a Chromium browser has been asked to expose its web content.
#[derive(Clone, Copy, PartialEq)]
enum Exposed {
    /// AXManualAccessibility, which has no side effects, asked at this time.
    Manual(Instant),
    /// AXEnhancedUserInterface, for a browser that ignored the first. It
    /// changes how some apps animate windows, so it is turned off at stop.
    Enhanced,
}

/// Reads captions until `stop` is set; runs on its own thread.
pub fn run(stop: &AtomicBool) -> Vec<CaptionPiece> {
    // Asking would show the system prompt in the middle of a recording.
    if !ax::is_process_trusted() {
        if !NO_PERMISSION_LOGGED.swap(true, Ordering::SeqCst) {
            log::info!("Meeting captions off: the app has no Accessibility permission");
        }
        return Vec::new();
    }
    // A browser that hangs must not hold up the reader.
    let _ = ax::UiElement::sys_wide().set_messaging_timeout_secs(1.0);
    let mut exposed = HashMap::new();
    let pieces = capture(stop, || find_meet_captions(&mut exposed, stop, false), |region| caption_texts(region, stop));
    unexpose(&exposed);
    pieces
}

/// Turns AXEnhancedUserInterface back off where this reader turned it on.
fn unexpose(exposed: &HashMap<i32, Exposed>) {
    for (pid, how) in exposed {
        if *how == Exposed::Enhanced {
            set_flag(&mut ax::UiElement::with_app_pid(*pid), cf::str!(c"AXEnhancedUserInterface"), false);
        }
    }
}

/// Running browsers: process id, whether it is the frontmost app, and
/// whether it is Chromium-based.
fn running_browsers() -> Vec<(i32, bool, bool)> {
    objc::ar_pool(|| {
        let mut found = Vec::new();
        for (bundle_id, chromium) in BROWSERS {
            for app in ns::RunningApp::with_bundle_id(&ns::String::with_str(bundle_id)).iter() {
                found.push((app.pid(), app.is_active(), chromium));
            }
        }
        found
    })
}

/// A string attribute, when the element has it.
fn text(element: &ax::UiElement, attr: &ax::Attr) -> Option<String> {
    Some(element.attr_value(attr).ok()?.try_as_string()?.to_string())
}

/// The elements an array attribute (children, windows) holds.
fn elements(element: &ax::UiElement, attr: &ax::Attr) -> Vec<arc::R<ax::UiElement>> {
    let Ok(value) = element.attr_value(attr) else { return Vec::new() };
    if value.get_type_id() != cf::Array::type_id() {
        return Vec::new();
    }
    // SAFETY: the value is a CFArray (checked above), and each item is
    // checked to be an AXUIElement before it is used as one.
    let array: &cf::ArrayOf<cf::Type> = unsafe { std::mem::transmute::<&cf::Type, _>(&*value) };
    array
        .iter()
        .filter(|item| item.get_type_id() == ax::UiElement::type_id())
        .map(|item| unsafe { std::mem::transmute::<&cf::Type, &ax::UiElement>(item) }.retained())
        .collect()
}

/// The element's descendants in tree order, with their depth below it and
/// their role. The children of an element with role `stop_at` are skipped.
/// The walk ends early once `stop` is set.
fn descendants(root: &ax::UiElement, stop_at: Option<&str>, stop: &AtomicBool) -> Vec<(usize, String, arc::R<ax::UiElement>)> {
    let mut found = Vec::new();
    let mut stack: Vec<(usize, arc::R<ax::UiElement>)> =
        elements(root, ax::attr::children()).into_iter().rev().map(|e| (1, e)).collect();
    while let Some((depth, element)) = stack.pop() {
        if found.len() >= WALK_LIMIT || stop.load(Ordering::SeqCst) {
            break;
        }
        let role = text(&element, ax::attr::role()).unwrap_or_default();
        if stop_at != Some(role.as_str()) {
            stack.extend(elements(&element, ax::attr::children()).into_iter().rev().map(|e| (depth + 1, e)));
        }
        found.push((depth, role, element));
    }
    found
}

/// "Meet - abc-defg-hij" (or with an en dash): the tab title of a Meet call.
fn is_meet_title(title: &str) -> bool {
    title.strip_prefix("Meet").is_some_and(|rest| rest.starts_with([' ', '-', '\u{2013}']))
}

/// Sets a boolean attribute on an app, such as the switches that ask a
/// Chromium browser to put its web content in the tree.
fn set_flag(app: &mut ax::UiElement, attr: &cf::String, value: bool) {
    let value: &cf::Type = value.into();
    let _ = app.set_attr(ax::Attr::with_string(attr), value);
}

/// The caption region of a Meet window, when captions are on. With several
/// Meet windows, the focused window of the frontmost browser wins, then a
/// focused window of another browser; otherwise the first, and the log says
/// which. A browser shows only its current tab's page, so a Meet call in a
/// background tab is not found.
fn find_meet_captions(exposed: &mut HashMap<i32, Exposed>, stop: &AtomicBool, trace: bool) -> Option<arc::R<ax::UiElement>> {
    let mut found: Vec<(u8, String, arc::R<ax::UiElement>)> = Vec::new();
    for (pid, active, chromium) in running_browsers() {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        let mut app = ax::UiElement::with_app_pid(pid);
        if chromium && !exposed.contains_key(&pid) {
            set_flag(&mut app, cf::str!(c"AXManualAccessibility"), true);
            exposed.insert(pid, Exposed::Manual(Instant::now()));
        }
        let focused = app.attr_value(ax::attr::focused_window()).ok();
        let windows = elements(&app, ax::attr::windows());
        if trace {
            println!("browser pid {} frontmost: {} chromium: {} windows: {}", pid, active, chromium, windows.len());
        }
        for window in windows {
            let title = text(&window, ax::attr::title()).unwrap_or_default();
            if trace {
                println!("   window {:?}", title);
            }
            if !is_meet_title(&title) {
                continue;
            }
            let pages: Vec<_> = descendants(&window, Some("AXWebArea"), stop)
                .into_iter()
                .filter(|(_, role, _)| role == "AXWebArea")
                .map(|(_, _, page)| page)
                .collect();
            if trace {
                println!("      web areas: {}", pages.len());
            }
            let page = pages
                .iter()
                .find(|page| text(page, ax::attr::title()).is_some_and(|t| is_meet_title(&t)))
                .or(pages.first());
            let Some(page) = page else {
                // Still no page a search after AXManualAccessibility: the
                // browser ignored it, so try the other switch once and look
                // again at the next search.
                if let Some(Exposed::Manual(asked)) = exposed.get(&pid) {
                    if asked.elapsed() >= search_wait(0) {
                        set_flag(&mut app, cf::str!(c"AXEnhancedUserInterface"), true);
                        exposed.insert(pid, Exposed::Enhanced);
                    }
                }
                continue;
            };
            if let Some(region) = caption_region(page, stop, trace) {
                let in_focus = focused.as_ref().is_some_and(|f| f.equal(&window));
                found.push(((active && in_focus) as u8 * 2 + in_focus as u8, title, region));
            }
        }
    }
    let best = found.iter().map(|(score, _, _)| *score).max();
    let i = found.iter().position(|(score, _, _)| Some(*score) == best)?;
    if found.len() > 1 {
        log::info!(
            "{} Meet windows show captions; reading {:?} (focused window of the frontmost browser first)",
            found.len(),
            found[i].1
        );
    }
    Some(found.swap_remove(i).2)
}

/// The caption region in a Meet page: the group labelled "Captions", or
/// else the one landmark region whose texts read as speaker blocks.
fn caption_region(page: &ax::UiElement, stop: &AtomicBool, trace: bool) -> Option<arc::R<ax::UiElement>> {
    let mut shaped = Vec::new();
    for (_, role, element) in descendants(page, None, stop) {
        if stop.load(Ordering::SeqCst) {
            return None;
        }
        if role != "AXGroup" {
            continue;
        }
        // aria-label is the description in Chromium and Safari.
        let label = text(&element, ax::attr::desc()).filter(|l| !l.is_empty()).or_else(|| text(&element, ax::attr::title()));
        if label.as_deref() == Some(CAPTIONS_LABEL) {
            if trace {
                println!("      region {:?}: labelled", CAPTIONS_LABEL);
            }
            return Some(element);
        }
        if text(&element, ax::attr::subrole()).as_deref() != Some("AXLandmarkRegion") {
            continue;
        }
        let blocks = parse_caption_texts(&caption_texts(&element, stop).unwrap_or_default()).len();
        if trace {
            println!("      region {:?}: {} speaker blocks", label.unwrap_or_default(), blocks);
        }
        if blocks > 0 {
            shaped.push(element);
        }
    }
    // Two regions with that shape (the chat panel can look alike): unsure.
    if shaped.len() == 1 {
        shaped.pop()
    } else {
        None
    }
}

/// The texts under the caption region in tree order, with their depth; None
/// once the region is gone, which tells the caller to search again.
fn caption_texts(region: &ax::UiElement, stop: &AtomicBool) -> Option<Vec<(usize, String)>> {
    text(region, ax::attr::role())?;
    Some(
        descendants(region, None, stop)
            .into_iter()
            .filter(|(_, role, _)| role == "AXStaticText")
            .map(|(depth, _, element)| {
                let words = text(&element, ax::attr::value())
                    .filter(|v| !v.is_empty())
                    .or_else(|| text(&element, ax::attr::title()))
                    .unwrap_or_default();
                (depth, words)
            })
            .collect(),
    )
}

#[cfg(test)]
mod live {
    use super::*;
    use std::time::Duration;

    /// Prints what the search sees, then the caption texts for 30 s. Run
    /// during a Meet call with captions on, from a terminal that has
    /// Accessibility permission:
    /// cargo test --lib meeting_captions::ax::live -- --ignored --nocapture
    #[test]
    #[ignore]
    fn search_steps() {
        println!("Accessibility permission: {}", ax::is_process_trusted());
        let _ = ax::UiElement::sys_wide().set_messaging_timeout_secs(1.0);
        let stop = AtomicBool::new(false);
        let mut exposed = HashMap::new();
        let mut region = find_meet_captions(&mut exposed, &stop, true);
        for _ in 0..2 {
            if region.is_some() {
                break;
            }
            // Chromium builds its web tree after the first ask, and the
            // second switch is tried only a search interval later.
            std::thread::sleep(search_wait(0));
            println!("searching again");
            region = find_meet_captions(&mut exposed, &stop, true);
        }
        println!("found: {}", region.is_some());
        let Some(region) = region else {
            unexpose(&exposed);
            return;
        };
        for _ in 0..43 {
            let texts = caption_texts(&region, &stop);
            println!("texts: {:?}", texts);
            if let Some(texts) = texts {
                println!("blocks: {:?}", parse_caption_texts(&texts));
            }
            std::thread::sleep(Duration::from_millis(700));
        }
        unexpose(&exposed);
    }
}
