fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("erreur au lancement de l’application");
}
