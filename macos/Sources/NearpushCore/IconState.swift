import Foundation

/// Menu bar icon variants.
public enum IconState: Equatable, Sendable {
    /// Daemon up, no iPhone connected.
    case running
    /// At least one page (WebSocket) is connected.
    case connected
    /// Pushes paused (DND).
    case paused
    /// Daemon unreachable, external/older daemon, token problem, or supervisor failure.
    case error

    /// `supervisorFailed`: the supervisor gave up or found an external daemon.
    public static func derive(_ result: StatusResult?, supervisorFailed: Bool = false) -> IconState {
        guard !supervisorFailed, case .ok(let s)? = result else { return .error }
        if s.paused { return .paused }
        return s.viewers > 0 ? .connected : .running
    }
}
