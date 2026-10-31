// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "EggDB",
    products: [.library(name: "EggDB", targets: ["EggDB"])],
    targets: [
        .systemLibrary(name: "CEggDB", path: "Sources/CEggDB"),
        .target(name: "EggDB", dependencies: ["CEggDB"]),
        .testTarget(name: "EggDBTests", dependencies: ["EggDB"]),
    ]
)
