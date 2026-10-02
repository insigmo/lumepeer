// Runs decode-bench.html inside a WKWebView — the engine lumepeer's macOS
// guest decodes with — and prints the page's console lines.
//
//   swift decode-webkit-mac.swift /path/to/decode-bench.html
//
// The view sits in a real (small, off-screen-capable) window: a WKWebView
// that is not in any window is treated as hidden, and WebKit throttles the
// timers of hidden pages, which would distort the paced run.
import Cocoa
import WebKit

final class Sink: NSObject, WKScriptMessageHandler {
    func userContentController(_ c: WKUserContentController, didReceive m: WKScriptMessage) {
        guard let line = m.body as? String else { return }
        print(line)
        fflush(stdout)
        if line.hasPrefix("DECODE-BENCH-DONE") { NSApp.terminate(nil) }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let config = WKWebViewConfiguration()
let sink = Sink()
config.userContentController.add(sink, name: "log")
config.userContentController.addUserScript(WKUserScript(
    source: "const __log = console.log; console.log = (...a) => { window.webkit.messageHandlers.log.postMessage(a.join(' ')); __log(...a); };",
    injectionTime: .atDocumentStart, forMainFrameOnly: true))
let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 640, height: 400),
                      styleMask: [.titled], backing: .buffered, defer: false)
let view = WKWebView(frame: window.contentView!.bounds, configuration: config)
window.contentView!.addSubview(view)
window.orderFrontRegardless()
let page = URL(fileURLWithPath: CommandLine.arguments[1])
view.loadFileURL(page, allowingReadAccessTo: page.deletingLastPathComponent())
DispatchQueue.main.asyncAfter(deadline: .now() + 600) {
    print("DECODE-BENCH-TIMEOUT")
    NSApp.terminate(nil)
}
app.run()
