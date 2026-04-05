import AppKit
import OSLog
import SwiftTerm

final class LauncherViewController: NSViewController, LocalProcessTerminalViewDelegate {
    private let configStore: ConfigStore
    private let logger = Logger(subsystem: "ai.buildwithpi.coop.launcher", category: "ui")

    private var configuration = LauncherConfiguration.default(repoPath: SupportPaths.guessRepoPath())
    private var pendingRestart = false
    private var didPerformInitialLaunch = false

    private let startButton = NSButton(title: "Start", target: nil, action: nil)
    private let restartButton = NSButton(title: "Restart", target: nil, action: nil)
    private let stopButton = NSButton(title: "Stop", target: nil, action: nil)
    private let chooseRepoButton = NSButton(title: "Choose Repo…", target: nil, action: nil)
    private let editConfigButton = NSButton(title: "Edit Config", target: nil, action: nil)
    private let openSupportButton = NSButton(title: "Open Support", target: nil, action: nil)
    private let modePopup = NSPopUpButton(frame: .zero, pullsDown: false)
    private let repoLabel = NSTextField(labelWithString: "")
    private let statusLabel = NSTextField(labelWithString: "")
    private let terminalContainer = NSView()

    private var terminalView: LocalProcessTerminalView?

    init(configStore: ConfigStore) {
        self.configStore = configStore
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func loadView() {
        view = NSView()
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        buildUI()
        replaceTerminalView()
        _ = refreshConfiguration(showErrors: false)
        updateButtons()
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        performInitialLaunchIfNeeded()
    }

    @objc func startProcess(_ sender: Any?) {
        guard refreshConfiguration(showErrors: true) else {
            return
        }

        if configuration.launchMode != .customExecutable && configuration.repoPath.trimmedValue.isEmpty {
            chooseRepo(sender)
            return
        }

        guard !isProcessRunning else {
            updateStatus(
                "Coop is already running.",
                color: .secondaryLabelColor,
                toolTip: terminalView?.process.running == true ? "The current session is still active." : nil
            )
            return
        }

        do {
            let spec = try LaunchSpec.resolve(from: configuration)
            try configStore.writeGeneratedScript(for: configuration)
            replaceTerminalView()
            pendingRestart = false
            updateWindowTitle()
            updateStatus("Starting \(spec.displayName)…", color: .secondaryLabelColor, toolTip: spec.commandLineDisplay)
            logger.info("starting process: \(spec.commandLineDisplay, privacy: .public)")
            terminalView?.startProcess(
                executable: spec.executable,
                args: spec.arguments,
                environment: spec.environmentArray,
                currentDirectory: spec.currentDirectory
            )
            updateButtons()
        } catch {
            logger.error("failed to start process: \(error.localizedDescription, privacy: .public)")
            updateStatus(error.localizedDescription, color: .systemRed)
            presentErrorAlert(message: "Could not start Coop", informativeText: error.localizedDescription)
        }
    }

    @objc func restartProcess(_ sender: Any?) {
        guard isProcessRunning else {
            startProcess(sender)
            return
        }

        pendingRestart = true
        updateStatus("Restarting…", color: .secondaryLabelColor)
        terminalView?.terminate()
        updateButtons()
    }

    @objc func stopProcess(_ sender: Any?) {
        guard isProcessRunning else {
            updateStatus("Coop is not running.", color: .secondaryLabelColor)
            return
        }

        pendingRestart = false
        updateStatus("Stopping…", color: .secondaryLabelColor)
        terminalView?.terminate()
        updateButtons()
    }

    @objc func chooseRepo(_ sender: Any?) {
        let panel = NSOpenPanel()
        panel.title = "Choose your Coop checkout"
        panel.prompt = "Choose Repo"
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.resolvesAliases = true

        let currentRepoPath = configuration.repoPath.trimmedValue.expandedPath
        if !currentRepoPath.isEmpty {
            panel.directoryURL = URL(fileURLWithPath: currentRepoPath, isDirectory: true)
        }

        guard panel.runModal() == .OK, let selectedURL = panel.url else {
            if configuration.repoPath.trimmedValue.isEmpty {
                updateStatus("Choose a Coop checkout to start the launcher.", color: .systemOrange)
            }
            return
        }

        guard SupportPaths.looksLikeCoopRepo(selectedURL) else {
            presentErrorAlert(
                message: "That folder is not a Coop checkout",
                informativeText: "Pick the repository root that contains Cargo.toml and crates/coop-gateway/."
            )
            return
        }

        let wasRunning = isProcessRunning
        configuration.repoPath = selectedURL.standardizedFileURL.path

        do {
            try configStore.save(configuration)
            configuration = configuration.normalized()
            applyConfigurationToUI()
            updateStatus("Repo updated.", color: .secondaryLabelColor)

            if wasRunning {
                promptForRestart(message: "Restart Coop to use the new repo path?")
            } else {
                startProcess(nil)
            }
        } catch {
            presentErrorAlert(message: "Could not save the launcher config", informativeText: error.localizedDescription)
        }
    }

    @objc func editConfig(_ sender: Any?) {
        if !FileManager.default.fileExists(atPath: configStore.paths.configURL.path) {
            _ = try? configStore.load()
        }
        NSWorkspace.shared.open(configStore.paths.configURL)
    }

    @objc func openSupportFolder(_ sender: Any?) {
        NSWorkspace.shared.open(configStore.paths.applicationSupportURL)
    }

    func sizeChanged(source: LocalProcessTerminalView, newCols: Int, newRows: Int) {}

    func setTerminalTitle(source: LocalProcessTerminalView, title: String) {
        DispatchQueue.main.async { [weak self] in
            self?.updateWindowTitle(childTitle: title)
        }
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}

    func processTerminated(source: TerminalView, exitCode: Int32?) {
        DispatchQueue.main.async { [weak self] in
            guard let self else {
                return
            }

            let shouldRestart = pendingRestart
            pendingRestart = false

            if let exitCode {
                updateStatus("Coop exited with status \(exitCode).", color: exitCode == 0 ? .secondaryLabelColor : .systemOrange)
                logger.info("process exited with status \(exitCode, privacy: .public)")
            } else {
                updateStatus("Coop stopped.", color: .secondaryLabelColor)
                logger.info("process terminated")
            }

            updateWindowTitle()
            updateButtons()

            if shouldRestart {
                startProcess(nil)
            }
        }
    }

    @objc private func launchModeChanged(_ sender: Any?) {
        guard let selectedMode = selectedLaunchMode() else {
            return
        }

        let wasRunning = isProcessRunning
        configuration.launchMode = selectedMode

        do {
            try configStore.save(configuration)
            configuration = configuration.normalized()
            applyConfigurationToUI()

            if selectedMode == .customExecutable && configuration.customExecutablePath == nil {
                updateStatus("Custom mode selected. Set custom_executable_path in config.json.", color: .systemOrange)
                promptToEditConfigForCustomExecutable()
                return
            }

            if wasRunning {
                promptForRestart(message: "Restart Coop to apply the new launch mode?")
            } else {
                startProcess(nil)
            }
        } catch {
            presentErrorAlert(message: "Could not save the launcher config", informativeText: error.localizedDescription)
            _ = refreshConfiguration(showErrors: false)
        }
    }

    private var isProcessRunning: Bool {
        terminalView?.process.running ?? false
    }

    private func performInitialLaunchIfNeeded() {
        guard !didPerformInitialLaunch else {
            return
        }

        didPerformInitialLaunch = true

        if configuration.launchMode != .customExecutable && configuration.repoPath.trimmedValue.isEmpty {
            updateStatus("Choose a Coop checkout to start the launcher.", color: .systemOrange)
            chooseRepo(nil)
            return
        }

        startProcess(nil)
    }

    @discardableResult
    private func refreshConfiguration(showErrors: Bool) -> Bool {
        do {
            configuration = try configStore.load()
            applyConfigurationToUI()
            return true
        } catch {
            if showErrors {
                presentErrorAlert(message: "Could not load the launcher config", informativeText: error.localizedDescription)
            }
            updateStatus("Could not load config.json.", color: .systemRed)
            return false
        }
    }

    private func applyConfigurationToUI() {
        let modes = LaunchMode.allCases
        if let selectedIndex = modes.firstIndex(of: configuration.launchMode) {
            modePopup.selectItem(at: selectedIndex)
        }

        if configuration.repoPath.trimmedValue.isEmpty {
            repoLabel.stringValue = "Not configured"
            repoLabel.toolTip = nil
        } else {
            repoLabel.stringValue = configuration.repoPath.expandedPath
            repoLabel.toolTip = configuration.repoPath.expandedPath
        }

        modePopup.toolTip = configuration.launchMode.displayName
        updateWindowTitle()
        updateButtons()
    }

    private func buildUI() {
        startButton.target = self
        startButton.action = #selector(startProcess(_:))
        restartButton.target = self
        restartButton.action = #selector(restartProcess(_:))
        stopButton.target = self
        stopButton.action = #selector(stopProcess(_:))
        chooseRepoButton.target = self
        chooseRepoButton.action = #selector(chooseRepo(_:))
        editConfigButton.target = self
        editConfigButton.action = #selector(editConfig(_:))
        openSupportButton.target = self
        openSupportButton.action = #selector(openSupportFolder(_:))

        modePopup.addItems(withTitles: LaunchMode.allCases.map(\.displayName))
        modePopup.target = self
        modePopup.action = #selector(launchModeChanged(_:))

        repoLabel.cell?.lineBreakMode = .byTruncatingMiddle
        repoLabel.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
        repoLabel.textColor = .secondaryLabelColor
        repoLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)

        statusLabel.alignment = .right
        statusLabel.textColor = .secondaryLabelColor
        statusLabel.stringValue = "Idle"

        terminalContainer.translatesAutoresizingMaskIntoConstraints = false

        let modeLabel = NSTextField(labelWithString: "Mode:")
        modeLabel.textColor = .secondaryLabelColor

        let controlsStack = NSStackView(views: [
            startButton,
            restartButton,
            stopButton,
            modeLabel,
            modePopup,
            chooseRepoButton,
            editConfigButton,
            openSupportButton,
        ])
        controlsStack.orientation = .horizontal
        controlsStack.alignment = .centerY
        controlsStack.spacing = 8

        let spacer = NSView()
        spacer.translatesAutoresizingMaskIntoConstraints = false
        spacer.setContentHuggingPriority(.defaultLow, for: .horizontal)
        spacer.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        controlsStack.addArrangedSubview(spacer)
        controlsStack.addArrangedSubview(statusLabel)

        let repoTitleLabel = NSTextField(labelWithString: "Repo:")
        repoTitleLabel.textColor = .secondaryLabelColor

        let repoStack = NSStackView(views: [repoTitleLabel, repoLabel])
        repoStack.orientation = .horizontal
        repoStack.alignment = .centerY
        repoStack.spacing = 6

        let rootStack = NSStackView(views: [controlsStack, repoStack, terminalContainer])
        rootStack.orientation = .vertical
        rootStack.spacing = 10
        rootStack.edgeInsets = NSEdgeInsets(top: 12, left: 12, bottom: 12, right: 12)
        rootStack.translatesAutoresizingMaskIntoConstraints = false

        view.addSubview(rootStack)

        NSLayoutConstraint.activate([
            rootStack.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            rootStack.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            rootStack.topAnchor.constraint(equalTo: view.topAnchor),
            rootStack.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            terminalContainer.heightAnchor.constraint(greaterThanOrEqualToConstant: 480),
        ])
    }

    private func replaceTerminalView() {
        terminalContainer.subviews.forEach { $0.removeFromSuperview() }

        let terminalView = LocalProcessTerminalView(frame: .zero)
        terminalView.processDelegate = self
        terminalView.font = NSFont.monospacedSystemFont(ofSize: 13, weight: .regular)
        terminalView.translatesAutoresizingMaskIntoConstraints = false

        terminalContainer.addSubview(terminalView)
        NSLayoutConstraint.activate([
            terminalView.leadingAnchor.constraint(equalTo: terminalContainer.leadingAnchor),
            terminalView.trailingAnchor.constraint(equalTo: terminalContainer.trailingAnchor),
            terminalView.topAnchor.constraint(equalTo: terminalContainer.topAnchor),
            terminalView.bottomAnchor.constraint(equalTo: terminalContainer.bottomAnchor),
        ])

        self.terminalView = terminalView
    }

    private func selectedLaunchMode() -> LaunchMode? {
        let index = modePopup.indexOfSelectedItem
        guard index >= 0, index < LaunchMode.allCases.count else {
            return nil
        }
        return LaunchMode.allCases[index]
    }

    private func updateWindowTitle(childTitle: String? = nil) {
        let baseTitle = configuration.safeWindowTitle
        let resolvedTitle = childTitle?.trimmedValue
        if let resolvedTitle, !resolvedTitle.isEmpty {
            view.window?.title = "\(baseTitle) — \(resolvedTitle)"
        } else {
            view.window?.title = baseTitle
        }
    }

    private func updateStatus(_ text: String, color: NSColor, toolTip: String? = nil) {
        statusLabel.stringValue = text
        statusLabel.textColor = color
        statusLabel.toolTip = toolTip
    }

    private func updateButtons() {
        stopButton.isEnabled = isProcessRunning
        restartButton.isEnabled = isProcessRunning || configuration.repoPath.trimmedValue.isEmpty == false || configuration.launchMode == .customExecutable
    }

    private func promptForRestart(message: String) {
        let alert = NSAlert()
        alert.messageText = message
        alert.informativeText = "The current session keeps running until you restart it."
        alert.addButton(withTitle: "Restart")
        alert.addButton(withTitle: "Keep Running")

        if alert.runModal() == .alertFirstButtonReturn {
            restartProcess(nil)
        }
    }

    private func promptToEditConfigForCustomExecutable() {
        let alert = NSAlert()
        alert.messageText = "Custom Executable mode needs a path"
        alert.informativeText = "Set custom_executable_path in config.json, then restart the launcher or press Start again."
        alert.addButton(withTitle: "Edit Config")
        alert.addButton(withTitle: "Later")

        if alert.runModal() == .alertFirstButtonReturn {
            editConfig(nil)
        }
    }

    private func presentErrorAlert(message: String, informativeText: String) {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = message
        alert.informativeText = informativeText
        alert.runModal()
    }
}
