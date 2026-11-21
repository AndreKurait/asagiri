import ApplicationServices
import Foundation
import AppKit

let app = AXUIElementCreateApplication(pid_t(CommandLine.arguments[1])!)
let prefs = URL(fileURLWithPath: CommandLine.arguments[2])
func attr(_ e: AXUIElement, _ key: String) -> CFTypeRef? {
    var v: CFTypeRef?
    guard AXUIElementCopyAttributeValue(e, key as CFString, &v) == .success else { return nil }
    return v
}
func geometry(_ e: AXUIElement) -> CGRect {
    var point = CGPoint.zero, size = CGSize.zero
    precondition(AXValueGetValue(attr(e, "AXPosition") as! AXValue, .cgPoint, &point))
    precondition(AXValueGetValue(attr(e, "AXSize") as! AXValue, .cgSize, &size))
    return CGRect(origin: point, size: size)
}
func button(_ e: AXUIElement, _ label: String, depth: Int = 0) -> AXUIElement? {
    if depth > 40 { return nil }
    if ["AXPress"].allSatisfy({ action in
        var actions: CFArray?
        return AXUIElementCopyActionNames(e, &actions) == .success && (actions as? [String] ?? []).contains(action)
    }), ["AXTitle", "AXDescription"].contains(where: {attr(e, $0) as? String == label}) { return e }
    for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] {
        if let found = button(child, label, depth: depth + 1) { return found }
    }
    return nil
}
func waitFor(_ message: String, _ condition: () -> Bool) {
    for _ in 0..<100 { if condition() {return}; Thread.sleep(forTimeInterval: 0.1) }; 
    func dump(_ e: AXUIElement, _ depth: Int) {
        guard depth < 24 else { return }; print(["AXRole", "AXTitle", "AXDescription", "AXValue"].map { attr(e,$0) as? String ?? "" }.joined(separator:" | "))
        for child in attr(e,"AXChildren") as? [AXUIElement] ?? [] { dump(child,depth+1) }
    }; dump(app,0); print("Timed out",message); exit(1)
}
func windows() -> [AXUIElement] { attr(app, "AXWindows") as? [AXUIElement] ?? [] }
let rtl = CommandLine.arguments[3] == "ar"
let labels = rtl ? ["Dark":"داكن", "Light":"فاتح", "System":"النظام"] : ["Dark":"Dark", "Light":"Light", "System":"System"]
waitFor("find theme control") { windows().first.flatMap {button($0,labels["Dark"]!)} != nil }
let window = windows()[0]
func measure(_ phase: String) {
    let frame = geometry(window)
    for (index,key) in ["AXCloseButton","AXMinimizeButton","AXZoomButton"].enumerated() {
        let box = geometry(attr(window,key) as! AXUIElement)
        let x = rtl ? frame.maxX - box.maxX : box.minX - frame.minX, y = box.midY - frame.minY
        print(phase, key, "outer inset", x, "center from top", y)
        guard abs(y-22) <= 1 && abs(x-CGFloat(20+20*index)) <= 1 else {
            print("FAIL", phase, key, "window", frame, "button", box); exit(1)
        }
    }
}

func cycle(_ phase: String) {
    for theme in ["Dark", "Light", "System"] {
        let control = button(window, labels[theme]!)!
        precondition(AXUIElementPerformAction(control, "AXPress" as CFString) == .success)
        waitFor("persist theme") {
            guard let data = try? Data(contentsOf: prefs), let state = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return false }
            return state["theme"] as? String == theme.lowercased()
        }
        Thread.sleep(forTimeInterval: 0.5)
        measure(phase + " " + theme)
    }
}
func lifecycleChecks() {
cycle("launch")
let original = geometry(window).size
var resized = CGSize(width: original.width + 60, height: original.height - 40)
precondition(AXUIElementSetAttributeValue(window, "AXSize" as CFString, AXValueCreate(.cgSize, &resized)!) == .success)
waitFor("resized") { abs(geometry(window).width - original.width) > 10 }
cycle("resize")
for entering in [true, false] {
    precondition(AXUIElementSetAttributeValue(window, "AXFullScreen" as CFString, entering ? kCFBooleanTrue : kCFBooleanFalse) == .success)
    waitFor("full screen transition") { attr(window,"AXFullScreen") as? Bool == entering }
    Thread.sleep(forTimeInterval: 1.5)
}
cycle("full screen exit")
let target = NSRunningApplication(processIdentifier: pid_t(CommandLine.arguments[1])!)!
let finder = NSRunningApplication.runningApplications(withBundleIdentifier:"com.apple.finder").first!
_ = finder.activate(options: [.activateIgnoringOtherApps]); waitFor("focus Finder") { finder.isActive }
let frame = geometry(window)
let point = CGPoint(x: frame.midX, y: frame.minY + 22)
for kind in [CGEventType.leftMouseDown, .leftMouseUp] {
    CGEvent(mouseEventSource: nil, mouseType: kind, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
}
waitFor("refocus app by clicking window") { target.isActive }
cycle("refocus")
}

// This check runs on a disposable hosted Mac and restores its initial appearance.
func systemAppearance(_ statement: String) throws -> String {
    let script = Process()
    let output = Pipe()
    script.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    script.arguments = ["-e", "tell application \"System Events\" to tell appearance preferences to " + statement]
    script.standardOutput = output
    try script.run()
    script.waitUntilExit()
    guard script.terminationStatus == 0 else { throw NSError(domain: "System Events appearance", code: Int(script.terminationStatus)) }
    return String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
}
func checkAppearanceChanges() throws {
    let original = try systemAppearance("get dark mode")
    guard ["true", "false"].contains(original) else { throw NSError(domain: "Unexpected appearance", code: 1) }
    defer { _ = try? systemAppearance("set dark mode to " + original) }
    for state in [original == "true" ? "false" : "true", original] {
        _ = try systemAppearance("set dark mode to " + state)
        let reported = try systemAppearance("get dark mode")
        precondition(reported == state)
        Thread.sleep(forTimeInterval: 1)
        measure("System follows macOS dark mode " + state)
    }
}
cycle("launch before system switch")
try checkAppearanceChanges()
lifecycleChecks()
