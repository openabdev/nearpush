import AppKit
import CoreImage
import CoreImage.CIFilterBuiltins
import SwiftUI

enum QRCode {
    /// A crisp QR code for `text`, about `points` wide.
    static func image(_ text: String, points: CGFloat = 240) -> NSImage? {
        let f = CIFilter.qrCodeGenerator()
        f.message = Data(text.utf8)
        f.correctionLevel = "M"
        guard let out = f.outputImage else { return nil }
        let scale = max(1, (points * 2 / out.extent.width).rounded(.down)) // 2x for Retina
        let scaled = out.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        guard let cg = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return NSImage(cgImage: cg, size: NSSize(width: scaled.extent.width / 2, height: scaled.extent.height / 2))
    }
}

struct QRView: View {
    let url: String

    var body: some View {
        VStack(spacing: 12) {
            if let img = QRCode.image(url) {
                Image(nsImage: img)
                    .interpolation(.none)
                    .resizable()
                    .frame(width: 240, height: 240)
                    .padding(10)
                    .background(Color.white)
                    .cornerRadius(8)
            }
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
        .padding(20)
        .frame(width: 320)
    }
}

/// The QR window. A plain AppKit window: a SwiftUI `Window` scene could open by
/// itself at launch, which a menu bar app must not do.
@MainActor
enum QRWindow {
    private static var window: NSWindow?

    static func show(url: String) {
        let host = NSHostingController(rootView: QRView(url: url))
        let w = window ?? NSWindow(contentViewController: host)
        w.contentViewController = host
        w.title = "iPhone URL"
        w.styleMask = [.titled, .closable]
        w.isReleasedWhenClosed = false
        w.center()
        window = w
        NSApp.activate(ignoringOtherApps: true)
        w.makeKeyAndOrderFront(nil)
    }
}
