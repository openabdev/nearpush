import AppKit
import SwiftUI
import NearpushCore

@main
struct NearpushApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        MenuBarExtra(isInserted: Binding(
            get: { model.menuBarIconOn },
            set: { model.setMenuBarIcon($0) })
        ) {
            MenuContent().environmentObject(model)
        } label: {
            Image(nsImage: Icons.image(for: model.icon))
                .accessibilityLabel("Nearpush")
        }
        .menuBarExtraStyle(.menu)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        MainActor.assumeIsolated {
            AppModel.shared.start()
            SettingsWindow.show()
        }
    }

    /// A Dock click with no visible window opens the settings window.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag {
            MainActor.assumeIsolated { SettingsWindow.show() }
        }
        return true
    }

    /// Closing the window keeps the app running: the daemon and the menu bar icon live in this process.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Agent mode leaves the daemon running; a child daemon is stopped.
    func applicationWillTerminate(_ notification: Notification) {
        MainActor.assumeIsolated { AppModel.shared.supervisor.stopForQuit() }
    }
}
