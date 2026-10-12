//! Validation des artefacts avant publication, avec le même vérificateur que Tauri.
use base64::{engine::general_purpose::STANDARD, Engine};
use minisign_verify::{PublicKey, Signature};
use std::{error::Error, fs};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [archive, signature, config, version] = args.as_slice() else {
        return Err("Usage: verify-update ARCHIVE SIGNATURE CONFIG VERSION".into());
    };
    let config: serde_json::Value = serde_json::from_slice(&fs::read(config)?)?;
    let key = config["plugins"]["updater"]["pubkey"]
        .as_str()
        .ok_or("Missing public key")?;
    let key = PublicKey::decode(&String::from_utf8(STANDARD.decode(key.trim())?)?)?;
    let signature = Signature::decode(&String::from_utf8(
        STANDARD.decode(fs::read_to_string(signature)?.trim())?,
    )?)?;
    key.verify(&fs::read(archive)?, &signature, false)?;
    if signature
        .trusted_comment()
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
        != Some(version.as_str())
    {
        return Err("Signature version does not match release".into());
    }
    println!("Updater artifact and signed version verified");
    Ok(())
}
