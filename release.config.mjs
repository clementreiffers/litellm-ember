export default {
  branches: ["main"],
  tagFormat: "${version}",
  plugins: [
    ["@semantic-release/commit-analyzer", { preset: "conventionalcommits" }],
    ["@semantic-release/release-notes-generator", { preset: "conventionalcommits" }],
    ["@semantic-release/exec", {
      prepareCmd: "bash scripts/prepare-release.sh ${nextRelease.version}",
    }],
    ["@semantic-release/github", {
      assets: ["release/Ember-*-macos-universal.zip", "release/SHA256SUMS.txt"],
      successComment: false,
      failComment: false,
      failTitle: false,
      releasedLabels: false,
    }],
  ],
};
