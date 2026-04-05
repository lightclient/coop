import Foundation

struct LaunchSpec {
    let executable: String
    let arguments: [String]
    let environment: [String: String]
    let currentDirectory: String
    let displayName: String

    var environmentArray: [String] {
        environment.keys.sorted().map { key in
            "\(key)=\(environment[key] ?? "")"
        }
    }

    var commandLineDisplay: String {
        ([executable] + arguments).map(shellQuote).joined(separator: " ")
    }

    static func resolve(from configuration: LauncherConfiguration, fileManager: FileManager = .default) throws -> Self {
        let normalized = configuration.normalized()

        switch normalized.launchMode {
        case .cargoRun:
            let repoPath = try resolveRepoPath(from: normalized, fileManager: fileManager)
            return Self(
                executable: "/usr/bin/env",
                arguments: ["cargo", "run", "-p", "coop-gateway", "--bin", "coop", "--"] + normalized.arguments,
                environment: buildEnvironment(from: normalized, fileManager: fileManager),
                currentDirectory: repoPath,
                displayName: normalized.launchMode.displayName
            )
        case .debugBinary:
            let repoPath = try resolveRepoPath(from: normalized, fileManager: fileManager)
            let executable = URL(fileURLWithPath: repoPath, isDirectory: true)
                .appendingPathComponent("target/debug/coop")
                .path

            guard fileManager.fileExists(atPath: executable) else {
                throw LauncherError.missingDebugBinary(executable)
            }

            return Self(
                executable: executable,
                arguments: normalized.arguments,
                environment: buildEnvironment(from: normalized, fileManager: fileManager),
                currentDirectory: repoPath,
                displayName: normalized.launchMode.displayName
            )
        case .customExecutable:
            guard let customPath = normalized.customExecutablePath?.expandedPath,
                  !customPath.trimmedValue.isEmpty else {
                throw LauncherError.missingCustomExecutable
            }

            let executable = resolveCustomExecutablePath(
                customPath,
                repoPath: normalized.repoPath,
                fileManager: fileManager
            )

            guard fileManager.fileExists(atPath: executable) else {
                throw LauncherError.customExecutableNotFound(executable)
            }

            return Self(
                executable: executable,
                arguments: normalized.arguments,
                environment: buildEnvironment(from: normalized, fileManager: fileManager),
                currentDirectory: resolveWorkingDirectory(
                    repoPath: normalized.repoPath,
                    executablePath: executable,
                    fileManager: fileManager
                ),
                displayName: normalized.launchMode.displayName
            )
        }
    }

    private static func resolveRepoPath(from configuration: LauncherConfiguration, fileManager: FileManager) throws -> String {
        let repoPath = configuration.repoPath.expandedPath
        guard !repoPath.trimmedValue.isEmpty else {
            throw LauncherError.missingRepoPath
        }

        var isDirectory: ObjCBool = false
        guard fileManager.fileExists(atPath: repoPath, isDirectory: &isDirectory), isDirectory.boolValue else {
            throw LauncherError.repoNotFound(repoPath)
        }

        let repoURL = URL(fileURLWithPath: repoPath, isDirectory: true)
        guard SupportPaths.looksLikeCoopRepo(repoURL, fileManager: fileManager) else {
            throw LauncherError.invalidRepo(repoPath)
        }

        return repoURL.standardizedFileURL.path
    }

    private static func resolveCustomExecutablePath(
        _ path: String,
        repoPath: String,
        fileManager: FileManager
    ) -> String {
        if path.hasPrefix("/") || path.hasPrefix("~") {
            return URL(fileURLWithPath: path.expandedPath).standardizedFileURL.path
        }

        let trimmedRepoPath = repoPath.trimmedValue
        if !trimmedRepoPath.isEmpty {
            return URL(fileURLWithPath: trimmedRepoPath.expandedPath, isDirectory: true)
                .appendingPathComponent(path)
                .standardizedFileURL
                .path
        }

        return URL(fileURLWithPath: fileManager.currentDirectoryPath, isDirectory: true)
            .appendingPathComponent(path)
            .standardizedFileURL
            .path
    }

    private static func resolveWorkingDirectory(
        repoPath: String,
        executablePath: String,
        fileManager: FileManager
    ) -> String {
        let trimmedRepoPath = repoPath.trimmedValue
        if !trimmedRepoPath.isEmpty {
            let expandedRepoPath = trimmedRepoPath.expandedPath
            var isDirectory: ObjCBool = false
            if fileManager.fileExists(atPath: expandedRepoPath, isDirectory: &isDirectory), isDirectory.boolValue {
                return URL(fileURLWithPath: expandedRepoPath, isDirectory: true).standardizedFileURL.path
            }
        }

        return URL(fileURLWithPath: executablePath).deletingLastPathComponent().standardizedFileURL.path
    }

    private static func buildEnvironment(
        from configuration: LauncherConfiguration,
        fileManager: FileManager
    ) -> [String: String] {
        var environment = ProcessInfo.processInfo.environment
        environment["TERM"] = "xterm-256color"
        environment["COLORTERM"] = "truecolor"

        if let lang = environment["LANG"]?.trimmedValue, !lang.isEmpty {
            environment["LANG"] = lang
        } else {
            environment["LANG"] = "en_US.UTF-8"
        }

        environment["PATH"] = buildPath(existing: environment["PATH"], fileManager: fileManager)

        let home = fileManager.homeDirectoryForCurrentUser.path
        if !home.isEmpty {
            environment["HOME"] = home
        }

        let user = NSUserName()
        if !user.isEmpty {
            environment["USER"] = user
            environment["LOGNAME"] = user
        }

        if let shell = environment["SHELL"]?.trimmedValue, !shell.isEmpty {
            environment["SHELL"] = shell
        } else {
            environment["SHELL"] = "/bin/zsh"
        }

        if let traceFile = configuration.traceFile?.trimmedValue, !traceFile.isEmpty {
            environment["COOP_TRACE_FILE"] = traceFile
        }

        for (key, value) in configuration.environment {
            let trimmedKey = key.trimmedValue
            guard !trimmedKey.isEmpty else {
                continue
            }

            let trimmedValue = value.trimmedValue
            if trimmedValue.isEmpty {
                environment.removeValue(forKey: trimmedKey)
            } else {
                environment[trimmedKey] = value
            }
        }

        return environment
    }

    private static func buildPath(existing: String?, fileManager: FileManager) -> String {
        let home = fileManager.homeDirectoryForCurrentUser.path
        let preferredEntries = [
            home.isEmpty ? nil : "\(home)/.cargo/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]

        var orderedEntries: [String] = []
        var seen = Set<String>()

        func appendEntry(_ entry: String?) {
            guard let entry, !entry.trimmedValue.isEmpty else {
                return
            }
            guard seen.insert(entry).inserted else {
                return
            }
            orderedEntries.append(entry)
        }

        preferredEntries.forEach(appendEntry)
        existing?.split(separator: ":").map(String.init).forEach(appendEntry)
        return orderedEntries.joined(separator: ":")
    }
}

enum LauncherError: LocalizedError {
    case missingRepoPath
    case repoNotFound(String)
    case invalidRepo(String)
    case missingDebugBinary(String)
    case missingCustomExecutable
    case customExecutableNotFound(String)

    var errorDescription: String? {
        switch self {
        case .missingRepoPath:
            return "Choose your Coop checkout before starting the launcher."
        case let .repoNotFound(path):
            return "The configured repo path does not exist: \(path)"
        case let .invalidRepo(path):
            return "The configured path is not a Coop checkout: \(path)"
        case let .missingDebugBinary(path):
            return "The debug binary is missing at \(path). Build Coop first or switch to Cargo Run mode."
        case .missingCustomExecutable:
            return "Custom Executable mode requires custom_executable_path in config.json."
        case let .customExecutableNotFound(path):
            return "The custom executable does not exist: \(path)"
        }
    }
}
