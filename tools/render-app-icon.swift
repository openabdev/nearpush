// Renders macos/Resources/AppIcon.svg into an .iconset with AppKit's SVG support (macOS 14+).
// Run on the build host from the repo root (not the laptop):
//   swift tools/render-app-icon.swift macos/Resources/AppIcon.svg build/AppIcon.iconset [preview.png]
//   iconutil -c icns build/AppIcon.iconset -o macos/Resources/AppIcon.icns
// With a third argument it also writes a preview: the icon at 1024, 128, 64 and 32 px
// on a light and a dark background.
import AppKit

let args = CommandLine.arguments
guard args.count >= 3 else {
    FileHandle.standardError.write("usage: render-app-icon.swift <svg> <out.iconset> [preview.png]\n".data(using: .utf8)!)
    exit(2)
}
guard let svg = NSImage(contentsOf: URL(fileURLWithPath: args[1])) else {
    FileHandle.standardError.write("cannot load \(args[1])\n".data(using: .utf8)!)
    exit(1)
}

func bitmap(_ width: Int, _ height: Int, _ draw: () -> Void) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    rep.size = NSSize(width: width, height: height)
    NSGraphicsContext.saveGraphicsState()
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!
    ctx.imageInterpolation = .high
    NSGraphicsContext.current = ctx
    draw()
    ctx.flushGraphics()
    NSGraphicsContext.restoreGraphicsState()
    return rep.retagging(with: .sRGB) ?? rep
}

func write(_ rep: NSBitmapImageRep, _ path: String) {
    guard let data = rep.representation(using: .png, properties: [:]) else { fatalError("png \(path)") }
    try! data.write(to: URL(fileURLWithPath: path))
}

let outDir = args[2]
try! FileManager.default.createDirectory(atPath: outDir, withIntermediateDirectories: true)
var rendered: [Int: NSBitmapImageRep] = [:]
for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let px = base * scale
        let rep = rendered[px] ?? bitmap(px, px) {
            svg.draw(in: NSRect(x: 0, y: 0, width: px, height: px), from: .zero, operation: .sourceOver, fraction: 1)
        }
        rendered[px] = rep
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        write(rep, "\(outDir)/\(name)")
    }
}

if args.count >= 4 {
    let sizes = [1024, 128, 64, 32]
    let gap = 48
    let rowH = 1024 + 2 * gap
    let width = sizes.reduce(gap) { $0 + $1 + gap }
    let backgrounds = [NSColor(srgbRed: 0.93, green: 0.93, blue: 0.94, alpha: 1),
                       NSColor(srgbRed: 0.12, green: 0.12, blue: 0.13, alpha: 1)]
    let preview = bitmap(width, rowH * backgrounds.count) {
        for (row, bg) in backgrounds.enumerated() {
            // Light row on top, dark row below (AppKit's origin is bottom-left).
            let y0 = (backgrounds.count - 1 - row) * rowH
            bg.setFill()
            NSRect(x: 0, y: y0, width: width, height: rowH).fill()
            var x = gap
            for px in sizes {
                // Each size is drawn 1:1 from its own rendering, centred on the row.
                let y = y0 + (rowH - px) / 2
                rendered[px]!.draw(in: NSRect(x: x, y: y, width: px, height: px), from: .zero, operation: .sourceOver,
                                   fraction: 1, respectFlipped: false, hints: nil)
                x += px + gap
            }
        }
    }
    write(preview, args[3])
}
