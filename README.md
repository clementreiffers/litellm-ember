# LiteLLM Menubar

App Tauri v2 (macOS) : affiche le coût total LiteLLM dans la menu bar. Un clic ouvre un panneau avec le coût par modèle et un graphique du jour (tokens et $ par modèle). Rafraîchissement toutes les 30 s.

- Backend : Rust (`src-tauri/`), il appelle l'API LiteLLM et met à jour le titre du tray.
- Front : Rust/WASM avec Leptos + charming (`ui/`).
- Données partagées : `shared/`.

## Configuration

Après le changement d’identité de l’application, renseignez à nouveau votre endpoint et votre clé. Les anciennes données locales sont conservées sans être importées automatiquement ; macOS peut demander une nouvelle autorisation des notifications.

Bouton ⚙︎ du panneau : l’endpoint est vide au premier lancement et doit être renseigné avec votre clé API.

| Paramètre | Stockage |
|---|---|
| Endpoint LiteLLM | `settings.json` (dossier de données de l'app) |
| Fréquence de rafraîchissement (5 à 3600 s) | `settings.json` |
| Clé API LiteLLM | Trousseau macOS (service `io.github.clementreiffers.ember`), jamais dans un fichier |

Notifications (activables) : une notification macOS native quand le budget de la période atteint un seuil, une seule fois par seuil et par période. Par défaut 💸 à 50 % consommé et ⚠️ à 75 % (il reste 1/4) ; les deux seuils sont réglables. Le bouton « Envoyer une notification de test » déclenche la demande d'autorisation de macOS.

Sans clé dans le Trousseau, l'app n'appelle pas l'API et le demande dans le panneau. Le front ne relit jamais la clé.

## Lancer

```bash
cargo install tauri-cli --version "^2" --locked
cargo install trunk --locked
cargo tauri dev            # dev
cargo tauri build          # .app dans target/release/bundle/macos
cargo test -p litellm-menubar                                   # tests unitaires
cargo test -p litellm-menubar live -- --ignored --nocapture     # appel réel (lit OPENAI_API_KEY et LITELLM_BASE_URL, test seulement)
```
