import AppKit
import Foundation
import NearpushCore
import ServiceManagement

/// Starts and watches the bundled nearpushd.
///
/// Order: (1) if something already answers on 127.0.0.1:8787 and it is not the
/// bundled binary, report it and never start a second daemon; (2) preferred mode
/// `agent`: register `Contents/Library/LaunchAgents/dev.nearpush.agent.plist`
/// with `SMAppService.agent` and wait up to 10 s for the bundled daemon to answer;
/// (3) on any failure there, unregister, remember `daemonMode = child` with the
/// reason, and run nearpushd as a child process restarted with backoff.
/// Override for testing: `defaults write dev.nearpush.app daemonMode child|agent`.
@MainActor
final class DaemonSupervisor: ObservableObject {
    enum Mode: String {
        case agent, child
    }

    enum Phase: Equatable {
        case idle
        case starting
        /// The bundled daemon answers; `Mode` says who keeps it alive.
        case running(Mode)
        /// Another nearpushd owns the port; nothing is started.
        case external(String)
        case failed(String)
    }

    static let agentPlist = "dev.nearpush.agent.plist"
    static let agentLabel = "dev.nearpush.agent"
    static let logPath = "~/Library/Logs/nearpush/nearpushd.log"
    private static let modeKey = "daemonMode"
    private static let reasonKey = "daemonModeReason"

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var mode: Mode
    @Published private(set) var reason: String?

    private let client: DaemonClient
    private var child: Process?
    private var backoff: TimeInterval = 1
    private var childStartedAt = Date.distantPast
    private var restartTask: Task<Void, Never>?
    private var starting = false
    private var quitting = false

    init(client: DaemonClient) {
        self.client = client
        let d = UserDefaults.standard
        mode = Mode(rawValue: d.string(forKey: Self.modeKey) ?? "") ?? .agent
        reason = d.string(forKey: Self.reasonKey)
    }

    /// `Contents/MacOS/nearpushd` of this bundle, symlinks resolved.
    var bundledExe: String {
        Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/nearpushd").resolvingSymlinksInPath().path
    }

    func isBundled(_ exe: String?) -> Bool {
        guard let exe else { return false }
        return URL(fileURLWithPath: exe).resolvingSymlinksInPath().path == bundledExe
    }

    var modeDescription: String {
        switch phase {
        case .running(.agent): return "Daemon: launchd agent (\(Self.agentLabel))"
        case .running(.child): return "Daemon: child process of the app"
        case .external: return "Daemon: external, not managed"
        case .starting: return "Daemon: starting (\(mode.rawValue))"
        case .failed: return "Daemon: failed"
        case .idle: return "Daemon: idle"
        }
    }

    var failed: Bool {
        switch phase {
        case .external, .failed: return true
        default: return false
        }
    }

    private func setMode(_ m: Mode, reason r: String?) {
        mode = m
        reason = r
        UserDefaults.standard.set(m.rawValue, forKey: Self.modeKey)
        UserDefaults.standard.set(r, forKey: Self.reasonKey)
    }

    /// What to do about an answer on the port.
    private enum Probe {
        case ours, external(String), nobody, tokenRejected
    }

    private func probe() async -> Probe {
        switch await client.status() {
        case .ok(let s):
            return isBundled(s.exe) ? .ours : .external("External nearpushd (\(s.exe ?? "unknown path")) owns 127.0.0.1:8787; not starting the bundled one")
        case .external(let why):
            return .external("External nearpushd (\(why)) owns 127.0.0.1:8787; not starting the bundled one")
        case .unauthorized:
            return .tokenRejected
        case .unreachable, .noToken:
            return .nobody
        }
    }

    func start() async {
        guard !starting, !quitting else { return }
        starting = true
        defer { starting = false }
        phase = .starting
        switch await probe() {
        case .ours:
            // Already up (agent from an earlier run, or an orphaned child).
            phase = .running(SMAppService.agent(plistName: Self.agentPlist).status == .enabled ? .agent : .child)
            return
        case .external(let msg):
            phase = .external(msg)
            return
        case .tokenRejected:
            phase = .failed("A nearpushd on 127.0.0.1:8787 rejects ~/.config/nearpush/token; not starting another one")
            return
        case .nobody:
            break
        }
        if mode == .agent {
            if let err = await startAgent() {
                let svc = SMAppService.agent(plistName: Self.agentPlist)
                try? await svc.unregister()
                setMode(.child, reason: err)
            } else {
                phase = .running(.agent)
                return
            }
        }
        await spawnChild()
    }

    /// `nil` on success, else why agent mode cannot be used.
    private func startAgent() async -> String? {
        let svc = SMAppService.agent(plistName: Self.agentPlist)
        if svc.status != .enabled {
            do { try svc.register() } catch {
                return "SMAppService.agent register failed: \(error.localizedDescription) (\((error as NSError).code))"
            }
        }
        switch svc.status {
        case .requiresApproval: return "SMAppService.agent requires approval in System Settings › Login Items"
        case .notFound: return "SMAppService.agent: \(Self.agentPlist) not found in the bundle"
        case .notRegistered: return "SMAppService.agent stayed notRegistered after register()"
        default: break
        }
        for _ in 0..<20 {
            try? await Task.sleep(nanoseconds: 500_000_000)
            if case .ok(let s) = await client.status(), isBundled(s.exe) { return nil }
        }
        return "SMAppService.agent registered but the bundled nearpushd did not answer within 10 s"
    }

    private func spawnChild() async {
        guard !quitting, child == nil else { return }
        switch await probe() {
        case .ours:
            phase = .running(.child)
            return
        case .external(let msg):
            phase = .external(msg)
            return
        case .tokenRejected:
            phase = .failed("A nearpushd on 127.0.0.1:8787 rejects ~/.config/nearpush/token")
            return
        case .nobody:
            break
        }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: bundledExe)
        p.arguments = ["--log-file", Self.logPath]
        p.standardInput = FileHandle.nullDevice
        p.standardOutput = FileHandle.nullDevice
        p.standardError = FileHandle.nullDevice
        p.terminationHandler = { [weak self] proc in
            let status = proc.terminationStatus
            Task { @MainActor in self?.childExited(proc, status: status) }
        }
        do {
            try p.run()
        } catch {
            phase = .failed("Cannot start \(bundledExe): \(error.localizedDescription)")
            return
        }
        child = p
        childStartedAt = Date()
        phase = .running(.child)
    }

    private func childExited(_ proc: Process, status: Int32) {
        guard proc === child else { return }
        child = nil
        if quitting { return }
        if Date().timeIntervalSince(childStartedAt) > 60 { backoff = 1 }
        let delay = backoff
        backoff = min(backoff * 2, 30)
        phase = .starting
        restartTask?.cancel()
        restartTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            self?.restartTask = nil
            await self?.spawnChild() // re-probes first: an external daemon stops supervision
        }
    }

    /// Called on every status poll so a daemon that vanished is noticed.
    func observe(_ result: StatusResult) {
        guard !starting, !quitting else { return }
        switch (phase, result) {
        case (.running(.child), .unreachable) where child == nil && restartTask == nil:
            Task { await spawnChild() } // adopted orphan died
        case (.external, .unreachable), (.failed, .unreachable), (.idle, _):
            Task { await start() } // the port is free now (e.g. the old LaunchAgent was booted out)
        case (.running, .ok(let s)) where !isBundled(s.exe):
            phase = .external("External nearpushd (\(s.exe ?? "unknown path")) owns 127.0.0.1:8787")
        default:
            break
        }
    }

    func restart() async {
        switch phase {
        case .running(.agent):
            let uid = getuid()
            _ = await ProcessRunner.run("/bin/launchctl", ["kickstart", "-k", "gui/\(uid)/\(Self.agentLabel)"], timeout: 10, captureStdout: false)
        case .running(.child):
            if let c = child {
                backoff = 0.2
                childStartedAt = .distantPast
                c.terminate() // childExited respawns
            } else {
                await spawnChild()
            }
        default:
            await start()
        }
    }

    func stopForQuit() {
        quitting = true
        restartTask?.cancel()
        if let c = child, c.isRunning {
            c.terminate()
            let deadline = Date().addingTimeInterval(2)
            while c.isRunning && Date() < deadline { usleep(50_000) }
        }
    }
}
