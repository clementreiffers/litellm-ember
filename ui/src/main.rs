mod app;
mod chart;
mod details;
mod settings;
mod tauri;

fn main() {
    leptos::mount::mount_to_body(app::App);
}
