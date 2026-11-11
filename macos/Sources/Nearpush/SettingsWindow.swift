import AppKit
import SwiftUI

/// The main Nearpush window. A plain AppKit window hosting SwiftUI, like `QRWindow`.
/// Closing it does not quit the app: the daemon supervisor and the menu bar icon live in this process.
@MainActor
enum SettingsWindow {
    private static var window: NSWindow?

    static func show() {
        if let window {
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
            return
        }
        let host = NSHostingController(rootView: SettingsView().environmentObject(AppModel.shared))
        // Size is set below and kept by the user. SwiftUI must not drive window size during layout.
        host.sizingOptions = []
        let w = NSWindow(contentViewController: host)
        w.title = "Nearpush"
        w.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        w.isReleasedWhenClosed = false
        w.setContentSize(NSSize(width: 560, height: 680))
        w.setFrameAutosaveName("NearpushSettings")
        window = w
        NSApp.activate(ignoringOtherApps: true)
        w.makeKeyAndOrderFront(nil)
    }
}
