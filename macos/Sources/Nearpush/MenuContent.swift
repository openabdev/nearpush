import AppKit
import NearpushCore
import ServiceManagement
import SwiftUI

struct MenuContent: View {
    @EnvironmentObject var model: AppModel

    private func binding(_ get: @escaping () -> Bool, _ set: @escaping (Bool) -> Void) -> Binding<Bool> {
        Binding(get: get, set: set)
    }

    var body: some View {
        Text(model.statusLine)
        if let err = model.actionError {
            Text("⚠︎ " + err)
        }
        Divider()

        Menu("iPhone URL") {
            if let url = model.publicURL {
                Text(url)
                Button("Show QR Code…") { QRWindow.show(url: url) }
                Button("Copy URL") { model.copy(url) }
            } else {
                Text("No URL yet: set up tailscale serve (see Diagnostics)")
            }
        }

        Divider()
        Text("Recent")
        if let recent = model.status?.recent, !recent.isEmpty {
            ForEach(recent) { e in
                Button(e.menuLine) { model.copy(e.copyText) }
            }
        } else {
            Text("No events yet")
        }

        Divider()
        Toggle("Pause pushes", isOn: binding({ model.status?.paused ?? false }, model.setPaused))
            .disabled(model.status == nil)
        Menu("Show on iPhone") {
            ForEach(AgentState.allCases, id: \.self) { st in
                Toggle(st.label, isOn: binding({ model.isShown(st) }, { model.setShown(st, $0) }))
            }
            Divider()
            Text("Events without a state always show")
        }
        .disabled(model.status == nil)
        Toggle("Summarizer", isOn: binding({ model.summarizerOn }, model.setSummarizer))
        Toggle("Launch at Login", isOn: binding({ model.loginStatus == .enabled || model.loginStatus == .requiresApproval },
                                                 model.setLaunchAtLogin))
        if model.loginStatus == .requiresApproval {
            Button("Approve in System Settings › Login Items…") { SMAppService.openSystemSettingsLoginItems() }
        }

        Menu("Diagnostics") {
            ForEach(model.checks) { c in
                Text((c.ok ? "✓ " : "✗ ") + c.title)
                if let hint = c.hint {
                    Text("    " + hint)
                }
            }
            Divider()
            Text(model.supervisor.modeDescription)
            if let reason = model.supervisor.reason {
                Text("    " + reason)
            }
            if case .empty = model.serve {
                Button("Set up tailscale serve") { model.setUpServe() }
            }
            Button("Check again") { model.recheck() }
        }

        Divider()
        Button("Restart daemon") { model.restartDaemon() }
        Button("Open logs") { model.openLogs() }
        Button("Quit Nearpush") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
