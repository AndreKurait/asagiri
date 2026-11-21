import ApplicationServices
import Foundation

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
    if attr(e, "AXRole") as? String == "AXButton", ["AXTitle", "AXDescription"].contains(where: {attr(e, $0) as? String == label}) { return e }
    for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] {
        if let found = button(child, label, depth: depth + 1) { return found }
    }
    return nil
}
func waitFor(_ condition: () -> Bool) {
    for _ in 0..<100 { if condition() {return}; Thread.sleep(forTimeInterval: 0.1) }; fatalError("Timed out")
}
func windows() -> [AXUIElement] { attr(app, "AXWindows") as? [AXUIElement] ?? [] }
waitFor { windows().first.flatMap {button($0,"Dark")} != nil }
let window = windows()[0]
for theme in ["Dark", "Light", "System", "Light", "Dark", "System"] {
    let control = button(window, theme)!
    precondition(AXUIElementPerformAction(control, "AXPress" as CFString) == .success)
    waitFor { guard let data = try? Data(contentsOf: prefs), let state = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return false }; return state["theme"] as? String == theme.lowercased() }
    Thread.sleep(forTimeInterval: 0.4)
    let frame = geometry(window)
    for (index,key) in ["AXCloseButton","AXMinimizeButton","AXZoomButton"].enumerated() {
        let box = geometry(attr(window,key) as! AXUIElement)
        let x = box.minX - frame.minX, y = box.midY - frame.minY
        print(theme, key, "left", x, "center from top", y)
        precondition(abs(y-22) <= 1 && abs(x-CGFloat(20+20*index)) <= 1)
    }
}
