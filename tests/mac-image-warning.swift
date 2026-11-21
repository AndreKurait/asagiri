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
    var count = 0
    func dump(_ e: AXUIElement, _ depth: Int) {
        guard count < 400 && depth < 30 else { return }; count += 1
        print(["AXRole", "AXTitle", "AXDescription", "AXValue"].map { String((attr(e, $0) as? String ?? "").prefix(180)) }.joined(separator: " | "))
        for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] { dump(child, depth + 1) }
    }
    dump(app, 0)
    fputs("FAIL \(message)\n", stderr); exit(1)
}
expect("native Accessibility access") { AXIsProcessTrusted() }
let pid = Int32(CommandLine.arguments[1])!
for down in [true, false] {
    let event = CGEvent(keyboardEventSource: nil, virtualKey: 40, keyDown: down)!
    event.flags = .maskCommand
    event.postToPid(pid)
}
expect("Images lists the recovered Alpine image") { shows(app, "alpine") }
expect("Images displays the original index backup path") { shows(app, backup) }
try FileManager.default.removeItem(atPath: backup)
expect("Images removes the warning after the retained backup is removed") { shows(app, "alpine") && !shows(app, backup) }
