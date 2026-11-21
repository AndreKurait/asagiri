import AppKit
import ApplicationServices

func require(_ condition: Bool, _ message: String) {
    if !condition { fputs("FAIL \(message)\n", stderr); exit(1) }
    print("PASS \(message)")
}
func waitFor(_ predicate: () -> Bool) -> Bool {
    let deadline = Date().addingTimeInterval(15)
    repeat {
        if predicate() { return true }
        RunLoop.current.run(until: Date().addingTimeInterval(0.2))
    } while Date() < deadline
    return false
}
func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
    return value
}
func children(_ element: AXUIElement, _ name: String = "AXChildren") -> [AXUIElement] {
    attribute(element, name) as? [AXUIElement] ?? []
}
func press(_ item: AXUIElement) -> Bool {
    AXUIElementPerformAction(item, kAXPressAction as CFString) == .success
}
let pid = Int32(CommandLine.arguments[1])!
let daemon = Int32(CommandLine.arguments[2])!
let app = AXUIElementCreateApplication(pid)
func policy(_ pid: Int32) -> NSApplication.ActivationPolicy? {
    NSRunningApplication(processIdentifier: pid)?.activationPolicy
}
require(waitFor { policy(pid) == .regular }, "window process has regular Dock policy")
require(policy(daemon) != .regular, "daemon has no regular Dock entry")
require(AXIsProcessTrusted(), "runner has accessibility access for native window and menu interactions")
require(waitFor { !children(app, "AXWindows").isEmpty }, "management window exists")
let window = children(app, "AXWindows")[0]
guard let close = attribute(window, "AXCloseButton") else { fatalError("window has no close button") }
require(press(close as! AXUIElement), "pressed native window close button")
require(waitFor { policy(pid) == .accessory && children(app, "AXWindows").isEmpty }, "last window closed and UI became accessory")
guard let extras = attribute(app, "AXExtrasMenuBar") else { fatalError("menu bar item unavailable") }
let items = children(extras as! AXUIElement)
require(!items.isEmpty && press(items[0]), "opened Asagiri menu bar item")
func findOpen(_ element: AXUIElement, depth: Int) -> AXUIElement? {
    if attribute(element, "AXTitle") as? String == "Open Asagiri" { return element }
    if depth > 0 { for child in children(element) { if let item = findOpen(child, depth: depth - 1) { return item } } }
    return nil
}
require(waitFor { findOpen(items[0], depth: 4) != nil }, "Open Asagiri menu command exists")
require(press(findOpen(items[0], depth: 4)!), "selected Open Asagiri")
require(waitFor { policy(pid) == .regular && !children(app, "AXWindows").isEmpty }, "window and Dock entry restored")
