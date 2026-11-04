import Foundation

public enum Tailscale {
    /// Same list as nearpushd's `TAILSCALE_CANDIDATES` (config.rs). A GUI app's
    /// PATH is minimal, so the absolute paths matter.
    public static let candidates = [
        "tailscale",
        "/usr/local/bin/tailscale",
        "/opt/homebrew/bin/tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ]

    /// First candidate that is an executable file (bare names searched on PATH).
    public static func binary() -> String? {
        let fm = FileManager.default
        let path = ProcessInfo.processInfo.environment["PATH"] ?? "/usr/bin:/bin"
        for c in candidates {
            if c.contains("/") {
                if fm.isExecutableFile(atPath: c) { return c }
            } else {
                for dir in path.split(separator: ":") {
                    let p = "\(dir)/\(c)"
                    if fm.isExecutableFile(atPath: p) { return p }
                }
            }
        }
        return nil
    }
}

/// Runs a program with a timeout. stdin is /dev/null; stderr is discarded.
public enum ProcessRunner {
    public struct Output: Sendable {
        public var status: Int32
        public var stdout: Data
        public var timedOut: Bool
    }

    /// `captureStdout: false` sends stdout to /dev/null too (used for the
    /// Keychain check, whose output is never read). `nil` if it cannot start.
    public static func run(_ path: String, _ args: [String], timeout: TimeInterval = 5, captureStdout: Bool = true) async -> Output? {
        await withCheckedContinuation { cont in
            DispatchQueue.global(qos: .utility).async {
                let p = Process()
                p.executableURL = URL(fileURLWithPath: path)
                p.arguments = args
                p.standardInput = FileHandle.nullDevice
                p.standardError = FileHandle.nullDevice
                let pipe = Pipe()
                p.standardOutput = captureStdout ? pipe : FileHandle.nullDevice
                do { try p.run() } catch { cont.resume(returning: nil); return }
                var timedOut = false
                let killer = DispatchWorkItem { if p.isRunning { timedOut = true; p.terminate() } }
                DispatchQueue.global().asyncAfter(deadline: .now() + timeout, execute: killer)
                let data = captureStdout ? pipe.fileHandleForReading.readDataToEndOfFile() : Data()
                p.waitUntilExit()
                killer.cancel()
                cont.resume(returning: Output(status: p.terminationStatus, stdout: data, timedOut: timedOut))
            }
        }
    }
}
