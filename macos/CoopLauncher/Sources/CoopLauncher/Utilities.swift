import Foundation

extension String {
    var trimmedValue: String {
        trimmingCharacters(in: .whitespacesAndNewlines)
    }

    var expandedPath: String {
        (self as NSString).expandingTildeInPath
    }
}

func shellQuote(_ value: String) -> String {
    "'\(value.replacingOccurrences(of: "'", with: "'\"'\"'"))'"
}
