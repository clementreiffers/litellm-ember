# LiteLLM Menubar

App Tauri v2 (macOS) : affiche le coût total LiteLLM dans la menu bar. Un clic ouvre un panneau avec le coût par modèle et un graphique du jour (tokens et $ par modèle). Rafraîchissement toutes les 30 s.

- Backend : Rust (`src-tauri/`), il appelle l'API LiteLLM et met à jour le titre du tray.
- Front : Rust/WASM avec Leptos + charming (`ui/`).
- Données partagées : `shared/`.

## Configuration (variables d'environnement)

| Variable | Rôle |
|---|---|
| `OPENAI_API_KEY` | Clé virtuelle LiteLLM (obligatoire). Jamais écrite dans le code ni dans les logs. |
| `LITELLM_BASE_URL` | URL de votre instance LiteLLM (obligatoire). |

Une app lancée depuis le Finder n'hérite pas des variables du shell : lancez-la depuis un terminal (`open -a "LiteLLM Menubar"` ou `cargo tauri dev`).

## Lancer

```bash
cargo install tauri-cli --version "^2" --locked
cargo install trunk --locked
cargo tauri dev            # dev
cargo tauri build          # .app dans target/release/bundle/macos
cargo test -p litellm-menubar                                   # tests unitaires
cargo test -p litellm-menubar live -- --ignored --nocapture     # appel réel (nécessite OPENAI_API_KEY et LITELLM_BASE_URL)
```
