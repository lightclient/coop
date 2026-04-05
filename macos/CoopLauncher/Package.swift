// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "CoopLauncher",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "CoopLauncher", targets: ["CoopLauncher"]),
    ],
    dependencies: [
        .package(url: "https://github.com/migueldeicaza/SwiftTerm.git", from: "1.13.0"),
    ],
    targets: [
        .executableTarget(
            name: "CoopLauncher",
            dependencies: [
                .product(name: "SwiftTerm", package: "SwiftTerm"),
            ],
            path: "Sources/CoopLauncher"
        ),
    ]
)
