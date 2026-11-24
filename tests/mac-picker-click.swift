import AppKit
import Vision

let label = CommandLine.arguments[1]
let screenshot = URL(fileURLWithPath: CommandLine.arguments[2])
let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
try VNImageRequestHandler(url: screenshot).perform([request])
let screen = CGDisplayBounds(CGMainDisplayID())
let normalize = { (s: String) in s.replacingOccurrences(of: "…", with: "...").trimmingCharacters(in: .whitespaces) }
for observation in request.results ?? [] {
    guard let candidate = observation.topCandidates(1).first else { continue }
    print(candidate.string)
    guard let range = candidate.string.range(of: label) ?? (normalize(candidate.string) == normalize(label) ? candidate.string.startIndex..<candidate.string.endIndex : nil), let bounds = try candidate.boundingBox(for: range)?.boundingBox else { continue }
    let point = CGPoint(x: bounds.midX * screen.width, y: (1 - bounds.midY) * screen.height)
    for type in [CGEventType.leftMouseDown, .leftMouseUp] {
        CGEvent(mouseEventSource: nil, mouseType: type, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
    }
    exit(0)
}
exit(1)
