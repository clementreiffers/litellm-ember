mod app;
mod chart;
mod tauri;

fn main() {
    leptos::mount::mount_to_body(app::App);
}
