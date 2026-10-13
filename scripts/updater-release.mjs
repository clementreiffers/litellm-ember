import { createHash, createPublicKey, verify } from "node:crypto";
import { readFile, writeFile, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";

export function validateVersion(version) {
  if (typeof version !== "string" || version.trim() !== version || !/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(version)) {
    throw new Error("Expected a stable X.Y.Z version");
  }
}

function publicKeyBytes(publicKey) {
  const lines = Buffer.from(publicKey || "", "base64").toString("utf8").trim().split(/\r?\n/);
  const bytes = Buffer.from(lines[1] || "", "base64");
  if (!lines[0]?.startsWith("untrusted comment:") || bytes.length !== 42 || bytes.subarray(0, 2).toString() !== "Ed") {
    throw new Error("TAURI_SIGNING_PUBLIC_KEY must contain the base64-encoded Tauri public key file");
  }
  return bytes;
}

export function releaseConfig(version, publicKey, signingIdentity) {
  validateVersion(version);
  publicKeyBytes(publicKey);
  if (!signingIdentity?.startsWith("Developer ID Application:")) {
    throw new Error("APPLE_SIGNING_IDENTITY must be a Developer ID Application identity");
  }
  return {
    version,
    bundle: {
      targets: ["app", "dmg"],
      createUpdaterArtifacts: true,
      macOS: { signingIdentity, hardenedRuntime: true },
    },
    plugins: { updater: { pubkey: publicKey.trim(), requireSignedVersion: true } },
  };
}

// Verify both Minisign signatures, including the signed version, before publication.
export function verifyUpdate(bytes, signature, publicKey, version) {
  validateVersion(version);
  const key = publicKeyBytes(publicKey);
  const lines = Buffer.from(signature.trim(), "base64").toString("utf8").trim().split(/\r?\n/);
  const record = Buffer.from(lines[1] || "", "base64");
  if (record.length !== 74 || record.subarray(0, 2).toString() !== "ED" || !record.subarray(2, 10).equals(key.subarray(2, 10))) {
    throw new Error("Invalid updater signature or mismatched signing key");
  }
  const trusted = lines[2]?.replace(/^trusted comment: /, "");
  if (!lines[2]?.startsWith("trusted comment: ") || !trusted.split(/\s+/).includes(`version:${version}`)) {
    throw new Error("Updater signature does not bind the release version");
  }
  const cryptoKey = createPublicKey({
    key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), key.subarray(10)]),
    format: "der", type: "spki",
  });
  const rawSignature = record.subarray(10);
  if (!verify(null, createHash("blake2b512").update(bytes).digest(), cryptoKey, rawSignature)
      || !verify(null, Buffer.concat([rawSignature, Buffer.from(trusted)]), cryptoKey, Buffer.from(lines[3] || "", "base64"))) {
    throw new Error("Updater signature verification failed");
  }
}

export function manifest(version, signature) {
  validateVersion(version);
  const platform = {
    signature: signature.trim(),
    url: `https://github.com/clementreiffers/litellm-ember/releases/download/${version}/Ember-${version}-macos-universal.app.tar.gz`,
  };
  return { version, platforms: { "darwin-aarch64": platform, "darwin-x86_64": platform } };
}

async function main() {
  const [command, version] = process.argv.slice(2);
  validateVersion(version);
  if (command === "notes") {
    console.log(`### Installing and updating Ember

For a first installation, or an older Ember version without integrated updates, download [the universal macOS DMG](https://github.com/clementreiffers/litellm-ember/releases/download/${version}/Ember-${version}-macos-universal.dmg), quit Ember, and drag Ember.app to Applications. Settings and the existing Keychain entry are preserved; macOS may ask for Keychain authorization once when moving from an unsigned build.

For versions with integrated updates, choose **Mettre à jour** in Settings, then **Redémarrer** when ready. The other release assets are managed automatically by Ember. Official builds are signed and notarized; no manual quarantine removal is required.`);
  } else if (command === "config") {
    const config = releaseConfig(version, process.env.TAURI_SIGNING_PUBLIC_KEY, process.env.APPLE_SIGNING_IDENTITY);
    await writeFile("release-config.json", JSON.stringify(config, null, 2) + "\n");
  } else if (command === "artifacts") {
    const base = `Ember-${version}-macos-universal`;
    const files = [`${base}.dmg`, `${base}.app.tar.gz`, `${base}.app.tar.gz.sig`];
    for (const file of files) {
      if ((await stat(`release/${file}`)).size === 0) throw new Error(`Empty artifact: ${file}`);
    }
    const signature = await readFile(`release/${base}.app.tar.gz.sig`, "utf8");
    verifyUpdate(await readFile(`release/${base}.app.tar.gz`), signature, process.env.TAURI_SIGNING_PUBLIC_KEY, version);
    await writeFile("release/latest.json", JSON.stringify(manifest(version, signature), null, 2) + "\n");
    files.push("latest.json");
    const sums = await Promise.all(files.map(async file => {
      const hash = createHash("sha256").update(await readFile(`release/${file}`)).digest("hex");
      return `${hash}  ${file}\n`;
    }));
    await writeFile("release/SHA256SUMS.txt", sums.join(""));
  } else throw new Error("Expected notes, config or artifacts");
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
