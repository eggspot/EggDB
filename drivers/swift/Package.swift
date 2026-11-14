// swift-tools-version:5.9
import Foundation
import PackageDescription

let xcframework = ProcessInfo.processInfo.environment["EGGDB_XCFRAMEWORK"]
let library: Target = xcframework.map { .binaryTarget(name: "CEggDB", path: $0) }
    ?? .systemLibrary(name: "CEggDB", path: "Sources/CEggDB")

let package = Package(
    name: "EggDB",
    platforms: [.macOS(.v11), .iOS(.v13)],
    products: [.library(name: "EggDB", targets: ["EggDB"])],
    targets: [
        library,
        .target(name: "EggDB", dependencies: ["CEggDB"]),
        .testTarget(name: "EggDBTests", dependencies: ["EggDB"]),
        .executableTarget(name: "EggDBBench", dependencies: ["EggDB"]),
    ]
)
