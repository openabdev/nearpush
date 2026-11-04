import Foundation

/// Agent states the daemon knows, in the order the menu lists them.
public enum AgentState: String, CaseIterable, Codable, Sendable {
    case allow
    case yourTurn = "your_turn"
    case allSet = "all_set"
    case working

    public var label: String {
        switch self {
        case .allow: return "allow"
        case .yourTurn: return "your turn"
        case .allSet: return "all set"
        case .working: return "working"
        }
    }
}

/// `GET /status`. Keys are snake_case on the wire; decode with `DaemonClient.decoder`.
public struct DaemonStatus: Codable, Equatable, Sendable {
    public var version: String
    public var uiVersion: String?
    public var pid: Int
    public var exe: String?
    public var startedAtMs: UInt64
    public var uptimeSecs: UInt64
    public var viewers: Int
    public var lastEventId: UInt64
    public var paused: Bool
    public var filter: Filter
    public var summarizer: SummarizerStatus
    public var publicUrl: String?
    public var recent: [RecentEvent]
}

public struct Filter: Codable, Equatable, Sendable {
    /// Raw state names; unknown future names are kept so a round trip loses nothing.
    public var states: [String]
    /// `config` (config.toml) or `control` (set via /control).
    public var source: String
}

public struct SummarizerStatus: Codable, Equatable, Sendable {
    public var enabled: Bool
    public var healthy: Bool
    public var lastOkMs: UInt64
    public var lastErrorMs: UInt64
}

public struct RecentEvent: Codable, Equatable, Identifiable, Sendable {
    public var id: UInt64
    public var ts: UInt64
    public var title: String
    public var state: String?
    public var level: String?
    public var session: String?
    public var sessionLabel: String?
    public var project: String?
    public var detail: String?
    /// False when the page did not show it (filtered out or pushed while paused).
    public var shown: Bool

    /// What a click copies: `title — detail`.
    public var copyText: String {
        [title, detail].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " — ")
    }

    /// One menu line: `#12 all set — took 3m`.
    public var menuLine: String {
        let head = state.map { AgentState(rawValue: $0)?.label ?? $0 } ?? title
        let norm = { (s: String) in s.lowercased().filter { $0 != " " && $0 != "." } }
        var s = "#\(id) \(head)"
        if let d = detail, !d.isEmpty {
            s += " — \(d)"
        } else if state != nil, norm(title) != norm(head) {
            s += " — \(title)"
        }
        if !shown { s += " (hidden)" }
        return s
    }
}

/// `POST /control` response.
public struct ControlResponse: Codable, Equatable, Sendable {
    public var paused: Bool
    public var filter: Filter
}
