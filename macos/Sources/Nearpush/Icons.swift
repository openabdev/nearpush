import AppKit
import NearpushCore

/// Template asterisk for the menu bar, matching the page's three-bar asterisk.
enum Icons {
    private static var cache: [String: NSImage] = [:]

    static func image(for state: IconState) -> NSImage {
        let key = "\(state)"
        if let img = cache[key] { return img }
        let img = draw(state)
        cache[key] = img
        return img
    }

    private static func draw(_ state: IconState) -> NSImage {
        let size = NSSize(width: 18, height: 18)
        let img = NSImage(size: size, flipped: false) { rect in
            NSColor.black.setStroke()
            NSColor.black.setFill()
            let filled = state == .connected
            // Asterisk centred a little up-left when a badge sits bottom-right.
            let badge = state == .paused || state == .error
            let c = NSPoint(x: badge ? 7.5 : 9, y: badge ? 10.5 : 9)
            let r: CGFloat = badge ? 6.5 : 7.5
            let star = NSBezierPath()
            star.lineCapStyle = .round
            star.lineWidth = filled ? 3.2 : 1.7
            for i in 0..<3 {
                let a = CGFloat(i) * .pi / 3 + .pi / 2
                star.move(to: NSPoint(x: c.x + r * cos(a), y: c.y + r * sin(a)))
                star.line(to: NSPoint(x: c.x - r * cos(a), y: c.y - r * sin(a)))
            }
            star.stroke()
            switch state {
            case .paused:
                for x in [12.5, 15.5] as [CGFloat] {
                    NSBezierPath(roundedRect: NSRect(x: x, y: 1, width: 2, height: 6.5), xRadius: 0.8, yRadius: 0.8).fill()
                }
            case .error:
                let slash = NSBezierPath()
                slash.lineWidth = 2
                slash.lineCapStyle = .round
                slash.move(to: NSPoint(x: 2, y: 16))
                slash.line(to: NSPoint(x: 16, y: 2))
                slash.stroke()
            case .running, .connected:
                break
            }
            _ = rect
            return true
        }
        img.isTemplate = true
        return img
    }
}
