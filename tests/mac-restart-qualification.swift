import AppKit
import ApplicationServices
import Foundation

let pid = Int32(CommandLine.arguments[1])!
if CommandLine.arguments.dropFirst(2).first == "quit" {
    guard let app = NSRunningApplication(processIdentifier: pid), app.terminate() else {
        fputs("Could not request a normal quit of the test app.\n", stderr)
        exit(1)
    }
    exit(0)
}
let application = AXUIElementCreateApplication(pid)
func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
    return value
}

func pressRestart() -> Bool {
    var pending = attribute(application, kAXWindowsAttribute) as? [AXUIElement] ?? []
    while let element = pending.popLast() {
        if attribute(element, kAXRoleAttribute) as? String == kAXButtonRole {
            let labels = [kAXTitleAttribute, kAXDescriptionAttribute, kAXValueAttribute].compactMap {
                attribute(element, $0) as? String
            }
            if labels.contains("Restart engine") {
                let result = AXUIElementPerformAction(element, kAXPressAction as CFString)
                print("Pressed Restart engine through Accessibility, result \(result.rawValue)")
                return result == .success
            }
        }
        if let children = attribute(element, kAXChildrenAttribute) as? [AXUIElement] {
            pending.append(contentsOf: children)
        }
    }
    return false
}

guard AXIsProcessTrusted() else {
    fputs("The runner does not permit Accessibility interaction.\n", stderr)
    exit(1)
}
NSRunningApplication(processIdentifier: pid)?.activate(options: [])
AXUIElementSetAttributeValue(application, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)
let deadline = Date().addingTimeInterval(45)
repeat {
    if pressRestart() { exit(0) }
    Thread.sleep(forTimeInterval: 0.25)
} while Date() < deadline
fputs("The app did not expose its Restart engine button.\n", stderr)
exit(1)
