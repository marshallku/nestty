// swift-tools-version: 6.0
import PackageDescription

// Phase 0 throwaway. Validates §D3 ABI + §R7 dual-staticlib linking
// against the existing nestty-ffi from the workspace target dir.
//
// Prereq before `swift build`:
//   cargo build --release -p nestty-ffi -p nestty-term-spike
//
// Linker flag resolves `-L../../../target/release` relative to this
// `Package.swift` (i.e. workspace root's target/release).

let package = Package(
    name: "macos-renderer-spike",
    platforms: [.macOS(.v14)],
    targets: [
        .target(
            name: "CNesttyTermSpike",
            path: "Sources/CNesttyTermSpike",
            publicHeadersPath: "include",
        ),
        .executableTarget(
            name: "SpikeApp",
            dependencies: ["CNesttyTermSpike"],
            path: "Sources/SpikeApp",
            linkerSettings: [
                .unsafeFlags(["-L../../../target/release"]),
                .linkedLibrary("nestty_term_spike"),
                .linkedLibrary("nestty_ffi"),
            ],
        ),
    ],
)
