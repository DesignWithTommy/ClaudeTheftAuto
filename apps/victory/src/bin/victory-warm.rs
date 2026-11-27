#[cfg(not(windows))]
fn main() {
    victory_core::env_alias::apply();
    eprintln!("M1LK shader preloading requires Windows.");
}

#[cfg(windows)]
#[path = "../play_launcher.rs"]
mod windows_launcher;

#[cfg(windows)]
fn main() {
    victory_core::env_alias::apply();
    if let Err(error) = windows_launcher::run(true) {
        eprintln!("Shader preload failed\n{error:#}");
        std::process::exit(1);
    }
}
