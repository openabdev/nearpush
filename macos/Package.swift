// swift-tools-version:5.10
import PackageDescription

let package = Package(
    name: "Nearpush",
    platforms: [.macOS(.v14)],
    targets: [
        // Pure logic (no UI), unit-tested with `swift test`.
        .target(name: "NearpushCore"),
        // The menu bar app. scripts/build-app.sh wraps it into Nearpush.app.
        .executableTarget(name: "Nearpush", dependencies: ["NearpushCore"]),
        .testTarget(name: "NearpushCoreTests", dependencies: ["NearpushCore", "Nearpush"]),
    ]
)
