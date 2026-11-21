import AppKit
import ApplicationServices

let pid = Int32(CommandLine.arguments[1])!
let app = AXUIElementCreateApplication(pid)
func attribute(_ el: AXUIElement, _ name: String) -> CFTypeRef? {
    var value: CFTypeRef?
    return AXUIElementCopyAttributeValue(el, name as CFString, &value) == .success ? value : nil
}
func position(_ window: AXUIElement) -> CGPoint? {
    guard let value = attribute(window, "AXPosition") else { return nil }
    var point = CGPoint.zero
    return AXValueGetValue(value as! AXValue, .cgPoint, &point) ? point : nil
}
func mouse(_ type: CGEventType, _ point: CGPoint) {
    CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left)!.post(tap: .cghidEventTap)
}
for _ in 0..<80 {
    if NSRunningApplication(processIdentifier: pid)?.isActive == true,
       let window = (attribute(app, "AXWindows") as? [AXUIElement])?.first,
       let before = position(window) {
        Thread.sleep(forTimeInterval: 1)
        let start = CGPoint(x: before.x + 255, y: before.y + 17)
        mouse(.mouseMoved, start); mouse(.leftMouseDown, start)
        Thread.sleep(forTimeInterval: 0.25)
        for step in 1...12 {
            mouse(.leftMouseDragged, CGPoint(x: start.x + CGFloat(step * 3), y: start.y + CGFloat(step * 2)))
            Thread.sleep(forTimeInterval: 0.04)
        }
        mouse(.leftMouseUp, CGPoint(x: start.x + 36, y: start.y + 24))
        Thread.sleep(forTimeInterval: 0.4)
        guard let after = position(window) else { exit(1) }
        print("Native window origin before \(before), after \(after)")
        guard abs(after.x - before.x - 36) <= 4, abs(after.y - before.y - 24) <= 4 else { exit(1) }
        print("PASS native title strip drag moved the window by the pointer displacement"); exit(0)
    }
    Thread.sleep(forTimeInterval: 0.25)
}
exit(1)
