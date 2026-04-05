import AppKit
import OSLog

final class AppDelegate: NSObject, NSApplicationDelegate {
    private let logger = Logger(subsystem: "ai.buildwithpi.coop.launcher", category: "app")

    private var windowController: LauncherWindowController?

    func applicationDidFinishLaunching(_ notification: Notification) {
        do {
            let configStore = try ConfigStore()
            let controller = LauncherWindowController(configStore: configStore)
            windowController = controller
            buildMenu()
            controller.showWindow(self)
            NSApplication.shared.activate(ignoringOtherApps: true)
            logger.info("Coop Launcher started")
        } catch {
            presentFatalError(error)
        }
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    @objc private func aboutSelected(_ sender: Any?) {
        NSApplication.shared.orderFrontStandardAboutPanel(sender)
    }

    @objc private func startSelected(_ sender: Any?) {
        windowController?.launcherViewController.startProcess(sender)
    }

    @objc private func restartSelected(_ sender: Any?) {
        windowController?.launcherViewController.restartProcess(sender)
    }

    @objc private func stopSelected(_ sender: Any?) {
        windowController?.launcherViewController.stopProcess(sender)
    }

    @objc private func chooseRepoSelected(_ sender: Any?) {
        windowController?.launcherViewController.chooseRepo(sender)
    }

    @objc private func editConfigSelected(_ sender: Any?) {
        windowController?.launcherViewController.editConfig(sender)
    }

    @objc private func openSupportSelected(_ sender: Any?) {
        windowController?.launcherViewController.openSupportFolder(sender)
    }

    private func buildMenu() {
        let mainMenu = NSMenu()

        let appMenuItem = NSMenuItem()
        let appMenu = NSMenu()
        let aboutItem = appMenu.addItem(withTitle: "About Coop Launcher", action: #selector(aboutSelected(_:)), keyEquivalent: "")
        aboutItem.target = self
        appMenu.addItem(.separator())
        let editConfigItem = appMenu.addItem(withTitle: "Edit Config…", action: #selector(editConfigSelected(_:)), keyEquivalent: ",")
        editConfigItem.target = self
        let openSupportItem = appMenu.addItem(withTitle: "Open Support Folder", action: #selector(openSupportSelected(_:)), keyEquivalent: "")
        openSupportItem.target = self
        appMenu.addItem(.separator())
        let quitTitle = "Quit \(ProcessInfo.processInfo.processName)"
        appMenu.addItem(withTitle: quitTitle, action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appMenuItem.submenu = appMenu
        mainMenu.addItem(appMenuItem)

        let fileMenuItem = NSMenuItem()
        let fileMenu = NSMenu(title: "File")
        let startItem = fileMenu.addItem(withTitle: "Start", action: #selector(startSelected(_:)), keyEquivalent: "r")
        startItem.target = self
        let restartItem = fileMenu.addItem(withTitle: "Restart", action: #selector(restartSelected(_:)), keyEquivalent: "R")
        restartItem.target = self
        let stopItem = fileMenu.addItem(withTitle: "Stop", action: #selector(stopSelected(_:)), keyEquivalent: ".")
        stopItem.target = self
        fileMenu.addItem(.separator())
        let chooseRepoItem = fileMenu.addItem(withTitle: "Choose Repo…", action: #selector(chooseRepoSelected(_:)), keyEquivalent: "o")
        chooseRepoItem.target = self
        fileMenuItem.submenu = fileMenu
        mainMenu.addItem(fileMenuItem)

        NSApplication.shared.mainMenu = mainMenu
    }

    private func presentFatalError(_ error: Error) {
        logger.error("fatal launcher error: \(error.localizedDescription, privacy: .public)")

        let alert = NSAlert()
        alert.alertStyle = .critical
        alert.messageText = "Coop Launcher could not start"
        alert.informativeText = error.localizedDescription
        alert.runModal()
        NSApplication.shared.terminate(nil)
    }
}
