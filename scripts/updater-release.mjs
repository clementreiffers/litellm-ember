import { createHash } from "node:crypto";
import { readFile, writeFile, stat } from "node:fs/promises";
import { pathToFileURL } from "node:url";


export function validateVersion(version) {
  if (!/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/.test(version)) {
    throw new Error("Expected a stable X.Y.Z version");
  }
}

export function releaseConfig(version) {
  validateVersion(version);
  return { version };
}

async function main() {
  const [command, version] = process.argv.slice(2);
  validateVersion(version);
  if (command === "config") {
    await writeFile("release-config.json", JSON.stringify(releaseConfig(version), null, 2) + "\n");
  } else if (command === "checksums") {
    const base = `Ember-${version}-macos-universal`;
    const files = [`${base}.zip`];
    for (const file of files) {
      if ((await stat(`release/${file}`)).size === 0) throw new Error(`Empty artifact: ${file}`);
    }
    const sums = await Promise.all(files.map(async file => {
      const hash = createHash("sha256").update(await readFile(`release/${file}`)).digest("hex");
      return `${hash}  ${file}\n`;
    }));
    await writeFile("release/SHA256SUMS.txt", sums.join(""));
  } else throw new Error("Expected config or checksums");
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
