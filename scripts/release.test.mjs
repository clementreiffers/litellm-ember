import assert from "node:assert/strict";
import { test } from "node:test";
import { analyzeCommits } from "@semantic-release/commit-analyzer";
import config from "../release.config.mjs";

const [, analyzerOptions] = config.plugins.find(([name]) => name === "@semantic-release/commit-analyzer");

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
