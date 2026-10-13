export default {
  branches: ["main"],
  tagFormat: "${version}",
  plugins: [
    ["@semantic-release/commit-analyzer", { preset: "conventionalcommits" }],
    ["@semantic-release/release-notes-generator", { preset: "conventionalcommits" }],
    ["@semantic-release/exec", {
      generateNotesCmd: "node scripts/updater-release.mjs notes ${nextRelease.version}",
      prepareCmd: "bash scripts/prepare-release.sh ${nextRelease.version}",
    }],
    ["@semantic-release/github", {
      assets: [
        "release/Ember-*-macos-universal.dmg",
        "release/Ember-*-macos-universal.app.tar.gz",
        "release/Ember-*-macos-universal.app.tar.gz.sig",
        "release/latest.json",
        "release/SHA256SUMS.txt",
      ],
      successComment: false,
      failComment: false,
      failTitle: false,
      releasedLabels: false,
    }],
  ],
};
