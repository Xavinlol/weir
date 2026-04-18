const { execSync } = require("child_process");

exports.preCommit = () => {
  execSync("cargo check --quiet", { stdio: "inherit" });
};
