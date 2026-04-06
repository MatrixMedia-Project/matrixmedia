// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "MatrixMediaSDK",
    platforms: [.iOS(.v16), .macOS(.v13)],
    products: [
        .library(name: "MatrixMediaSDK", targets: ["MatrixMediaSDK"]),
    ],
    dependencies: [
        .package(url: "https://github.com/livekit/client-sdk-swift", from: "2.1.0"),
    ],
    targets: [
        .target(
            name: "MatrixMediaSDK",
            dependencies: [
                .product(name: "LiveKit", package: "client-sdk-swift"),
            ],
            resources: [
                .process("Resources"),
            ]
        ),
        .testTarget(
            name: "MatrixMediaSDKTests",
            dependencies: ["MatrixMediaSDK"]
        ),
    ]
)
