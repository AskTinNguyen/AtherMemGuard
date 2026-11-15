//! The app-wide command enum used by the tray menu, the flyout, toasts, the prime hotkey and
//! the settings window. The UI never calls the engine: it sends a `Command` and the tray loop
//! carries it out (`tray.rs` has the one dispatcher).

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
    /// docs/UI.md section 6: the checkup window, on its Memory tab ("Free up memory…").
    OpenDoctor,
    /// docs/UI.md section 6: undo the last cleanup.
    UndoDoctor,
    /// docs/UI.md section 6: "Allow this time" on the "Closed {app}" toast. Commands carry no
    /// data, so the tray remembers which app the (single, keyed) toast is about.
    AllowGuarded,
    /// docs/RAMCHECK.md: "Show me how" on the RAM check's toast and checkup row. Writes the
    /// RAM report page to the data folder and opens it in the default browser.
    ShowRamReport,
    /// Empty Windows' file cache (asks for administrator permission): the tray menu and the
    /// flyout's "Windows file cache" row.
    FreeWindowsCache,
    /// The flyout's "Windows and drivers" menu: write the driver report page and open it.
    ShowDriverReport,
    /// The flyout's Unreal menu: end Unreal helpers whose editor, build or cook has exited
    /// (after a confirmation).
    EndUnrealLeftovers,
    /// "Empty the bin" on the toast that asks when a drive is critical: empty Ather MemGuard's
    /// bin on that drive (the tray remembers which one).
    EmptyDiskBin,
    /// docs/UI.md section 6: the checkup window on its Disk space tab ("Free up disk
    /// space…", the low-disk toast).
    OpenDoctorDisk,
}

/// What a flyout row menu asks for. Most are plain commands; the rest name the group or app
/// they act on, which a `Command` can't carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Run(Command),
    /// Free memory from one app group now (trim its processes).
    FreeGroup(String),
    /// Ask an app (by executable name, without `.exe`) to close, through the checkup's rules.
    CloseApp(String),
    /// Add an app to an app group in the settings.
    AddToGroup { app: String, group: String },
}

impl Command {
    /// Every command, in a fixed order (their ids).
    pub const ALL: [Command; 22] = [
        Command::FreeMemory,
        Command::Pause30,
        Command::PauseUntilResumed,
        Command::Resume,
        Command::OpenSettings,
        Command::OpenHistory,
        Command::CheckUpdates,
        Command::InstallUpdate,
        Command::ToggleStartup,
        Command::CopyAgentSetup,
        Command::Quit,
        Command::OpenReleasesPage,
        Command::OpenSettingsFile,
        Command::OpenDoctor,
        Command::UndoDoctor,
        Command::AllowGuarded,
        Command::ShowRamReport,
        Command::FreeWindowsCache,
        Command::ShowDriverReport,
        Command::EndUnrealLeftovers,
        Command::EmptyDiskBin,
        Command::OpenDoctorDisk,
    ];

    /// A small non-zero number for menu item ids and window messages.
    pub fn id(self) -> u32 {
        match self {
            Command::FreeMemory => 1,
            Command::Pause30 => 2,
            Command::PauseUntilResumed => 3,
            Command::Resume => 4,
            Command::OpenSettings => 5,
            Command::OpenHistory => 6,
            Command::CheckUpdates => 7,
            Command::InstallUpdate => 8,
            Command::ToggleStartup => 9,
            Command::CopyAgentSetup => 10,
            Command::Quit => 11,
            Command::OpenReleasesPage => 12,
            Command::OpenSettingsFile => 13,
            Command::OpenDoctor => 14,
            Command::UndoDoctor => 15,
            Command::AllowGuarded => 16,
            Command::ShowRamReport => 17,
            Command::FreeWindowsCache => 18,
            Command::ShowDriverReport => 19,
            Command::EndUnrealLeftovers => 20,
            Command::EmptyDiskBin => 21,
            Command::OpenDoctorDisk => 22,
        }
    }

    pub fn from_id(id: u32) -> Option<Command> {
        Command::ALL.iter().copied().find(|c| c.id() == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for c in Command::ALL {
            assert_ne!(c.id(), 0);
            assert_eq!(Command::from_id(c.id()), Some(c));
            seen.insert(c.id());
        }
        assert_eq!(seen.len(), 22, "every variant has its own id");
        assert_eq!(Command::OpenDoctor.id(), 14, "the Memory tab keeps the old id");
        assert_eq!(Command::from_id(22), Some(Command::OpenDoctorDisk));
        assert_eq!(Command::from_id(0), None);
        assert_eq!(Command::from_id(99), None);
    }
}
