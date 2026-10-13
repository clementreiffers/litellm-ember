import assert from "node:assert/strict";
import { test } from "node:test";
import { readFile } from "node:fs/promises";
import { analyzeCommits } from "@semantic-release/commit-analyzer";
import { generateNotes } from "@semantic-release/release-notes-generator";
import config from "../release.config.mjs";

const [, analyzerOptions] = config.plugins.find(([name]) => name === "@semantic-release/commit-analyzer");
const [, notesOptions] = config.plugins.find(([name]) => name === "@semantic-release/release-notes-generator");

for (const lastRelease of [{}, { gitTag: "1.0.0" }]) {
  test(`release notes render ${lastRelease.gitTag ? "after a previous release" : "for the first release"}`, async () => {
    const notes = await generateNotes(notesOptions, {
      cwd: process.cwd(),
      options: { repositoryUrl: "https://github.com/clementreiffers/litellm-ember.git" },
      commits: [
        { hash: "a".repeat(40), message: "feat: add weekly spending" },
        { hash: "b".repeat(40), message: "fix: correct budget totals" },
        { hash: "c".repeat(40), message: "feat!: change settings format\n\nBREAKING CHANGE: old settings are unsupported" },
      ],
      lastRelease,
      nextRelease: { version: "2.0.0", gitTag: "2.0.0" },
      logger: { log() {} },
    });
    assert.match(notes, /2\.0\.0/);
    assert.match(notes, /add weekly spending/);
    assert.match(notes, /correct budget totals/);
    assert.match(notes, /old settings are unsupported/);
    if (lastRelease.gitTag) {
      assert.match(notes, /compare\/1\.0\.0\.\.\.2\.0\.0/);
    }
  });
}

for (const [name, messages, expected] of [
  ["fix creates a patch", ["fix: correct budget totals"], "patch"],
  ["feat creates a minor", ["feat: add weekly spending"], "minor"],
  ["bang creates a major", ["feat(settings)!: change settings format"], "major"],
  ["breaking footer creates a major", ["fix: change settings\n\nBREAKING CHANGE: old settings are unsupported"], "major"],
  ["maintenance does not release", ["docs: update README", "ci: update release tooling", "chore: update dependencies", "test: cover settings"], null],
  ["largest bump wins", ["feat: add weekly spending", "fix: correct totals"], "minor"],
  ["breaking maintenance creates a major", ["chore!: drop old macOS support", "feat: add chart"], "major"],
  ["no new commits does not release", [], null],
]) {
  test(name, async () => {
    const actual = await analyzeCommits(analyzerOptions, {
      cwd: process.cwd(),
      commits: messages.map((message, index) => ({ hash: String(index), message })),
      logger: { log() {} },
    });
    assert.equal(actual, expected);
  });
}

test("only main releases, with unprefixed stable version tags", () => {
  assert.deepEqual(config.branches, ["main"]);
  assert.equal(config.tagFormat.replace("${version}", "1.2.3"), "1.2.3");
});

const { releaseConfig, manifest, verifyUpdate } = await import("./updater-release.mjs");
const { generateKeyPairSync, createHash, sign } = await import("node:crypto");
const { publicKey, privateKey } = generateKeyPairSync("ed25519");
const keyId = Buffer.from("0102030405060708", "hex");
const publicBytes = publicKey.export({ format: "der", type: "spki" }).subarray(-32);
const updaterKey = Buffer.from(`untrusted comment: test key\n${Buffer.concat([Buffer.from("Ed"), keyId, publicBytes]).toString("base64")}\n`).toString("base64");
function signatureFor(bytes, version = "1.2.3") {
  const signature = sign(null, createHash("blake2b512").update(bytes).digest(), privateKey);
  const trusted = `timestamp:123\tfile:Ember.app.tar.gz\tversion:${version}`;
  const global = sign(null, Buffer.concat([signature, Buffer.from(trusted)]), privateKey);
  const record = Buffer.concat([Buffer.from("ED"), keyId, signature]);
  return Buffer.from(`untrusted comment: test signature\n${record.toString("base64")}\ntrusted comment: ${trusted}\n${global.toString("base64")}\n`).toString("base64");
}

test("official config uses ad hoc signing and requires a Tauri key; local config needs no secrets", async () => {
  const base = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  assert.equal(Object.hasOwn(base, "version"), false);
  assert.equal(base.plugins.updater.pubkey, "");
  const release = releaseConfig("3.2.1", updaterKey);
  assert.equal(release.version, "3.2.1");
  assert.equal(release.bundle.createUpdaterArtifacts, true);
  assert.deepEqual(release.bundle.targets, ["app", "dmg"]);
  assert.deepEqual(release.bundle.macOS, { signingIdentity: "-", hardenedRuntime: false });
  assert.equal(release.plugins.updater.requireSignedVersion, true);
  assert.equal(release.plugins.updater.pubkey, updaterKey);
  for (const version of ["", "v1.2.3", "1.2", "01.2.3", "1.2.3-beta.1", "1.2.3\n", undefined]) {
    assert.throws(() => releaseConfig(version, updaterKey));
  }
  assert.throws(() => releaseConfig("1.2.3", ""));
  assert.throws(() => releaseConfig("1.2.3", "invalid"), /TAURI_SIGNING_PUBLIC_KEY/);
});

test("release publishes DMG, signed update, manifest and checksums", () => {
  const [, github] = config.plugins.find(([name]) => name === "@semantic-release/github");
  assert.deepEqual(github.assets, [
    "release/Ember-*-macos-universal.dmg",
    "release/Ember-*-macos-universal.app.tar.gz",
    "release/Ember-*-macos-universal.app.tar.gz.sig",
    "release/latest.json",
    "release/SHA256SUMS.txt",
  ]);
});

test("manifest maps both architectures to immutable universal artifact", () => {
  const value = manifest("1.2.3", "signature\n");
  assert.equal(value.version, "1.2.3");
  assert.deepEqual(value.platforms["darwin-aarch64"], value.platforms["darwin-x86_64"]);
  assert.equal(value.platforms["darwin-aarch64"].signature, "signature");
  assert.equal(value.platforms["darwin-aarch64"].url,
    "https://github.com/clementreiffers/litellm-ember/releases/download/1.2.3/Ember-1.2.3-macos-universal.app.tar.gz");
});

test("verification rejects corrupted payloads, signatures, keys and substituted versions", () => {
  const bytes = Buffer.from("fixture");
  const signature = signatureFor(bytes);
  verifyUpdate(bytes, signature, updaterKey, "1.2.3");
  assert.throws(() => verifyUpdate(Buffer.from("corrupt"), signature, updaterKey, "1.2.3"));
  assert.throws(() => verifyUpdate(bytes, "invalid", updaterKey, "1.2.3"));
  assert.throws(() => verifyUpdate(bytes, signature, updaterKey, "1.2.4"));
  const substituted = Buffer.from(Buffer.from(signature, "base64").toString().replace("version:1.2.3", "version:1.2.4")).toString("base64");
  assert.throws(() => verifyUpdate(bytes, substituted, updaterKey, "1.2.4"));
  const wrongKey = Buffer.from(Buffer.from(updaterKey, "base64").toString().replace(Buffer.concat([Buffer.from("Ed"), keyId, publicBytes]).toString("base64"), Buffer.concat([Buffer.from("Ed"), keyId, Buffer.alloc(32)]).toString("base64"))).toString("base64");
  assert.throws(() => verifyUpdate(bytes, signature, wrongKey, "1.2.3"));
});

test("release tooling refuses missing credentials and validates all publication artifacts", async () => {
  const { mkdtemp, mkdir, writeFile, rm } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { fileURLToPath } = await import("node:url");
  const { execFileSync } = await import("node:child_process");
  const directory = await mkdtemp(join(tmpdir(), "ember-release-"));
  const script = fileURLToPath(new URL("./updater-release.mjs", import.meta.url));
  try {
    const options = { cwd: directory, env: { TAURI_SIGNING_PUBLIC_KEY: updaterKey }, stdio: "pipe" };
    const notes = execFileSync(process.execPath, [script, "notes", "1.2.3"], { ...options, env: {} }).toString();
    assert.ok(notes.includes("/1.2.3/Ember-1.2.3-macos-universal.dmg"));
    assert.ok(notes.includes("older Ember version without integrated updates"));
    assert.ok(notes.includes("not notarized by Apple"));
    assert.ok(notes.includes("Open Anyway"));
    assert.ok(notes.includes("Tauri signature"));
    assert.ok(!notes.includes("Official builds are signed and notarized"));
    assert.throws(() => execFileSync(process.execPath, [script, "config", "1.2.3"], { ...options, env: {} }));
    execFileSync(process.execPath, [script, "config", "1.2.3"], options);
    assert.deepEqual(JSON.parse(await readFile(join(directory, "release-config.json"))), releaseConfig("1.2.3", updaterKey));
    await mkdir(join(directory, "release"));
    const base = "Ember-1.2.3-macos-universal";
    const bytes = Buffer.from("fixture");
    await writeFile(join(directory, "release", `${base}.dmg`), bytes);
    await writeFile(join(directory, "release", `${base}.app.tar.gz`), bytes);
    assert.throws(() => execFileSync(process.execPath, [script, "artifacts", "1.2.3"], options));
    await writeFile(join(directory, "release", `${base}.app.tar.gz.sig`), signatureFor(bytes));
    execFileSync(process.execPath, [script, "artifacts", "1.2.3"], options);
    assert.deepEqual(JSON.parse(await readFile(join(directory, "release/latest.json"))), manifest("1.2.3", signatureFor(bytes)));
    const sums = await readFile(join(directory, "release/SHA256SUMS.txt"), "utf8");
    assert.ok(sums.includes(`${createHash("sha256").update(bytes).digest("hex")}  ${base}.app.tar.gz\n`));
    assert.equal(sums.trim().split("\n").length, 4);
    await writeFile(join(directory, "release", `${base}.dmg`), "");
    assert.throws(() => execFileSync(process.execPath, [script, "artifacts", "1.2.3"], options));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});


test("release verification accepts a real Tauri CLI signature", async () => {
  const base = new URL("../src-tauri/test-data/updater/", import.meta.url);
  verifyUpdate(
    await readFile(new URL("payload.txt", base)),
    await readFile(new URL("payload.txt.sig", base), "utf8"),
    await readFile(new URL("public-key.txt", base), "utf8"),
    "2.0.0",
  );
});

test("release preflight requires only Tauri credentials and fails before installing tools", async () => {
  const { mkdtemp, mkdir, writeFile, rm, access, copyFile } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { spawnSync } = await import("node:child_process");
  const directory = await mkdtemp(join(tmpdir(), "ember-preflight-"));
  try {
    await mkdir(join(directory, "scripts"));
    await mkdir(join(directory, "bin"));
    for (const file of ["prepare-release.sh", "updater-release.mjs"]) {
      await copyFile(new URL(file, import.meta.url), join(directory, "scripts", file));
    }
    // Arrêter le scénario valide avant toute installation réelle.
    await writeFile(join(directory, "bin/cargo"), '#!/bin/sh\ntouch tools-started\nexit 73\n', { mode: 0o700 });
    const env = {
      PATH: `${join(directory, "bin")}:${process.env.PATH}`,
      TAURI_SIGNING_PRIVATE_KEY: "test-only-placeholder",
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: "test-only-password",
      TAURI_SIGNING_PUBLIC_KEY: updaterKey,
    };
    for (const name of ["TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "TAURI_SIGNING_PUBLIC_KEY"]) {
      const result = spawnSync("/bin/bash", ["scripts/prepare-release.sh", "1.2.3"], {
        cwd: directory, env: { ...env, [name]: "" }, encoding: "utf8",
      });
      assert.equal(result.status, 1);
      assert.ok(result.stderr.includes(`Missing release configuration: ${name}`));
      await assert.rejects(access(join(directory, "tools-started")));
    }
    const invalid = spawnSync("/bin/bash", ["scripts/prepare-release.sh", "1.2.3"], {
      cwd: directory, env: { ...env, TAURI_SIGNING_PUBLIC_KEY: "invalid" }, encoding: "utf8",
    });
    assert.equal(invalid.status, 1);
    assert.match(invalid.stderr, /TAURI_SIGNING_PUBLIC_KEY/);
    await assert.rejects(access(join(directory, "tools-started")));
    const valid = spawnSync("/bin/bash", ["scripts/prepare-release.sh", "1.2.3"], {
      cwd: directory, env, encoding: "utf8",
    });
    assert.equal(valid.status, 73, valid.stderr);
    await access(join(directory, "tools-started"));
    const generated = JSON.parse(await readFile(join(directory, "release-config.json"), "utf8"));
    assert.deepEqual(generated, releaseConfig("1.2.3", updaterKey));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
