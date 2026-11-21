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
expect("Command-K focuses the search field") {
    key(40)
    guard let focus = attr(app, "AXFocusedUIElement") else { return false }
    return attr(focus as! AXUIElement, "AXRole") as? String == "AXComboBox" || attr(focus as! AXUIElement, "AXRole") as? String == "AXTextField"
}
func hasMenuTitle(_ element: AXUIElement, _ title: String, _ depth: Int = 0) -> Bool {
    if attr(element, "AXTitle") as? String == title { return true }
    guard depth < 5 else { return false }
    return (attr(element, "AXChildren") as? [AXUIElement] ?? []).contains { hasMenuTitle($0, title, depth + 1) }
}
expect("German native Edit menu exposes localized clipboard and history commands") {
    guard let bar = attr(app, "AXMenuBar") else { return false }
    return ["Bearbeiten", "Rückgängig", "Wiederholen", "Ausschneiden", "Kopieren", "Einfügen", "Alle auswählen"].allSatisfy { hasMenuTitle(bar as! AXUIElement, $0) }
}
let board = NSPasteboard.general
board.clearContents(); board.setString(marker, forType: .string)
key(9); expect("Command-V inserts clipboard text") { value() == marker }
key(0); key(7); expect("Command-X cuts the selection") { value() == "" && board.string(forType: .string) == marker }
key(6); expect("Command-Z restores cut text") { value() == marker }
key(6, [.maskCommand, .maskShift]); expect("Shift-Command-Z reapplies the cut") { value() == "" }
key(9); expect("paste after redo") { value() == marker }
key(0); board.clearContents(); key(8); expect("Command-C copies selected text") { board.string(forType: .string) == marker }
