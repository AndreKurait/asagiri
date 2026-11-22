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
func menuTitles(_ element: AXUIElement, _ depth: Int = 0) -> [String] {
    let title = attr(element, "AXTitle") as? String ?? ""
    guard depth < 7 else { return [title] }
    let children = attr(element, "AXChildren") as? [AXUIElement] ?? []
    return [title] + children.flatMap { menuTitles($0, depth + 1) }
}
var editMenu: AXUIElement?
expect("German Edit menu is present in the native menu bar") {
    guard let bar = attr(app, "AXMenuBar") else { return false }
    let entries = attr(bar as! AXUIElement, "AXChildren") as? [AXUIElement] ?? []
    editMenu = entries.first { attr($0, "AXTitle") as? String == "Bearbeiten" }
    if editMenu == nil { print("Menu titles: \(entries.map { attr($0, "AXTitle") as? String ?? "" })") }
    return editMenu != nil
}
let menu = editMenu!
_ = AXUIElementPerformAction(menu, kAXPressAction as CFString)
var lastTitles: [String] = []
expect("German native Edit menu exposes localized clipboard and history commands") {
    let titles = menuTitles(menu).filter { !$0.isEmpty }
    if titles != lastTitles { print("Observed native Edit titles: \(titles)"); lastTitles = titles }
    return ["Rückgängig", "Wiederholen", "Ausschneiden", "Kopieren", "Einfügen", "Alle auswählen"].allSatisfy { titles.contains($0) }
}
key(53, []) // Dismiss the menu before editing the focused search field.
let board = NSPasteboard.general
board.clearContents(); board.setString(marker, forType: .string)
key(9); expect("Command-V inserts clipboard text") { value() == marker }
key(0); key(7); expect("Command-X cuts the selection") { value() == "" && board.string(forType: .string) == marker }
key(6); expect("Command-Z restores cut text") { value() == marker }
key(6, [.maskCommand, .maskShift]); expect("Shift-Command-Z reapplies the cut") { value() == "" }
key(9); expect("paste after redo") { value() == marker }
key(0); board.clearContents(); key(8); expect("Command-C copies selected text") { board.string(forType: .string) == marker }

key(53, [])
key(50, .maskControl)
Thread.sleep(forTimeInterval: 2)
let path = "/tmp/host-terminal-" + UUID().uuidString
board.clearContents()
board.setString("printf %s host-terminal-paste > " + path, forType: .string)
key(9)
Thread.sleep(forTimeInterval: 0.5)
key(36, [])
expect("host shell executes command pasted through Command-V") {
    (try? String(contentsOfFile: path, encoding: .utf8)) == "host-terminal-paste"
}

// The literal output marker is absent from the command echoed by the shell.
let copyID = UUID().uuidString
let output = "terminal-copy-" + copyID
board.clearContents()
board.setString("printf '%s%s\\n' 'terminal-' 'copy-" + copyID + "'", forType: .string)
key(9)
Thread.sleep(forTimeInterval: 0.5)
key(36, [])
Thread.sleep(forTimeInterval: 1)
board.clearContents()
key(0)
Thread.sleep(forTimeInterval: 0.3)
key(8)
expect("Command-A and Command-C copy rendered host shell output") {
    board.string(forType: .string)?.contains(output) == true
}
