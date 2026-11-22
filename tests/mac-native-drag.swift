import AppKit
import ApplicationServices

let pid = Int32(CommandLine.arguments[1])!
let direction: CGFloat = CommandLine.arguments.contains("--reverse") ? -1 : 1
let app = AXUIElementCreateApplication(pid)
print("Display scale factors: \(NSScreen.screens.map { $0.backingScaleFactor })")
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
// A separate process owns this small window over the left end of the strip.
func verifyForeignPress(_ window: AXUIElement, _ before: CGPoint, _ start: CGPoint) -> Bool {
    guard let screen = NSScreen.screens.first else { return false }
    let app = NSApplication.shared
    app.setActivationPolicy(.accessory)
    let rect = NSRect(x: start.x - 84, y: screen.frame.maxY - start.y - 12, width: 64, height: 24)
    let source = NSWindow(contentRect: rect, styleMask: .borderless, backing: .buffered, defer: false)
    source.level = .floating
    source.isMovable = false
    source.orderFrontRegardless()
    defer { source.orderOut(nil) }
    var receivedDown = false
    let monitor = NSEvent.addLocalMonitorForEvents(matching: .leftMouseDown) { event in
        if event.window === source { receivedDown = true }
        return event
    }
    defer { if let monitor { NSEvent.removeMonitor(monitor) } }
    func pump(_ duration: TimeInterval) {
        let end = Date(timeIntervalSinceNow: duration)
        while Date() < end {
            if let event = app.nextEvent(matching: .any, until: end, inMode: .default, dequeue: true) {
                app.sendEvent(event)
            }
        }
    }
    pump(0.2)
    let origin = CGPoint(x: start.x - 50, y: start.y)
    mouse(.mouseMoved, origin)
    mouse(.leftMouseDown, origin)
    pump(0.15)
    mouse(.leftMouseDragged, start)
    pump(0.15)
    let finish = CGPoint(x: start.x + 36, y: start.y + 24)
    mouse(.leftMouseDragged, finish)
    mouse(.leftMouseUp, finish)
    pump(0.3)
    print("Foreign window received press: \(receivedDown), Asagiri origin: \(String(describing: position(window)))")
    return receivedDown && position(window) == before
}
if CommandLine.arguments.contains("--foreign-press") {
    NSRunningApplication(processIdentifier: pid)?.activate(options: [.activateAllWindows])
}
for _ in 0..<80 {
    if NSRunningApplication(processIdentifier: pid)?.isActive == true,
       let window = (attribute(app, "AXWindows") as? [AXUIElement])?.first,
       let before = position(window) {
        Thread.sleep(forTimeInterval: 1)
        let start = CGPoint(x: before.x + 255, y: before.y + 17)
        if CommandLine.arguments.contains("--foreign-press") {
            exit(verifyForeignPress(window, before, start) ? 0 : 1)
        }
        mouse(.mouseMoved, start)
        mouse(.leftMouseDown, start)
        mouse(.leftMouseUp, start)
        Thread.sleep(forTimeInterval: 0.2)
        mouse(.mouseMoved, CGPoint(x: start.x + 50, y: start.y + 30))
        Thread.sleep(forTimeInterval: 0.3)
        guard position(window) == before else {
            print("FAIL window followed the pointer after mouse-up"); exit(1)
        }
        print("PASS fast click followed by pointer motion leaves the window stationary")
        Thread.sleep(forTimeInterval: NSEvent.doubleClickInterval + 0.1)
        mouse(.mouseMoved, start)
        mouse(.leftMouseDown, start)
        // The pointer is already moving when WebKit delivers the strip message.
        // Leave the button down and give AppKit time to process that first move.
        mouse(.leftMouseDragged, CGPoint(x: start.x + direction * 12, y: start.y + direction * 8))
        Thread.sleep(forTimeInterval: 0.12)
        for step in 5...12 {
            mouse(.leftMouseDragged, CGPoint(x: start.x + direction * CGFloat(step * 3), y: start.y + direction * CGFloat(step * 2)))
            Thread.sleep(forTimeInterval: 0.04)
        }
        mouse(.leftMouseUp, CGPoint(x: start.x + direction * 36, y: start.y + direction * 24))
        Thread.sleep(forTimeInterval: 0.4)
        guard let after = position(window) else { exit(1) }
        print("Native window origin before \(before), after \(after)")
        guard abs(after.x - before.x - direction * 36) <= 4, abs(after.y - before.y - direction * 24) <= 4 else { exit(1) }
        print("PASS native title strip drag moved the window by the pointer displacement"); exit(0)
    }
    Thread.sleep(forTimeInterval: 0.25)
}
exit(1)
