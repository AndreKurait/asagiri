import AppKit
import ApplicationServices
import Foundation

let args = Array(CommandLine.arguments.dropFirst())
guard args.count == 2, ["launch", "restart", "quit"].contains(args[0]) else {
    fputs("Usage: mac-restart-qualification launch APP | restart PID | quit PID\n", stderr)
    exit(2)
}
let action = args[0]
if action == "launch" {
    let config = NSWorkspace.OpenConfiguration()
    config.activates = true
    config.addsToRecentItems = false
    config.createsNewApplicationInstance = true
    config.arguments = ["ui"]
    let names = ["HOME", "ASAGIRI_SOCKET", "ASAGIRI_DATA_DIR", "ASAGIRI_NO_FINDER", "ASAGIRI_IDLE_SECS"]
    config.environment = ProcessInfo.processInfo.environment.filter { names.contains($0.key) }
    let url = URL(fileURLWithPath: args[1], isDirectory: true)
    NSWorkspace.shared.openApplication(at: url, configuration: config) { app, error in
        guard let app, app.bundleURL?.standardizedFileURL == url.standardizedFileURL else {
            fputs("Could not open the test bundle: \(String(describing: error))\n", stderr)
            exit(1)
        }
        print(app.processIdentifier)
        exit(0)
    }
    RunLoop.main.run()
    exit(1)
}
guard let pid = Int32(args[1]), let running = NSRunningApplication(processIdentifier: pid) else {
    fputs("Test app process is missing.\n", stderr)
    exit(1)
}
if action == "quit" {
    guard running.terminate() else {
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

func pressRestart() -> (pressed: Bool, windows: Int, roles: [String: Int], buttons: Set<String>) {
    let windows = attribute(application, kAXWindowsAttribute) as? [AXUIElement] ?? []
    var pending = windows
    var seen = Set<AXUIElement>()
    var roles: [String: Int] = [:]
    var buttons = Set<String>()
    while let element = pending.popLast() {
        guard seen.insert(element).inserted else { continue }
        let role = attribute(element, kAXRoleAttribute) as? String ?? "(unknown)"
        roles[role, default: 0] += 1
        if role == kAXButtonRole {
            let labels = [kAXTitleAttribute, kAXDescriptionAttribute, kAXValueAttribute].compactMap {
                attribute(element, $0) as? String
            }
            buttons.formUnion(labels)
            if labels.contains("Restart engine") {
                let result = AXUIElementPerformAction(element, kAXPressAction as CFString)
                print("Pressed Restart engine through Accessibility, result \(result.rawValue)")
                return (result == .success, windows.count, roles, buttons)
            }
        }
        if let children = attribute(element, kAXChildrenAttribute) as? [AXUIElement] {
            pending.append(contentsOf: children)
        }
    }
    return (false, windows.count, roles, buttons)
}

guard AXIsProcessTrusted() else {
    fputs("The runner does not permit Accessibility interaction.\n", stderr)
    exit(1)
}
let activationRequested = running.activate(options: [])
let enhancedResult = AXUIElementSetAttributeValue(application, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)
let deadline = Date().addingTimeInterval(45)
var observedWindows = 0
var observedRoles: [String: Int] = [:]
var observedButtons = Set<String>()
repeat {
    let snapshot = pressRestart()
    if snapshot.pressed { exit(0) }
    observedWindows = max(observedWindows, snapshot.windows)
    for (role, count) in snapshot.roles { observedRoles[role] = max(observedRoles[role] ?? 0, count) }
    observedButtons.formUnion(snapshot.buttons)
    if running.isTerminated { break }
    Thread.sleep(forTimeInterval: 0.25)
} while Date() < deadline
fputs("Restart button unavailable. Activation requested \(activationRequested), enhanced Accessibility result \(enhancedResult.rawValue), app active \(running.isActive), terminated \(running.isTerminated), windows \(observedWindows), roles \(observedRoles), buttons \(observedButtons.sorted()).\n", stderr)
exit(1)
