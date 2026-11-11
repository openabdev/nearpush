import Foundation

/// Minimal line-based edits of `~/.config/nearpush/config.toml` that keep the
/// user's comments and layout.
public enum ConfigEditor {
    public static var defaultURL: URL {
        if let dir = ProcessInfo.processInfo.environment["NEARPUSH_CONFIG_DIR"], !dir.isEmpty {
            return URL(fileURLWithPath: dir).appendingPathComponent("config.toml")
        }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config/nearpush/config.toml")
    }

    /// Section name for a `[name]` header line, ignoring a trailing comment.
    static func header(_ line: String) -> String? {
        var t = line.trimmingCharacters(in: .whitespaces)
        if let hash = t.firstIndex(of: "#") { t = String(t[..<hash]).trimmingCharacters(in: .whitespaces) }
        guard t.hasPrefix("["), t.hasSuffix("]"), !t.hasPrefix("[[") else { return nil }
        return t.dropFirst().dropLast().trimmingCharacters(in: .whitespaces)
    }

    /// The key of a `key = value` line, or nil for comments/blank lines.
    static func key(_ line: String) -> String? {
        let t = line.trimmingCharacters(in: .whitespaces)
        guard !t.hasPrefix("#"), let eq = t.firstIndex(of: "=") else { return nil }
        return t[..<eq].trimmingCharacters(in: .whitespaces)
    }

    /// `[summarizer] enabled`, defaulting to true like the daemon.
    public static func summarizerEnabled(_ text: String) -> Bool {
        var section: String?
        for line in text.components(separatedBy: "\n") {
            if let h = header(line) { section = h; continue }
            guard section == "summarizer", key(line) == "enabled" else { continue }
            var value = line[line.firstIndex(of: "=")!...].dropFirst()
            if let hash = value.firstIndex(of: "#") { value = value[..<hash] }
            return value.trimmingCharacters(in: .whitespaces) != "false"
        }
        return true
    }

    /// Sets `enabled` inside `[summarizer]`: replaces the line (keeping a trailing
    /// comment), inserts it under the header, or appends the section.
    public static func setSummarizerEnabled(_ text: String, _ on: Bool) -> String {
        var lines = text.components(separatedBy: "\n")
        let newValue = "enabled = \(on ? "true" : "false")"
        var section: String?
        var headerIndex: Int?
        for (i, line) in lines.enumerated() {
            if let h = header(line) {
                section = h
                if h == "summarizer" { headerIndex = i }
                continue
            }
            if section == "summarizer", key(line) == "enabled" {
                let indent = String(line.prefix { $0 == " " || $0 == "\t" })
                var comment = ""
                let afterEq = line[line.firstIndex(of: "=")!...]
                if let hash = afterEq.firstIndex(of: "#") { comment = "   " + line[hash...] }
                lines[i] = indent + newValue + comment
                return lines.joined(separator: "\n")
            }
        }
        if let h = headerIndex {
            lines.insert(newValue, at: h + 1)
            return lines.joined(separator: "\n")
        }
        var out = text
        if !out.isEmpty && !out.hasSuffix("\n") { out += "\n" }
        if !out.isEmpty { out += "\n" }
        return out + "[summarizer]\n" + newValue + "\n"
    }

    /// Writes via a temp file + rename: file 0600, a created directory 0700.
    public static func writeAtomically(_ text: String, to url: URL) throws {
        let fm = FileManager.default
        let dir = url.deletingLastPathComponent()
        if !fm.fileExists(atPath: dir.path) {
            try fm.createDirectory(at: dir, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        }
        let tmp = dir.appendingPathComponent(".\(url.lastPathComponent).tmp\(getpid())")
        guard fm.createFile(atPath: tmp.path, contents: Data(text.utf8), attributes: [.posixPermissions: 0o600]) else {
            throw CocoaError(.fileWriteUnknown, userInfo: [NSFilePathErrorKey: tmp.path])
        }
        if rename(tmp.path, url.path) != 0 {
            let err = errno
            try? fm.removeItem(at: tmp)
            throw POSIXError(POSIXErrorCode(rawValue: err) ?? .EIO)
        }
    }
}
