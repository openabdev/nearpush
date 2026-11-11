import AppKit
import NearpushCore
import ServiceManagement
import SwiftUI

/// The main window: status, display filter, classifier, phone URL, relay, diagnostics and general settings.
/// Every control reads and writes `AppModel`, the same as the menu bar menu.
struct SettingsView: View {
    @EnvironmentObject var model: AppModel

    private var cl: ClassifierStatus? { model.classifierStatus }

    var body: some View {
        Form {
            statusSection
            displaySection
            classifierSection
            phoneSection
            relaySection
            diagnosticsSection
            generalSection
        }
        .formStyle(.grouped)
        .frame(minWidth: 520, minHeight: 600)
    }

    // MARK: Status

    private var statusSection: some View {
        Section("Status") {
            LabeledContent("Daemon", value: model.statusLine)
            LabeledContent("Mode", value: model.supervisor.modeDescription)
            LabeledContent("iPhones connected", value: "\(model.status?.viewers ?? 0)")
            LabeledContent("Paused", value: model.status.map { $0.paused ? "yes" : "no" } ?? "unknown")
            LabeledContent("Last event", value: model.status?.recent.first?.menuLine ?? "none")
            LabeledContent("Relay connected", value: model.relay.map { $0.connected ? "yes" : "no" } ?? "unknown")
            LabeledContent("Relay last_id", value: model.relay.map { "\($0.lastId)" } ?? "unknown")
            LabeledContent("Classifier provider", value: ProviderPicker.selected(cl)?.title ?? cl?.provider ?? "unknown")
            LabeledContent("Classifier model", value: cl?.model ?? "unknown")
        }
    }

    // MARK: Display

    private var displaySection: some View {
        Section("Display") {
            Toggle("Pause pushes", isOn: Binding(
                get: { model.status?.paused ?? false },
                set: { model.setPaused($0) }))
                .disabled(model.status == nil)

            ForEach(ShowOnPhone.rows, id: \.self) { st in
                Toggle("Show on iPhone: \(st.label)", isOn: Binding(
                    get: { model.isShown(st) },
                    set: { model.setShown(st, $0) }))
                    .disabled(model.status == nil)
            }
            Text("Generic events (no state) always show on iPhone.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    // MARK: Classifier

    private var classifierSection: some View {
        Section("Classifier") {
            Picker("Provider", selection: Binding(
                get: { ProviderPicker.selected(cl) },
                set: { if let p = $0 { model.setClassifier(p) } })) {
                ForEach(ClassifierProvider.allCases, id: \.self) { p in
                    Text(p.title).tag(Optional(p))
                }
            }
            .disabled(model.status == nil || !ProviderPicker.isEnabled(cl))

            if let cl {
                if cl.enabled {
                    LabeledContent("Model", value: cl.model)
                } else {
                    Text("Classifier off in config.toml")
                        .foregroundStyle(.secondary)
                }
            }

            Toggle("Session labels (summarizer)", isOn: Binding(
                get: { model.summarizerOn },
                set: { model.setSummarizer($0) }))
            Text("Edits [summarizer] enabled in config.toml and restarts nearpushd.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    // MARK: Phone

    private var phoneSection: some View {
        Section("Phone") {
            if let url = model.publicURL {
                Text(url)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                HStack {
                    Button("Copy URL") { model.copy(url) }
                    Button("Show QR Code…") { QRWindow.show(url: url) }
                }
                QRCodeView(url: url, side: 200)
                    .id(url)
                    .frame(maxWidth: .infinity)
            } else {
                Text("No URL yet: set up tailscale serve (see Diagnostics).")
                    .foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Relay

    private var relaySection: some View {
        Section("Relay") {
            if let relay = model.relay {
                LabeledContent("Enabled", value: relay.enabled ? "yes" : "no")
                LabeledContent("Connected", value: relay.connected ? "yes" : "no")
                LabeledContent("last_id", value: "\(relay.lastId)")
                LabeledContent("Last error class", value: relay.lastErrorClass ?? "none")
            } else {
                Text("Not reported by this daemon.")
                    .foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Diagnostics

    private var diagnosticsSection: some View {
        Section("Diagnostics") {
            ForEach(model.checks) { c in
                VStack(alignment: .leading, spacing: 2) {
                    Text((c.ok ? "✓ " : "✗ ") + c.title)
                    if let hint = c.hint {
                        Text(hint)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            if let reason = model.supervisor.reason {
                Text(reason)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            HStack {
                if case .empty = model.serve {
                    Button("Set up tailscale serve") { model.setUpServe() }
                }
                Button("Check again") { model.recheck() }
                Button("Open logs") { model.openLogs() }
            }
        }
    }

    // MARK: General

    private var generalSection: some View {
        Section("General") {
            Toggle("Launch at Login", isOn: Binding(
                get: { model.loginStatus == .enabled || model.loginStatus == .requiresApproval },
                set: { model.setLaunchAtLogin($0) }))
            if model.loginStatus == .requiresApproval {
                Button("Approve in System Settings › Login Items…") { SMAppService.openSystemSettingsLoginItems() }
            }

            Toggle("Show menu bar icon", isOn: Binding(
                get: { model.menuBarIconOn },
                set: { model.setMenuBarIcon($0) }))

            Button("Quit Nearpush") { NSApp.terminate(nil) }
        }
    }
}
