#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    victory_core::env_alias::apply();
    eprintln!("M1LK requires Windows.");
}

#[cfg(windows)]
#[path = "../play_launcher.rs"]
mod windows_launcher;

#[cfg(windows)]
fn main() {
    victory_core::env_alias::apply();
    if let Err(error) = windows_launcher::run(false) {
        windows_launcher::show_error(&format!("{error:#}"));
        std::process::exit(1);
    }
}
