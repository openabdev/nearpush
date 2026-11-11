import Foundation

/// The menu bar dropdown: a quick-glance menu. Everything else lives in the settings window.
public enum QuickMenuItem: Equatable, Sendable {
    /// One read-only line: daemon state plus the latest card.
    case status
    /// "Settings…" (⌘,): opens and focuses the settings window.
    case settings
    /// "Pause pushes" toggle.
    case pause
    /// "Quit Nearpush" (⌘Q), after a separator.
    case quit

    /// The item's label; nil for the status line, whose text is live.
    public var title: String? {
        switch self {
        case .status: return nil
        case .settings: return "Settings…"
        case .pause: return "Pause pushes"
        case .quit: return "Quit Nearpush"
        }
    }

    /// The ⌘-shortcut key, if any.
    public var shortcut: Character? {
        switch self {
        case .settings: return ","
        case .quit: return "q"
        case .status, .pause: return nil
        }
    }
}

public enum QuickMenu {
    /// The dropdown's items, top to bottom.
    public static let items: [QuickMenuItem] = [.status, .settings, .pause, .quit]

    /// The status line is cut to this many characters, the last one being "…".
    public static let maxStatusChars = 48

    /// `Paused`, `● your turn · nearpush / main`, `● all set`, or `● running` with no events yet.
    /// The latest card is the newest event the iPhone showed, else the newest event.
    public static func statusLine(_ s: DaemonStatus) -> String {
        if s.paused { return "Paused" }
        guard let e = s.recent.first(where: \.shown) ?? s.recent.first else { return "● running" }
        let head = e.state.map { AgentState(rawValue: $0)?.label ?? $0 } ?? e.title
        var line = "● " + head
        if let p = e.project, !p.isEmpty { line += " · " + p }
        return truncate(line)
    }

    public static func truncate(_ line: String) -> String {
        line.count > maxStatusChars ? String(line.prefix(maxStatusChars - 1)) + "…" : line
    }
}
