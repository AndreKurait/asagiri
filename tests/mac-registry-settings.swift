import AppKit
import ApplicationServices
let app = AXUIElementCreateApplication(Int32(CommandLine.arguments[1])!)
func attr(_ e: AXUIElement,_ k: String)->CFTypeRef? { var o:CFTypeRef?; _ = AXUIElementCopyAttributeValue(e,k as CFString,&o); return o }
func find(_ e:AXUIElement,_ text:String,_ role:String,_ depth:Int=0)->AXUIElement? { if (attr(e,"AXRole") as? String)==role && ["AXDescription","AXTitle","AXHelp","AXValue"].contains(where:{(attr(e,$0) as? String)==text}) {return e}; if depth<25 {for c in attr(e,"AXChildren") as? [AXUIElement] ?? [] {if !CFEqual(c,e), let v=find(c,text,role,depth+1){return v}}};return nil }
func dumpTree(_ e: AXUIElement, _ depth: Int = 0) {
    guard depth < 16 else { return }
    let attributes = ["AXRole", "AXTitle", "AXDescription", "AXValue"].compactMap { attr(e, $0) as? String }
    print(depth, attributes.map { String($0.prefix(100)) })
    for child in attr(e, "AXChildren") as? [AXUIElement] ?? [] where !CFEqual(child,e) { dumpTree(child,depth+1) }
}
func press(_ text:String,_ roles:[String]) { for _ in 0..<60 { for role in roles { if let e=find(app,text,role), AXUIElementPerformAction(e,kAXPressAction as CFString) == .success {print("Pressed",text);return} };Thread.sleep(forTimeInterval:0.3) }; dumpTree(app); print("Missing",text);exit(1) }
press("Registry mirrors",["AXButton","AXRadioButton"])
let pid = Int32(CommandLine.arguments[1])!
let removing = CommandLine.arguments.dropFirst(2).first == "remove"
let address = removing ? "" : "registry.test:5153"
for _ in 0..<60 {
    if let field = find(app, "Insecure registries", "AXTextArea") {
        guard AXUIElementSetAttributeValue(field, kAXFocusedAttribute as CFString, kCFBooleanTrue) == .success else { exit(3) }
        func key(_ code: CGKeyCode, _ flags: CGEventFlags) {
            for down in [true, false] {
                let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!
                event.flags = flags
                event.postToPid(pid)
            }
        }
        if removing {
            guard attr(field, "AXValue") as? String == "registry.test:5153" else { exit(5) }
            key(0, .maskCommand)
            key(51, [])
        } else {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(address, forType: .string)
            key(9, .maskCommand)
        }
        for _ in 0..<60 {
            if attr(field, "AXValue") as? String == address,
               let save = find(app, "Save", "AXButton"), attr(save, "AXEnabled") as? Bool == true {
                press("Save", ["AXButton"])
                exit(0)
            }
            Thread.sleep(forTimeInterval: 0.3)
        }
        print("Paste value or enabled Save missing")
        dumpTree(app)
        exit(4)
    }
    Thread.sleep(forTimeInterval: 0.3)
}
exit(2)
