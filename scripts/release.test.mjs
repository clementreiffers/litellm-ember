import assert from "node:assert/strict";
import { test } from "node:test";
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
