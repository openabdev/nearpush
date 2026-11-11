import Foundation

/// Holds UI-facing writes while the menu bar menu is open and applies them on close.
///
/// A published write re-renders the `MenuBarExtra` menu. While the menu is open, that
/// rebuild closes it under the cursor. Writes made while open are kept per key, so only the
/// latest value per key is applied, in one batch, when the menu closes.
///
/// The close signal comes from SwiftUI's `onDisappear`. If it never fires, the hold would
/// last forever and the UI would freeze. So the hold is bounded: when the first write is held,
/// a timer of `maxHold` seconds is armed, and when it fires while the menu is still open the
/// pending batch is applied. No write is held longer than `maxHold` seconds.
public final class MenuDeferral {
    public private(set) var isOpen = false
    private var pending: [String: () -> Void] = [:]
    /// True while a fallback timer is armed for the current batch.
    private var armed = false
    /// Bumped on every flush, so a timer from an earlier batch does nothing.
    private var epoch = 0
    private let maxHold: TimeInterval
    private let schedule: (TimeInterval, @escaping () -> Void) -> Void

    public init(
        maxHold: TimeInterval = 60,
        schedule: @escaping (TimeInterval, @escaping () -> Void) -> Void = { delay, work in
            DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: work)
        }
    ) {
        self.maxHold = maxHold
        self.schedule = schedule
    }

    /// Opening the menu defers writes. Closing it applies every pending write once.
    public func setOpen(_ open: Bool) {
        isOpen = open
        if !open { flush() }
    }

    /// Runs `write` now when the menu is closed. Otherwise keeps it under `key`, replacing
    /// any earlier write with the same key, and arms the fallback timer if it is not armed.
    public func publish(_ key: String, _ write: @escaping () -> Void) {
        guard isOpen else {
            write()
            return
        }
        pending[key] = write
        armFallback()
    }

    private func armFallback() {
        guard !armed else { return }
        armed = true
        let batch = epoch
        schedule(maxHold) { [weak self] in
            guard let self, self.epoch == batch, self.isOpen else { return }
            self.flush()
        }
    }

    private func flush() {
        let writes = Array(pending.values)
        pending = [:]
        armed = false
        epoch += 1
        writes.forEach { $0() }
    }
}
