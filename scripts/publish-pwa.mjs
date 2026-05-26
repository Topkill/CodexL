#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const repoRoot = resolve(import.meta.dirname, "..");
const pwaDir = "remote/control-pwa";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  printUsage();
  process.exit(0);
}

const outDir = resolve(repoRoot, pwaDir);
if (!existsSync(outDir)) {
  fail(`PWA directory does not exist: ${outDir}`);
}

if (!args.skipBuild) {
  run("pnpm", ["run", "build:pwa"]);
}

const wranglerArgs = ["dlx", "wrangler@latest", "pages", "deploy", outDir];
if (args.commitDirty) {
  wranglerArgs.push("--commit-dirty=true");
}
if (args.projectName) {
  wranglerArgs.push("--project-name", args.projectName);
}
if (args.branch) {
  wranglerArgs.push("--branch", args.branch);
}
run("pnpm", wranglerArgs);

function parseArgs(argv) {
  const parsed = {
    branch: "",
    commitDirty: true,
    help: false,
    projectName: process.env.CODEXL_REMOTE_PWA_PAGES_PROJECT || "codexl-remote-pwa",
    skipBuild: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--":
        break;
      case "--branch":
        parsed.branch = readValue(argv, ++index, arg);
        break;
      case "--project-name":
        parsed.projectName = readValue(argv, ++index, arg);
        break;
      case "--skip-build":
        parsed.skipBuild = true;
        break;
      case "--no-commit-dirty":
        parsed.commitDirty = false;
        break;
      case "--help":
      case "-h":
        parsed.help = true;
        break;
      default:
        fail(`Unsupported argument: ${arg}`);
    }
  }
  return parsed;
}

function readValue(argv, index, flag) {
  const value = argv[index];
  if (!value || value.startsWith("--")) {
    fail(`Missing value for ${flag}`);
  }
  return value;
}

function printUsage() {
  console.log(`Usage:
  pnpm run publish -- [options]

Options:
  --project-name <name>  Cloudflare Pages project. Default: codexl-remote-pwa
  --branch <branch>      Deploy to a preview branch
  --skip-build           Deploy the existing PWA files without rebuilding
  --no-commit-dirty      Let Wrangler reject dirty worktrees
`);
}

function run(command, commandArgs) {
  const result = spawnSync(executableName(command), commandArgs, {
    cwd: repoRoot,
    env: process.env,
    stdio: "inherit",
  });
  if (result.error) {
    fail(result.error.message);
  }
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

function executableName(command) {
  if (process.platform === "win32" && command === "pnpm") {
    return "pnpm.cmd";
  }
  return command;
}

function fail(message) {
  console.error(message);
  process.exit(1);
}
