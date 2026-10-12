mod litellm;
mod notify;
mod persistence;
mod service;
mod settings;
mod tray;
mod updater;

fn main() {
    tray::run();
}
