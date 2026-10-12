import { createHash } from "node:crypto";
import { readFile, writeFile, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const repository = "https://github.com/clementreiffers/litellm-ember";

export function validateVersion(version) {
  if (!/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(version)) {
    throw new Error("Expected a stable X.Y.Z version");
  }
}

export function releaseConfig(version, publicKey) {
  validateVersion(version);
  if (!publicKey?.trim()) throw new Error("Missing EMBER_UPDATER_PUBLIC_KEY");
  // Tauri encodes a minisign public-key box as base64. Never put the private key here.
  const box = Buffer.from(publicKey.trim(), "base64").toString("utf8").trim().split("\n");
  if (box.length !== 2 || !box[0].startsWith("untrusted comment:") || Buffer.from(box[1], "base64").length !== 42) {
    throw new Error("Invalid updater public key");
  }
  return {
    version,
    bundle: { createUpdaterArtifacts: true },
    plugins: { updater: { pubkey: publicKey.trim(), requireSignedVersion: true } },
  };
}

export function manifest(version, signature) {
  validateVersion(version);
  if (!signature?.trim()) throw new Error("Missing updater signature");
  const comment = Buffer.from(signature.trim(), "base64").toString("utf8").split("\n")
    .find(line => line.startsWith("trusted comment: "));
  if (!comment?.slice("trusted comment: ".length).split("\t").includes(`version:${version}`)) {
    throw new Error("Signature version does not match release");
  }
  const platform = {
    signature: signature.trim(),
    url: `${repository}/releases/download/${version}/Ember-${version}-macos-universal.app.tar.gz`,
  };
  return { version, platforms: { "darwin-aarch64": platform, "darwin-x86_64": platform } };
}

async function main() {
  const [command, version] = process.argv.slice(2);
  validateVersion(version);
  if (command === "config") {
    await writeFile("release-config.json", JSON.stringify(releaseConfig(version, process.env.EMBER_UPDATER_PUBLIC_KEY), null, 2) + "\n");
  } else if (command === "manifest") {
    const base = `Ember-${version}-macos-universal`;
    const files = [`${base}.zip`, `${base}.app.tar.gz`, `${base}.app.tar.gz.sig`];
    for (const file of files) {
      if ((await stat(`release/${file}`)).size === 0) throw new Error(`Empty artifact: ${file}`);
    }
    const signature = await readFile(`release/${base}.app.tar.gz.sig`, "utf8");
    await writeFile("release/latest.json", JSON.stringify(manifest(version, signature), null, 2) + "\n");
    files.push("latest.json");
    const sums = await Promise.all(files.map(async file => {
      const hash = createHash("sha256").update(await readFile(`release/${file}`)).digest("hex");
      return `${hash}  ${file}\n`;
    }));
    await writeFile("release/SHA256SUMS.txt", sums.join(""));
  } else throw new Error("Expected config or manifest");
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
