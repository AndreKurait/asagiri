import AppKit
import ApplicationServices

let pid = Int32(CommandLine.arguments[1])!
let app = AXUIElementCreateApplication(pid)
let marker = "asagiri-native-edit-255"
func expect(_ message: String, _ condition: () -> Bool) {
    let deadline = Date().addingTimeInterval(15)
    repeat {
        if condition() { print("PASS \(message)"); return }
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
    } while Date() < deadline
    fputs("FAIL \(message)\n", stderr); exit(1)
}
func attr(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success ? value : nil
}
func value() -> String? {
    guard let focus = attr(app, "AXFocusedUIElement") else { return nil }
    return attr(focus as! AXUIElement, "AXValue") as? String
}
func key(_ code: CGKeyCode, _ flags: CGEventFlags = .maskCommand) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!
        event.flags = flags
        event.postToPid(pid)
    }
}
expect("AX access on disposable runner") { AXIsProcessTrusted() }
expect("native window active") { NSRunningApplication(processIdentifier: pid)?.isActive == true }
func find(_ element: AXUIElement, depth: Int, predicate: (AXUIElement) -> Bool) -> AXUIElement? {
    if predicate(element) { return element }
    if depth > 0 {
        for child in attr(element, "AXChildren") as? [AXUIElement] ?? [] {
            if let match = find(child, depth: depth - 1, predicate: predicate) { return match }
        }
    }
    return nil
}
func findInWindow(_ predicate: (AXUIElement) -> Bool) -> AXUIElement? {
    guard let window = (attr(app, "AXWindows") as? [AXUIElement])?.first else { return nil }
    return find(window, depth: 24, predicate: predicate)
}
key(43) // Command-comma opens Settings.
expect("opened Network settings") {
    guard let button = findInWindow({ item in
        attr(item, "AXRole") as? String == "AXButton" && ["AXTitle", "AXDescription"].contains { attr(item, $0) as? String == "Network" }
    }) else { return false }
    return AXUIElementPerformAction(button, kAXPressAction as CFString) == .success
}
expect("focused the configured HTTP proxy field") {
    guard let field = findInWindow({ attr($0, "AXValue") as? String == "initial.invalid:3128" }) else { return false }
    _ = AXUIElementSetAttributeValue(field, kAXFocusedAttribute as CFString, kCFBooleanTrue)
    return value() == "initial.invalid:3128"
}
let board = NSPasteboard.general
key(0); board.clearContents(); key(8)
expect("Command-C copied the configured HTTP proxy") { board.string(forType: .string) == "initial.invalid:3128" }
board.clearContents(); board.setString(marker, forType: .string)
key(9); expect("Command-V replaced the selected HTTP proxy text") { value() == marker }
