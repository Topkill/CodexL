#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { resolve } from "node:path";

const repoRoot = resolve(import.meta.dirname, "..");
const defaultOutDir = "dist/codex-app-web";

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  printUsage();
  process.exit(0);
}

if (!args.skipExtract) {
  const extractArgs = ["scripts/extract-codex-app-web-bundle.mjs", "--out-dir", args.outDir];
  if (args.app) extractArgs.push("--app", args.app);
  if (args.asar) extractArgs.push("--asar", args.asar);
  if (args.bridgeScript) extractArgs.push("--bridge-script", args.bridgeScript);
  if (args.pluginRuntimeScript) extractArgs.push("--plugin-runtime-script", args.pluginRuntimeScript);
  if (args.runtimeBaseUrl) extractArgs.push("--runtime-base-url", args.runtimeBaseUrl);
  if (args.runtimeDir) extractArgs.push("--runtime-dir", args.runtimeDir);
  if (args.version) extractArgs.push("--version", args.version);
  if (args.clean) extractArgs.push("--clean");
  if (args.noClean) extractArgs.push("--no-clean");
  if (args.noHeaders) extractArgs.push("--no-headers");
  if (args.noLatest) extractArgs.push("--no-latest");
  run("node", extractArgs);
}

const outDir = resolve(repoRoot, args.outDir);
if (!existsSync(outDir)) {
  fail(`Registry directory does not exist: ${outDir}`);
}

const wranglerArgs = ["dlx", "wrangler@latest", "pages", "deploy", outDir];
wranglerArgs.push("--commit-dirty=true");
if (args.projectName) {
  wranglerArgs.push("--project-name", args.projectName);
}
if (args.branch) {
  wranglerArgs.push("--branch", args.branch);
}
run("pnpm", wranglerArgs);

function parseArgs(argv) {
  const parsed = {
    app: "",
    asar: "",
    branch: "",
    bridgeScript: "",
    clean: false,
    help: false,
    noHeaders: false,
    noClean: false,
    noLatest: false,
    outDir: defaultOutDir,
    pluginRuntimeScript: "",
    projectName: process.env.CODEXL_CODEX_WEB_ASSET_PAGES_PROJECT || "codexl-codex-app-web",
    runtimeBaseUrl: "",
    runtimeDir: "",
    skipExtract: false,
    version: "",
  };

  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--":
        break;
      case "--app":
        parsed.app = readValue(argv, ++index, arg);
        break;
      case "--asar":
        parsed.asar = readValue(argv, ++index, arg);
        break;
      case "--branch":
        parsed.branch = readValue(argv, ++index, arg);
        break;
      case "--bridge-script":
        parsed.bridgeScript = readValue(argv, ++index, arg);
        break;
      case "--plugin-runtime-script":
        parsed.pluginRuntimeScript = readValue(argv, ++index, arg);
        break;
      case "--runtime-base-url":
        parsed.runtimeBaseUrl = readValue(argv, ++index, arg);
        break;
      case "--runtime-dir":
        parsed.runtimeDir = readValue(argv, ++index, arg);
        break;
      case "--out-dir":
        parsed.outDir = readValue(argv, ++index, arg);
        break;
      case "--project-name":
        parsed.projectName = readValue(argv, ++index, arg);
        break;
      case "--version":
        parsed.version = readValue(argv, ++index, arg);
        break;
      case "--clean":
        parsed.clean = true;
        break;
      case "--no-clean":
        parsed.noClean = true;
        break;
      case "--no-headers":
        parsed.noHeaders = true;
        break;
      case "--no-latest":
        parsed.noLatest = true;
        break;
      case "--skip-extract":
        parsed.skipExtract = true;
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
  pnpm run publish:codex-web -- [options]

Options:
  --project-name <name>  Cloudflare Pages project. Default: codexl-codex-app-web
  --branch <branch>      Deploy to a preview branch
  --skip-extract         Publish the existing output directory

Extraction options are forwarded:
  --app <path>
  --asar <path>
  --out-dir <path>       Default: ${defaultOutDir}
  --version <version>
  --bridge-script <path>
  --plugin-runtime-script <path>
  --runtime-base-url <url>
  --runtime-dir <path>
  --clean
  --no-clean
  --no-latest
  --no-headers
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
