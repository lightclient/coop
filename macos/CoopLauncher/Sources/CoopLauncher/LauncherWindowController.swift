import AppKit

final class LauncherWindowController: NSWindowController {
    let launcherViewController: LauncherViewController

    init(configStore: ConfigStore) {
        launcherViewController = LauncherViewController(configStore: configStore)

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1200, height: 800),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )

        window.contentViewController = launcherViewController
        window.title = "Coop Launcher"
        window.minSize = NSSize(width: 900, height: 600)
        window.setFrameAutosaveName("CoopLauncherMainWindow")
        window.center()

        super.init(window: window)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }
}
