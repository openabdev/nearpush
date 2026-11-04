import AppKit
import Combine
import Foundation
import NearpushCore
import ServiceManagement

@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()

    let client = DaemonClient()
    let supervisor: DaemonSupervisor

    @Published private(set) var result: StatusResult?
    @Published private(set) var checks: [Check] = []
    @Published private(set) var serve: ServeStatus = .unknown("not checked yet")
    @Published private(set) var loginStatus: SMAppService.Status = SMAppService.mainApp.status
    @Published private(set) var summarizerInConfig = true
    /// Last failed action, shown at the top of the menu until the next success.
    @Published var actionError: String?

    private var started = false
    private var bag: Set<AnyCancellable> = []

    static let pollInterval: UInt64 = 3_000_000_000
    static let diagnosticsInterval: TimeInterval = 30

    private init() {
        supervisor = DaemonSupervisor(client: client)
        // Re-render when the supervisor's phase changes.
        supervisor.objectWillChange.sink { [weak self] _ in self?.objectWillChange.send() }.store(in: &bag)
        summarizerInConfig = ConfigEditor.summarizerEnabled(Self.readConfig())
    }

    var status: DaemonStatus? { result?.status }
    var icon: IconState { IconState.derive(result, supervisorFailed: supervisor.failed) }

    /// The iPhone URL: the serve mapping if known, else the daemon's guess.
    var publicURL: String? {
        if case .mapped(let u) = serve { return u }
        return status?.publicUrl
    }

    var statusLine: String {
        switch supervisor.phase {
        case .external(let m), .failed(let m): return "⚠︎ " + m
        default: break
        }
        switch result {
        case .ok(let s)?:
            var parts = [s.paused ? "⏸ paused" : "● running"]
            parts.append(s.viewers == 1 ? "1 iPhone" : "\(s.viewers) iPhones")
            if let e = s.recent.first(where: \.shown) ?? s.recent.first {
                parts.append(e.menuLine)
            }
            let line = parts.joined(separator: " · ")
            return line.count > 90 ? String(line.prefix(89)) + "…" : line
        case .unauthorized?: return "⚠︎ Daemon rejects the token"
        case .external(let why)?: return "⚠︎ External nearpushd: \(why)"
        case .noToken?: return "⚠︎ No push token yet"
        case .unreachable?: return supervisor.phase == .starting ? "Starting nearpushd…" : "⚠︎ Daemon not reachable"
        case nil: return "Starting…"
        }
    }

    func start() {
        guard !started else { return }
        started = true
        Task {
            await supervisor.start()
            await poll() // so the first diagnostics see a real status, not nil
            await refreshDiagnostics()
            var lastDiag = Date()
            while true {
                await poll()
                if Date().timeIntervalSince(lastDiag) >= Self.diagnosticsInterval {
                    lastDiag = Date()
                    await refreshDiagnostics()
                }
                try? await Task.sleep(nanoseconds: Self.pollInterval)
            }
        }
    }

    func poll() async {
        let r = await client.status()
        result = r
        supervisor.observe(r)
        loginStatus = SMAppService.mainApp.status
    }

    func refreshDiagnostics() async {
        let (c, s) = await Diagnostics.run(result: result, tokenURL: client.tokenURL)
        checks = c
        serve = s
    }

    // ---- actions ----------------------------------------------------------------

    private func act(_ name: String, _ body: () async throws -> Void) async {
        do {
            try await body()
            actionError = nil
        } catch {
            actionError = "\(name): \(error.localizedDescription)"
        }
        await poll()
    }

    func setPaused(_ on: Bool) {
        Task { await act("Pause") { _ = try await client.setPaused(on) } }
    }

    func isShown(_ state: AgentState) -> Bool {
        status?.filter.states.contains(state.rawValue) ?? false
    }

    /// Adds or removes one state from the live filter.
    func setShown(_ state: AgentState, _ on: Bool) {
        guard let current = status?.filter.states else { return }
        var next = current.filter { $0 != state.rawValue }
        if on { next.append(state.rawValue) }
        Task { await act("Show on iPhone") { _ = try await client.setStates(next) } }
    }

    var summarizerOn: Bool { status?.summarizer.enabled ?? summarizerInConfig }

    static func readConfig() -> String {
        (try? String(contentsOf: ConfigEditor.defaultURL, encoding: .utf8)) ?? ""
    }

    /// Edits `[summarizer] enabled` in config.toml, then restarts the daemon.
    func setSummarizer(_ on: Bool) {
        Task {
            await act("Summarizer") {
                let text = Self.readConfig()
                try ConfigEditor.writeAtomically(ConfigEditor.setSummarizerEnabled(text, on), to: ConfigEditor.defaultURL)
                summarizerInConfig = on
                await supervisor.restart()
                try? await Task.sleep(nanoseconds: 1_500_000_000)
            }
        }
    }

    func setLaunchAtLogin(_ on: Bool) {
        Task {
            await act("Launch at Login") {
                if on { try SMAppService.mainApp.register() } else { try await SMAppService.mainApp.unregister() }
            }
            loginStatus = SMAppService.mainApp.status
        }
    }

    func restartDaemon() {
        Task { await act("Restart") { await supervisor.restart() } }
    }

    /// Offered only when there is no serve config at all; re-checked right before
    /// running so an existing config is never overwritten.
    func setUpServe() {
        Task {
            await act("tailscale serve") {
                guard case .empty = await Diagnostics.serveStatus() else {
                    throw NSError(domain: "nearpush", code: 1, userInfo: [NSLocalizedDescriptionKey: "a serve config exists now; not touching it"])
                }
                guard let bin = Tailscale.binary() else {
                    throw NSError(domain: "nearpush", code: 2, userInfo: [NSLocalizedDescriptionKey: "tailscale CLI not found"])
                }
                let out = await ProcessRunner.run(bin, ["serve", "--bg", "http://127.0.0.1:8787"], timeout: 15, captureStdout: false)
                guard out?.status == 0 else {
                    throw NSError(domain: "nearpush", code: 3, userInfo: [NSLocalizedDescriptionKey: "tailscale serve --bg exited \(out?.status ?? -1)"])
                }
            }
            await refreshDiagnostics()
        }
    }

    func recheck() {
        Task { await poll(); await refreshDiagnostics() }
    }

    func openLogs() {
        let path = (DaemonSupervisor.logPath as NSString).expandingTildeInPath
        let url = URL(fileURLWithPath: path)
        if FileManager.default.fileExists(atPath: path) {
            NSWorkspace.shared.open(url)
        } else {
            NSWorkspace.shared.open(url.deletingLastPathComponent().deletingLastPathComponent())
        }
    }

    func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}
