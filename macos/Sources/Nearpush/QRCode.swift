import AppKit
import NearpushCore
import SwiftUI

/// The QR image alone, drawn at `side` points. Used by the QR window and the settings window.
struct QRCodeView: View {
    let url: String
    let side: CGFloat
    /// Generated once in init, never in body, so each SwiftUI render stays cheap and side-effect free.
    /// Callers that change `url` must rebuild this view (for example with `.id(url)`).
    private let image: NSImage?

    init(url: String, side: CGFloat) {
        self.url = url
        self.side = side
        self.image = QRImage.cgImage(for: url, points: side).map {
            NSImage(cgImage: $0, size: NSSize(width: side, height: side))
        }
    }

    var body: some View {
        if let image {
            Image(nsImage: image)
                .interpolation(.none)
                .resizable()
                .frame(width: side, height: side)
                .padding(10)
                .background(Color.white)
                .cornerRadius(8)
        } else {
            Text(url.isEmpty
                 ? "No public URL is configured. Set up tailscale serve (see Diagnostics)."
                 : "Could not draw a QR code for this URL.")
                .multilineTextAlignment(.center)
                .frame(width: side, height: side)
        }
    }
}

struct QRView: View {
    let url: String

    var body: some View {
        VStack(spacing: 12) {
            QRCodeView(url: url, side: 240)
            if !url.isEmpty {
                Text(url)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                Text("Scan with the iPhone camera, open in Safari, then Share › Add to Home Screen.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                Button("Copy URL") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(url, forType: .string)
                }
            }
        }
        .padding(20)
        .frame(width: 320, height: 420)
    }
}

/// The QR window. A plain AppKit window: a SwiftUI `Window` scene could open by
/// itself at launch, which a menu bar app must not do.
@MainActor
enum QRWindow {
    private static var window: NSWindow?
    private static var host: NSHostingController<QRView>?

    static func show(url: String) {
        if let window, let host {
            // Reuse the window and its controller. Only the root view changes.
            host.rootView = QRView(url: url)
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
            return
        }
        let host = NSHostingController(rootView: QRView(url: url))
        // Size is fixed by QRView's frame. SwiftUI must not drive window size during layout.
        host.sizingOptions = []
        let w = NSWindow(contentViewController: host)
        w.setContentSize(NSSize(width: 320, height: 420))
        w.title = "iPhone URL"
        w.styleMask = [.titled, .closable]
        w.isReleasedWhenClosed = false
        w.center()
        window = w
        self.host = host
        NSApp.activate(ignoringOtherApps: true)
        w.makeKeyAndOrderFront(nil)
    }
}
