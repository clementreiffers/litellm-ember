mod app;
mod chart;
mod daily_view;
mod details;
mod harness;
mod history;
mod settings;
mod tauri;
mod updater;
mod usage;

fn main() {
    leptos::mount::mount_to_body(app::App);
}
