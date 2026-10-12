mod app;
mod chart;
mod details;
mod settings;
mod tauri;
mod updater;

fn main() {
    leptos::mount::mount_to_body(app::App);
}
