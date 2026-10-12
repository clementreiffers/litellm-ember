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

const { releaseConfig } = await import("./updater-release.mjs");
test("release embeds its version without any signing configuration", async () => {
  const base = JSON.parse(await readFile(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  assert.equal(Object.hasOwn(base, "version"), false);
  assert.deepEqual(releaseConfig("3.2.1"), { version: "3.2.1" });
  for (const version of ["", "v1.2.3", "1.2", "01.2.3", "1.2.3-beta.1", "1.2.3\n", undefined]) {
    assert.throws(() => releaseConfig(version));
  }
});
test("release publishes the universal ZIP and its checksum", () => {
  const [, github] = config.plugins.find(([name]) => name === "@semantic-release/github");
  assert.deepEqual(github.assets, ["release/Ember-*-macos-universal.zip", "release/SHA256SUMS.txt"]);
});

test("release tooling generates config and checksum without signing keys", async () => {
  const { mkdtemp, mkdir, writeFile, rm } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");
  const { fileURLToPath } = await import("node:url");
  const { execFileSync } = await import("node:child_process");
  const { createHash } = await import("node:crypto");
  const directory = await mkdtemp(join(tmpdir(), "ember-release-"));
  const script = fileURLToPath(new URL("./updater-release.mjs", import.meta.url));
  try {
    const options = { cwd: directory, env: {}, stdio: "pipe" };
    execFileSync(process.execPath, [script, "config", "1.2.3"], options);
    assert.deepEqual(JSON.parse(await readFile(join(directory, "release-config.json"))), { version: "1.2.3" });
    await mkdir(join(directory, "release"));
    const archive = "Ember-1.2.3-macos-universal.zip";
    await writeFile(join(directory, "release", archive), "fixture");
    execFileSync(process.execPath, [script, "checksums", "1.2.3"], options);
    assert.equal(await readFile(join(directory, "release/SHA256SUMS.txt"), "utf8"),
      `${createHash("sha256").update("fixture").digest("hex")}  ${archive}\n`);
    await writeFile(join(directory, "release", archive), "");
    assert.throws(() => execFileSync(process.execPath, [script, "checksums", "1.2.3"], options));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
