mod harness;
mod history;
mod increase;
mod litellm;
mod notify;
#[cfg(target_os = "macos")]
mod panel;
mod persistence;
mod service;
mod settings;
mod tray;
mod updater;

fn main() {
    if harness::collect_if_requested() {
        return;
    }
    tray::run();
}
