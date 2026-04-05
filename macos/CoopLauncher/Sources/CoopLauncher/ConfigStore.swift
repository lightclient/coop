import Foundation

final class ConfigStore {
    let paths: SupportPaths

    private let fileManager: FileManager
    private let encoder: JSONEncoder
    private let decoder: JSONDecoder

    init(fileManager: FileManager = .default) throws {
        self.fileManager = fileManager
        paths = try SupportPaths.resolve(fileManager: fileManager)
        encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        decoder = JSONDecoder()

        try paths.ensureDirectories(fileManager: fileManager)
    }

    func load() throws -> LauncherConfiguration {
        if !fileManager.fileExists(atPath: paths.configURL.path) {
            let configuration = LauncherConfiguration.default(repoPath: SupportPaths.guessRepoPath(fileManager: fileManager))
            try save(configuration)
            return configuration
        }

        let data = try Data(contentsOf: paths.configURL)
        let configuration = try decoder.decode(LauncherConfiguration.self, from: data).normalized()
        try writeGeneratedScript(for: configuration)
        return configuration
    }

    func save(_ configuration: LauncherConfiguration) throws {
        let normalized = configuration.normalized()
        let data = try encoder.encode(normalized)
        try data.write(to: paths.configURL, options: .atomic)
        try writeGeneratedScript(for: normalized)
    }

    func writeGeneratedScript(for configuration: LauncherConfiguration) throws {
        let content = generatedScriptContent(for: configuration.normalized())
        try content.write(to: paths.generatedScriptURL, atomically: true, encoding: .utf8)
        try fileManager.setAttributes([.posixPermissions: 0o755], ofItemAtPath: paths.generatedScriptURL.path)
    }

    private func generatedScriptContent(for configuration: LauncherConfiguration) -> String {
        do {
            let spec = try LaunchSpec.resolve(from: configuration, fileManager: fileManager)
            let exportLines = spec.environment.keys.sorted().compactMap { key -> String? in
                guard isValidEnvironmentKey(key) else {
                    return nil
                }
                return "export \(key)=\(shellQuote(spec.environment[key] ?? ""))"
            }
            .joined(separator: "\n")

            return """
            #!/bin/zsh
            set -euo pipefail

            cd \(shellQuote(spec.currentDirectory))
            \(exportLines)

            exec \(spec.commandLineDisplay) "$@"
            """
        } catch {
            return """
            #!/bin/zsh
            echo \(shellQuote(error.localizedDescription)) >&2
            echo "Edit \(paths.configURL.path) or choose a repo in the launcher UI." >&2
            exit 1
            """
        }
    }

    private func isValidEnvironmentKey(_ key: String) -> Bool {
        guard let first = key.unicodeScalars.first else {
            return false
        }

        let head = CharacterSet.letters.union(CharacterSet(charactersIn: "_"))
        let tail = head.union(.decimalDigits)

        guard head.contains(first) else {
            return false
        }

        return key.unicodeScalars.dropFirst().allSatisfy(tail.contains)
    }
}
