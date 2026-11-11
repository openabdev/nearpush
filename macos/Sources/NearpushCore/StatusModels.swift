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
    /// Absent from daemons that predate the classifier.
    public var classifier: ClassifierStatus?
    /// Absent from daemons that predate the relay status.
    public var relay: RelayStatus?
    /// Sessions waiting on the user, oldest first. Absent from daemons that predate the working-card feature.
    public var pendingYourTurn: [PendingYourTurn]?
    /// Number of sessions waiting on the user. Absent from daemons that predate it.
    public var pendingCount: Int?
    public var publicUrl: String?
    public var recent: [RecentEvent]
}

/// `classifier` in `GET /status`.
public struct ClassifierStatus: Codable, Equatable, Sendable {
    public var enabled: Bool
    /// `kiro` | `openrouter` | `typesafe`.
    public var provider: String
    public var model: String
    /// `config` or `control`.
    public var source: String
    public var threshold: Double
    public var keyService: String
    /// `nil` until the first check of the selected provider's key.
    public var keyPresent: Bool?
    public var healthy: Bool
    public var lastOkMs: UInt64
    public var lastErrorMs: UInt64
}

/// `relay` in `GET /status`. `last_error_class` is a fixed class name, never a message or token.
public struct RelayStatus: Codable, Equatable, Sendable {
    public var enabled: Bool
    public var connected: Bool
    public var lastId: UInt64
    public var lastOkMs: UInt64
    public var lastErrorClass: String?
}

/// An entry of `pending_your_turn` in `GET /status`.
public struct PendingYourTurn: Codable, Equatable, Sendable {
    public var session: String
    public var ts: UInt64
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
