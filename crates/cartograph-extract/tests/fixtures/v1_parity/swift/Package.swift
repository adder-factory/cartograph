// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Shop",
    dependencies: [
        .package(url: "https://example.invalid/vapor/vapor.git", from: "4.0.0")
    ],
    targets: [
        .executableTarget(name: "App", dependencies: [.product(name: "Vapor", package: "vapor")]),
        .testTarget(name: "AppTests", dependencies: ["App"])
    ]
)
