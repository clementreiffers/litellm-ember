mod litellm;
mod notify;
mod persistence;
mod service;
mod settings;
mod tray;

fn main() {
    tray::run();
}
