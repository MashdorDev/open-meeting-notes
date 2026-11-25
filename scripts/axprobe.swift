// Accessibility probe for call apps.
// swift axprobe.swift <app name or bundle id> [dump|watch] [seconds]
//   dump  : print the whole tree once (default)
//   watch : read the tree every 0.7 s and print only lines that appeared or went away
// Electron/Chromium apps get AXManualAccessibility so web content enters the tree.
import AppKit
import ApplicationServices

let args = CommandLine.arguments
guard args.count >= 2 else {
    print("usage: axprobe <app name|bundle id> [dump|watch] [seconds]"); exit(2)
}
let query = args[1].lowercased()
let mode = args.count >= 3 ? args[2].lowercased() : "dump"
guard mode == "dump" || mode == "watch" else {
    print("usage: axprobe <app name|bundle id> [dump|watch] [seconds]"); exit(2)
}
let seconds = args.count >= 4 ? Double(args[3]) ?? 60 : 60
let maxNodes = 40000

guard AXIsProcessTrusted() else { print("no Accessibility permission"); exit(1) }

let running = NSWorkspace.shared.runningApplications.filter { $0.activationPolicy == .regular }
let exact = running.filter {
    $0.localizedName?.lowercased() == query || $0.bundleIdentifier?.lowercased() == query
}
let partial = running.filter {
    ($0.localizedName?.lowercased().contains(query) ?? false)
        || ($0.bundleIdentifier?.lowercased().contains(query) ?? false)
}
let candidates = exact.isEmpty ? partial : exact
guard let app = candidates.first else { print("not running: \(query)"); exit(1) }
if candidates.count > 1 {
    print("several apps match, using the first: " + candidates.map { $0.bundleIdentifier ?? "?" }.joined(separator: ", "))
}
print("app: \(app.localizedName ?? "?") \(app.bundleIdentifier ?? "?") pid \(app.processIdentifier)")

let root = AXUIElementCreateApplication(app.processIdentifier)
// A timeout set on one element does not reach the elements read from it;
// the system-wide element sets it for every element, so a hung app costs
// at most 1 s per read instead of 6.
AXUIElementSetMessagingTimeout(AXUIElementCreateSystemWide(), 1.0)
let manual = AXUIElementSetAttributeValue(root, "AXManualAccessibility" as CFString, kCFBooleanTrue)
print("AXManualAccessibility set: \(manual == .success ? "ok" : "error \(manual.rawValue)")")

func attr(_ e: AXUIElement, _ name: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success ? v : nil
}
func str(_ e: AXUIElement, _ name: String) -> String {
    guard let v = attr(e, name) else { return "" }
    if let s = v as? String { return s }
    if let n = v as? NSNumber { return n.stringValue }
    return ""
}
func clip(_ s: String) -> String {
    let one = s.replacingOccurrences(of: "\n", with: "⏎")
    return one.count > 160 ? String(one.prefix(160)) + "…" : one
}

func walk() -> [String] {
    var out: [String] = []
    var count = 0
    var ancestors: [AXUIElement] = []
    func visit(_ e: AXUIElement, _ depth: Int, _ context: String) {
        // Zoom's toolbar buttons list themselves as their own child.
        if ancestors.contains(where: { CFEqual($0, e) }) { return }
        count += 1
        if count > maxNodes || depth > 60 { return }
        let role = str(e, "AXRole")
        let sub = str(e, "AXSubrole")
        let title = str(e, "AXTitle")
        let desc = str(e, "AXDescription")
        let value = str(e, "AXValue")
        let help = str(e, "AXHelp")
        let ident = str(e, "AXIdentifier")
        var parts = [role + (sub.isEmpty ? "" : "/\(sub)")]
        if !title.isEmpty { parts.append("title=\(clip(title))") }
        if !desc.isEmpty { parts.append("desc=\(clip(desc))") }
        if !value.isEmpty { parts.append("value=\(clip(value))") }
        if !help.isEmpty { parts.append("help=\(clip(help))") }
        if !ident.isEmpty { parts.append("id=\(clip(ident))") }
        let line = parts.joined(separator: " ")
        out.append(String(repeating: "  ", count: depth) + line + (context.isEmpty ? "" : "   ⟨in: \(context)⟩"))
        let label = !desc.isEmpty ? desc : title
        let childContext = label.isEmpty ? context : clip(role + " " + label)
        let kids = (attr(e, "AXChildren") as? [AXUIElement]) ?? []
        ancestors.append(e)
        for k in kids { visit(k, depth + 1, childContext) }
        ancestors.removeLast()
    }
    let windows = (attr(root, "AXWindows") as? [AXUIElement]) ?? []
    for w in windows { visit(w, 0, "") }
    if count > maxNodes { out.append("… stopped at \(maxNodes) nodes") }
    return out
}

if mode == "dump" {
    // Chromium builds the web tree lazily; give it a moment after the flag.
    // Web trees fill in after they are first asked for: walk, wait, walk again.
    for _ in 0..<3 { _ = walk(); Thread.sleep(forTimeInterval: 1.0) }
    let t0 = Date()
    let lines = walk()
    lines.forEach { print($0) }
    print("— \(lines.count) nodes in \(String(format: "%.2f", Date().timeIntervalSince(t0))) s")
} else {
    for _ in 0..<3 { _ = walk(); Thread.sleep(forTimeInterval: 1.0) }
    var prev = Set(walk())
    print("baseline: \(prev.count) lines; watching \(Int(seconds)) s")
    let end = Date().addingTimeInterval(seconds)
    while Date() < end {
        Thread.sleep(forTimeInterval: 0.7)
        let cur = walk()
        let curSet = Set(cur)
        let stamp = String(format: "%6.1f", seconds - end.timeIntervalSinceNow)
        for l in cur where !prev.contains(l) { print("\(stamp) + \(l.trimmingCharacters(in: .whitespaces))") }
        for l in prev.subtracting(curSet).sorted() { print("\(stamp) - \(l.trimmingCharacters(in: .whitespaces))") }
        fflush(stdout)
        prev = curSet
    }
}
