//! The app-wide command enum used by the tray menu, the flyout, toasts and the settings window.
//! The UI never calls the engine: it sends a `Command` and the tray loop carries it out.
//! (Part D defines it; Part E owns it from integration on.)

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Command {
    FreeMemory,
    Pause30,
    PauseUntilResumed,
    Resume,
    OpenSettings,
    OpenHistory,
    CheckUpdates,
    InstallUpdate,
    ToggleStartup,
    CopyAgentSetup,
    Quit,
    /// Not in docs/UI.md's list: the "Update failed" toast opens the GitHub releases page.
    OpenReleasesPage,
    /// Not in docs/UI.md's list: the "Settings problem" toast's "Open file" button.
    OpenSettingsFile,
}
