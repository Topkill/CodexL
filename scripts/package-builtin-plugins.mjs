import { execFileSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const builtinPluginsDir = join(repoRoot, "extensions", "builtins");
const packageDir = join(repoRoot, "src-tauri", "builtin-plugin-packages");
const nextAiGatewaySourceDir = optionalResolveEnvPath("NEXT_AI_GATEWAY_SOURCE_DIR");
const botGatewaySourceDir = optionalResolveEnvPath("BOT_GATEWAY_SOURCE_DIR");

const plugins = [
  {
    name: "bot-gateway",
    include: ["plugin.json", "package.json", "stdio"],
    beforePackage: syncBotGateway,
  },
  {
    name: "next-ai-gateway",
    include: ["plugin.json", "package.json", "gateway"],
    beforePackage: buildNextAiGateway,
  },
];

mkdirSync(packageDir, { recursive: true });

for (const plugin of plugins) {
  const pluginDir = join(builtinPluginsDir, plugin.name);
  const manifest = readManifest(pluginDir);

  if (plugin.beforePackage) {
    plugin.beforePackage(pluginDir);
  }

  const archivePath = join(packageDir, `${manifest.id}-${manifest.version}.tar.gz`);
  writeTarGz(archivePath, pluginDir, plugin.include);

  console.log(archivePath);
}

function readManifest(pluginDir) {
  const manifestPath = join(pluginDir, "plugin.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));

  if (!manifest.id || !manifest.version) {
    throw new Error(`Built-in plugin manifest must include id and version: ${manifestPath}`);
  }

  return manifest;
}

function optionalResolveEnvPath(name) {
  const value = process.env[name];
  return value ? resolve(value) : undefined;
}

function syncBotGateway(pluginDir) {
  const outputFile = join(pluginDir, "stdio", "stdio.js");

  if (!botGatewaySourceDir) {
    reuseExistingBundleOrThrow(outputFile, "BOT_GATEWAY_SOURCE_DIR");
    return;
  }

  const sourceBundle = join(botGatewaySourceDir, "dist-bundle", "stdio", "stdio.js");
  if (!existsSync(sourceBundle)) {
    if (existsSync(outputFile)) {
      console.warn(`Bot Gateway source bundle skipped; reusing existing bundle: ${outputFile}`);
      return;
    }
    throw new Error(
      `Bot Gateway stdio bundle not found: ${sourceBundle}. Run npm run bundle:stdio in ${botGatewaySourceDir}.`,
    );
  }

  mkdirSync(join(pluginDir, "stdio"), { recursive: true });
  copyFileSync(sourceBundle, outputFile);
  patchBotGatewayBundle(outputFile);
}

function patchBotGatewayBundle(outputFile) {
  let content = readFileSync(outputFile, "utf8");
  const originalContent = content;
  content = patchBotGatewayFeishuCardActions(content, outputFile);

  const marker = `#!/usr/bin/env node\n`;
  if (!content.includes("__codexlFileURLToPath")) {
    if (!content.startsWith(marker)) {
      throw new Error(`Bot Gateway stdio bundle has an unexpected header: ${outputFile}`);
    }
    content =
      `${marker}import { fileURLToPath as __codexlFileURLToPath } from "node:url";\n` +
      `import { dirname as __codexlDirname } from "node:path";\n` +
      `const __filename = __codexlFileURLToPath(import.meta.url);\n` +
      `const __dirname = __codexlDirname(__filename);\n` +
      content.slice(marker.length);
  }

  if (content !== originalContent) {
    writeFileSync(outputFile, content);
  }
}

function patchBotGatewayFeishuCardActions(content, outputFile) {
  if (content.includes("disabled: action.disabled === true ? true : void 0")) {
    return content;
  }
  const marker = `        url: action.url,\n        value: action.value ? { value: action.value } : void 0`;
  if (!content.includes(marker)) {
    throw new Error(`Bot Gateway Feishu card action renderer has an unexpected shape: ${outputFile}`);
  }
  return content.replace(
    marker,
    `        url: action.url,\n        disabled: action.disabled === true ? true : void 0,\n        value: action.value ? { value: action.value } : void 0`,
  );
}

function buildNextAiGateway(pluginDir) {
  const outputFile = join(pluginDir, "gateway", "index.cjs");

  if (!nextAiGatewaySourceDir) {
    reuseExistingBundleOrThrow(outputFile, "NEXT_AI_GATEWAY_SOURCE_DIR");
    patchNextAiGatewayBundle(outputFile);
    return;
  }

  const entryPoint = join(nextAiGatewaySourceDir, "src", "index.ts");
  const esbuild = join(
    nextAiGatewaySourceDir,
    "node_modules",
    ".bin",
    process.platform === "win32" ? "esbuild.cmd" : "esbuild",
  );

  if (!existsSync(entryPoint) || !existsSync(esbuild)) {
    if (existsSync(outputFile)) {
      console.warn(`NeXT AI gateway source build skipped; reusing existing bundle: ${outputFile}`);
      return;
    }
    if (!existsSync(entryPoint)) {
      throw new Error(`NeXT AI gateway entry not found: ${entryPoint}`);
    }
    throw new Error(
      `NeXT AI gateway esbuild binary not found: ${esbuild}. Run npm install in ${nextAiGatewaySourceDir}.`,
    );
  }

  mkdirSync(join(pluginDir, "gateway"), { recursive: true });
  rmSync(join(pluginDir, "gateway", "index.js"), { force: true });
  execFileSync(
    esbuild,
    [
      entryPoint,
      "--bundle",
      "--platform=node",
      "--target=node20",
      "--minify",
      "--log-level=warning",
      `--outfile=${outputFile}`,
    ],
    {
      cwd: nextAiGatewaySourceDir,
      stdio: "inherit",
    },
  );
  patchNextAiGatewayBundle(outputFile);
}

function patchNextAiGatewayBundle(outputFile) {
  let content = readFileSync(outputFile, "utf8");
  const enabledPatchedMarker = `if(!Xe(r))continue;if(ko(r.enabled)===!1)continue;let s=r,i=zwt(s.transport)||"stdio"`;
  if (!content.includes(enabledPatchedMarker)) {
    const marker = `if(!Xe(r))continue;let s=r,i=zwt(s.transport)||"stdio"`;
    if (!content.includes(marker)) {
      throw new Error(`NeXT AI Gateway MCP server parser has an unexpected shape: ${outputFile}`);
    }
    content = content.replace(marker, enabledPatchedMarker);
  }

  const stdioModePatchedMarker = `Hwt(s.stdioMessageMode)||"newline-json"`;
  if (!content.includes(stdioModePatchedMarker)) {
    const marker = `Hwt(s.stdioMessageMode)||"content-length"`;
    if (!content.includes(marker)) {
      throw new Error(`NeXT AI Gateway stdio message mode parser has an unexpected shape: ${outputFile}`);
    }
    content = content.replace(marker, stdioModePatchedMarker);
  }

  if (!content.includes(enabledPatchedMarker) || !content.includes(stdioModePatchedMarker)) {
    throw new Error(`NeXT AI Gateway MCP server parser has an unexpected shape: ${outputFile}`);
  }
  writeFileSync(outputFile, content);
}

function reuseExistingBundleOrThrow(outputFile, envName) {
  if (existsSync(outputFile)) {
    console.warn(`${envName} is not set; reusing existing bundle: ${outputFile}`);
    return;
  }
  throw new Error(`${envName} is not set and no existing bundle was found: ${outputFile}`);
}

function writeTarGz(archivePath, rootDir, includeEntries) {
  const chunks = [];
  for (const entry of includeEntries) {
    addTarPath(chunks, rootDir, normalizeTarPath(entry));
  }
  chunks.push(Buffer.alloc(1024));
  writeFileSync(archivePath, gzipSync(Buffer.concat(chunks), { level: 9 }));
}

function addTarPath(chunks, rootDir, relativePath) {
  const fullPath = join(rootDir, relativePath);
  const stats = statSync(fullPath);
  const tarPath = normalizeTarPath(relativePath);

  if (stats.isDirectory()) {
    const directoryPath = tarPath.endsWith("/") ? tarPath : `${tarPath}/`;
    chunks.push(tarHeader(directoryPath, 0, "5", 0o755, stats.mtimeMs));
    const children = readdirSync(fullPath).sort((left, right) => left.localeCompare(right));
    for (const child of children) {
      addTarPath(chunks, rootDir, `${tarPath}/${child}`);
    }
    return;
  }

  if (!stats.isFile()) {
    throw new Error(`Unsupported built-in plugin package entry: ${fullPath}`);
  }

  const content = readFileSync(fullPath);
  chunks.push(tarHeader(tarPath, content.length, "0", stats.mode & 0o777, stats.mtimeMs));
  chunks.push(content);
  chunks.push(Buffer.alloc(pad512(content.length)));
}

function tarHeader(name, size, typeflag, mode, mtimeMs) {
  const header = Buffer.alloc(512);
  const encodedName = Buffer.from(name, "utf8");
  if (encodedName.length > 100) {
    throw new Error(`Built-in plugin package path is too long for ustar header: ${name}`);
  }

  writeString(header, name, 0, 100);
  writeOctal(header, mode, 100, 8);
  writeOctal(header, 0, 108, 8);
  writeOctal(header, 0, 116, 8);
  writeOctal(header, size, 124, 12);
  writeOctal(header, Math.floor(mtimeMs / 1000), 136, 12);
  header.fill(0x20, 148, 156);
  header[156] = typeflag.charCodeAt(0);
  writeString(header, "ustar", 257, 6);
  writeString(header, "00", 263, 2);
  writeString(header, "codexl", 265, 32);
  writeString(header, "codexl", 297, 32);

  let checksum = 0;
  for (const byte of header) {
    checksum += byte;
  }
  writeChecksum(header, checksum);
  return header;
}

function writeString(buffer, value, offset, length) {
  const bytes = Buffer.from(value, "utf8");
  if (bytes.length > length) {
    throw new Error(`tar header field is too long: ${value}`);
  }
  bytes.copy(buffer, offset);
}

function writeOctal(buffer, value, offset, length) {
  const text = Math.trunc(value).toString(8).padStart(length - 1, "0");
  buffer.write(text.slice(-(length - 1)), offset, length - 1, "ascii");
  buffer[offset + length - 1] = 0;
}

function writeChecksum(buffer, checksum) {
  const text = checksum.toString(8).padStart(6, "0");
  buffer.write(text.slice(-6), 148, 6, "ascii");
  buffer[154] = 0;
  buffer[155] = 0x20;
}

function pad512(size) {
  const remainder = size % 512;
  return remainder === 0 ? 0 : 512 - remainder;
}

function normalizeTarPath(value) {
  const normalized = value
    .split(/[\\/]+/)
    .filter(Boolean)
    .join("/");
  if (!normalized || normalized === "." || normalized.startsWith("../") || normalized.includes("/../")) {
    throw new Error(`Unsafe built-in plugin package path: ${value}`);
  }
  return normalized;
}
