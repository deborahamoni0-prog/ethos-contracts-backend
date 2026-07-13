// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "EthosProtocol",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "EthosProtocol", targets: ["EthosProtocol"]),
        .library(name: "TTLWidget", targets: ["TTLWidget"])
    ],
    dependencies: [],
    targets: [
        .target(
            name: "EthosProtocol",
            path: "Sources",
            exclude: ["Widget"]
        ),
        // WidgetKit extension target — deploy as a separate app extension in Xcode.
        // Requires Associated Domains (applinks:ethos-protocol.app) entitlement and
        // BGTaskSchedulerPermittedIdentifiers in Info.plist for the host app.
        .target(
            name: "TTLWidget",
            dependencies: ["EthosProtocol"],
            path: "Sources/Widget"
        ),
        .testTarget(
            name: "EthosProtocolTests",
            dependencies: ["EthosProtocol", "TTLWidget"],
            path: "Tests"
        )
    ]
)
