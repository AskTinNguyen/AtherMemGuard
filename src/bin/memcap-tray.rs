// The background app: no console window, lives in the tray.
#![windows_subsystem = "windows"]

fn main() {
    memcap::tray::run();
}
