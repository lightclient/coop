import Foundation

struct SupportPaths {
    let applicationSupportURL: URL
    let configURL: URL
    let generatedScriptURL: URL
    let logsURL: URL

    static func resolve(fileManager: FileManager = .default) throws -> Self {
        let baseURL = try fileManager.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        .appendingPathComponent("CoopLauncher", isDirectory: true)

        return Self(
            applicationSupportURL: baseURL,
            configURL: baseURL.appendingPathComponent("config.json"),
            generatedScriptURL: baseURL.appendingPathComponent("run-coop.zsh"),
            logsURL: baseURL.appendingPathComponent("logs", isDirectory: true)
        )
    }

    func ensureDirectories(fileManager: FileManager = .default) throws {
        try fileManager.createDirectory(at: applicationSupportURL, withIntermediateDirectories: true)
        try fileManager.createDirectory(at: logsURL, withIntermediateDirectories: true)
    }

    static func looksLikeCoopRepo(_ url: URL, fileManager: FileManager = .default) -> Bool {
        let cargoURL = url.appendingPathComponent("Cargo.toml")
        let gatewayURL = url.appendingPathComponent("crates/coop-gateway/Cargo.toml")
        return fileManager.fileExists(atPath: cargoURL.path) && fileManager.fileExists(atPath: gatewayURL.path)
    }

    static func guessRepoPath(fileManager: FileManager = .default) -> String {
        let homeDirectory = fileManager.homeDirectoryForCurrentUser
        var candidates: [URL] = []
        var seen = Set<String>()

        func appendCandidate(_ url: URL?) {
            guard let url else {
                return
            }
            let standardized = url.standardizedFileURL
            guard seen.insert(standardized.path).inserted else {
                return
            }
            candidates.append(standardized)
        }

        if let envPath = ProcessInfo.processInfo.environment["COOP_REPO_PATH"]?.trimmedValue,
           !envPath.isEmpty {
            appendCandidate(URL(fileURLWithPath: envPath.expandedPath, isDirectory: true))
        }

        var currentBundleURL = Bundle.main.bundleURL.standardizedFileURL
        while true {
            appendCandidate(currentBundleURL)
            let parentURL = currentBundleURL.deletingLastPathComponent()
            guard parentURL.path != currentBundleURL.path else {
                break
            }
            currentBundleURL = parentURL
        }

        appendCandidate(URL(fileURLWithPath: fileManager.currentDirectoryPath, isDirectory: true))
        appendCandidate(homeDirectory.appendingPathComponent("src/coop/browser", isDirectory: true))
        appendCandidate(homeDirectory.appendingPathComponent("src/coop", isDirectory: true))
        appendCandidate(homeDirectory.appendingPathComponent("code/coop/browser", isDirectory: true))
        appendCandidate(homeDirectory.appendingPathComponent("code/coop", isDirectory: true))

        for candidate in candidates where looksLikeCoopRepo(candidate, fileManager: fileManager) {
            return candidate.path
        }

        return ""
    }
}
