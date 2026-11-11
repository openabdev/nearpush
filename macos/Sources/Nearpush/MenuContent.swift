import AppKit
import NearpushCore
import SwiftUI

/// The menu bar dropdown: `QuickMenu.items`, nothing else. Settings live in the settings window.
struct MenuContent: View {
    @EnvironmentObject var model: AppModel

    var body: some View {
        Group {
            ForEach(Array(QuickMenu.items.enumerated()), id: \.offset) { _, item in
                row(item)
            }
        }
        .onAppear { model.menuDidOpen() }
        .onDisappear { model.menuDidClose() }
    }

    @ViewBuilder private func row(_ item: QuickMenuItem) -> some View {
        switch item {
        case .status:
            Text(model.menuStatusLine)
        case .settings:
            Button(item.title ?? "") { SettingsWindow.show() }
                .keyboardShortcut(KeyEquivalent(item.shortcut ?? ","))
        case .pause:
            Toggle(item.title ?? "", isOn: Binding(
                get: { model.status?.paused ?? false },
                set: { model.setPaused($0) }))
                .disabled(model.status == nil)
        case .quit:
            Divider()
            Button(item.title ?? "") { NSApp.terminate(nil) }
                .keyboardShortcut(KeyEquivalent(item.shortcut ?? "q"))
        }
    }
}
