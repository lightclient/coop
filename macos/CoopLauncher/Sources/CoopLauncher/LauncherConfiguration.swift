import Foundation

enum LaunchMode: String, Codable, CaseIterable {
    case cargoRun
    case debugBinary
    case customExecutable

    var displayName: String {
        switch self {
        case .cargoRun:
            return "Cargo Run"
        case .debugBinary:
            return "Debug Binary"
        case .customExecutable:
            return "Custom Executable"
        }
    }
}

struct LauncherConfiguration: Codable, Equatable {
    var repoPath: String
    var launchMode: LaunchMode
    var customExecutablePath: String?
    var arguments: [String]
    var environment: [String: String]
    var traceFile: String?
    var windowTitle: String

    init(
        repoPath: String,
        launchMode: LaunchMode,
        customExecutablePath: String? = nil,
        arguments: [String] = ["chat"],
        environment: [String: String] = ["RUST_LOG": "info"],
        traceFile: String? = "traces.jsonl",
        windowTitle: String = "Coop Launcher"
    ) {
        self.repoPath = repoPath
        self.launchMode = launchMode
        self.customExecutablePath = customExecutablePath
        self.arguments = arguments
        self.environment = environment
        self.traceFile = traceFile
        self.windowTitle = windowTitle
    }

    static func `default`(repoPath: String) -> Self {
        Self(repoPath: repoPath, launchMode: .cargoRun)
    }

    var safeWindowTitle: String {
        let trimmed = windowTitle.trimmedValue
        return trimmed.isEmpty ? "Coop Launcher" : trimmed
    }

    func normalized() -> Self {
        var copy = self
        copy.repoPath = repoPath.trimmedValue

        if let customExecutablePath {
            let trimmed = customExecutablePath.trimmedValue
            copy.customExecutablePath = trimmed.isEmpty ? nil : trimmed
        }

        copy.arguments = arguments.map(\.trimmedValue).filter { !$0.isEmpty }
        copy.environment = Dictionary(
            uniqueKeysWithValues: environment.compactMap { key, value in
                let trimmedKey = key.trimmedValue
                guard !trimmedKey.isEmpty else {
                    return nil
                }
                return (trimmedKey, value)
            }
        )

        if let traceFile {
            let trimmed = traceFile.trimmedValue
            copy.traceFile = trimmed.isEmpty ? nil : trimmed
        }

        copy.windowTitle = safeWindowTitle
        return copy
    }

    private enum CodingKeys: String, CodingKey {
        case repoPath = "repo_path"
        case launchMode = "launch_mode"
        case customExecutablePath = "custom_executable_path"
        case arguments
        case environment
        case traceFile = "trace_file"
        case windowTitle = "window_title"
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        repoPath = try container.decodeIfPresent(String.self, forKey: .repoPath) ?? ""
        launchMode = try container.decodeIfPresent(LaunchMode.self, forKey: .launchMode) ?? .cargoRun
        customExecutablePath = try container.decodeIfPresent(String.self, forKey: .customExecutablePath)
        arguments = try container.decodeIfPresent([String].self, forKey: .arguments) ?? ["chat"]
        environment = try container.decodeIfPresent([String: String].self, forKey: .environment) ?? ["RUST_LOG": "info"]
        traceFile = try container.decodeIfPresent(String.self, forKey: .traceFile) ?? "traces.jsonl"
        windowTitle = try container.decodeIfPresent(String.self, forKey: .windowTitle) ?? "Coop Launcher"
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.container(keyedBy: CodingKeys.self)
        try container.encode(repoPath, forKey: .repoPath)
        try container.encode(launchMode, forKey: .launchMode)
        try container.encodeIfPresent(customExecutablePath, forKey: .customExecutablePath)
        try container.encode(arguments, forKey: .arguments)
        try container.encode(environment, forKey: .environment)
        try container.encodeIfPresent(traceFile, forKey: .traceFile)
        try container.encode(windowTitle, forKey: .windowTitle)
    }
}
