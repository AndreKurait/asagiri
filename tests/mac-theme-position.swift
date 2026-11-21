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
waitFor("find theme control") { windows().first.flatMap {button($0,"Dark")} != nil }
let window = windows()[0]
for theme in ["Dark", "Light", "System", "Light", "Dark", "System"] {
    let control = button(window, theme)!
    precondition(AXUIElementPerformAction(control, "AXPress" as CFString) == .success)
    waitFor("persist \(theme)") { guard let data = try? Data(contentsOf: prefs), let state = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return false }; return state["theme"] as? String == theme.lowercased() }
    Thread.sleep(forTimeInterval: 0.4)
    let frame = geometry(window)
    for (index,key) in ["AXCloseButton","AXMinimizeButton","AXZoomButton"].enumerated() {
        let box = geometry(attr(window,key) as! AXUIElement)
        let x = box.minX - frame.minX, y = box.midY - frame.minY
        print(theme, key, "left", x, "center from top", y)
        precondition(abs(y-22) <= 1 && abs(x-CGFloat(20+20*index)) <= 1)
    }
}
