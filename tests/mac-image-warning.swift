import AppKit
import ApplicationServices

let app = AXUIElementCreateApplication(Int32(CommandLine.arguments[1])!)
let backup = CommandLine.arguments[2]
func attr(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}
func shows(_ element: AXUIElement, _ text: String, depth: Int = 0) -> Bool {
    for field in ["AXValue", "AXTitle", "AXDescription"] {
        if let value = attr(element, field) as? String, value.contains(text) { return true }
    }
    guard depth < 30 else { return false }
    return (attr(element, "AXChildren") as? [AXUIElement] ?? []).contains { shows($0, text, depth: depth + 1) }
}
func expect(_ message: String, _ predicate: () -> Bool) {
    let end = Date().addingTimeInterval(40)
    repeat {
        if predicate() { print("PASS \(message)"); return }
        RunLoop.current.run(until: Date().addingTimeInterval(0.3))
    } while Date() < end
    fputs("FAIL \(message)\n", stderr); exit(1)
}
expect("native Accessibility access") { AXIsProcessTrusted() }
expect("Images shows recovered Alpine and the exact recovery path") { shows(app, "alpine") && shows(app, backup) }
try FileManager.default.removeItem(atPath: backup)
expect("Images removes the warning after the retained backup is removed") { shows(app, "alpine") && !shows(app, backup) }
