# Repository Guidelines

## Project Structure & Module Organization

Ember is a macOS menu bar application for monitoring LiteLLM spending, built as a Rust 2021 Cargo workspace.

- `src-tauri/` (`ember`): Tauri v2 backend; API requests in `litellm.rs`, configuration in `settings.rs`, tray integration in `tray.rs`, and budget notifications in `notify.rs`.
- `ui/`: Leptos frontend compiled to WebAssembly with Trunk; components live in `ui/src/`, with styles in `ui/style.css`.
- `shared/`: serializable types exchanged between backend and frontend.
- `assets/` and `src-tauri/icons/`: branding and application icons.
- CI lives in `.github/workflows/ci.yml`.

## Build, Test, and Development Commands

Develop on macOS with stable Rust. Install prerequisites:

```sh
rustup target add wasm32-unknown-unknown
cargo install tauri-cli --version "^2" --locked
cargo install trunk --locked
```

Run commands from the repository root:

- `cargo tauri dev`: launch the app and Trunk development server on port 1420.
- `cargo tauri build`: build the application bundle under `target/release/bundle/macos/`.
- `cargo test -p ember -p shared`: run the backend and shared-type tests used by CI.
- `cargo clippy -p ember -p shared --all-targets`: run CI lint checks.
- `cargo fmt --all -- --check`: check standard Rust formatting locally.

## Coding Style & Naming Conventions

Use four-space indentation and standard rustfmt conventions. Use `snake_case` for modules, functions, and variables, `PascalCase` for types and Leptos components, and `SCREAMING_SNAKE_CASE` for constants. Keep shared API types in `shared`; preserve compatibility with existing serialized settings and caches. Follow surrounding French comments and UI wording where applicable.

## Testing Guidelines

Place unit tests beside implementation in `#[cfg(test)] mod tests`, with descriptive behavior-based names. Async tests use `#[tokio::test]`; HTTP tests use `httpmock`. Add regression tests for changed behavior. No numerical coverage threshold is configured.

The optional live test runs with `cargo test -p ember live -- --ignored --nocapture`; it requires `OPENAI_API_KEY` and `LITELLM_BASE_URL`. Keep routine tests independent of credentials and real endpoints.

## Commit & Pull Request Guidelines

History uses short imperative subjects, such as “Add configurable native budget notifications”. Follow that style. Describe behavior changes, link relevant issues, report validation, and include screenshots for UI changes. Ensure macOS CI passes.

## Security & Configuration

Store API keys only in macOS Keychain; never commit credentials or expose stored keys to the frontend. Use `io.github.clementreiffers.ember` for the bundle identifier and Keychain service. Require a user-provided endpoint; do not embed a default server or migrate legacy application data automatically. Nonsecret preferences belong in the application data directory’s `settings.json`.
