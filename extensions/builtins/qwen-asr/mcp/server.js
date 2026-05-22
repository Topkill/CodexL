#!/usr/bin/env node
import { basename, extname, isAbsolute, resolve } from "node:path";
import { readFile, stat } from "node:fs/promises";

const SERVER_NAME = "codexl-qwen-asr";
const SERVER_VERSION = "1.0.0";
const DEFAULT_GRADIO_URL = "https://qwen-qwen3-asr.ms.show/";
const DEFAULT_LANG = "Auto";
const DEFAULT_TOOL_TIMEOUT_MS = 10 * 60 * 1000;
const MAX_UPLOAD_BYTES = 250 * 1024 * 1024;

const gradioUrl = normalizeUrl(process.env.CODEXL_QWEN_ASR_GRADIO_URL || DEFAULT_GRADIO_URL);
const defaultLang = process.env.CODEXL_QWEN_ASR_LANG || DEFAULT_LANG;
const defaultReturnTimestamps = parseBoolean(process.env.CODEXL_QWEN_ASR_RETURN_TIMESTAMPS, true);
const toolTimeoutMs = parsePositiveInteger(process.env.CODEXL_QWEN_ASR_TOOL_TIMEOUT_MS, DEFAULT_TOOL_TIMEOUT_MS);

const langOptions = [
  "Auto",
  "Chinese",
  "Cantonese",
  "English",
  "Arabic",
  "German",
  "French",
  "Spanish",
  "Portuguese",
  "Indonesian",
  "Italian",
  "Korean",
  "Russian",
  "Thai",
  "Vietnamese",
  "Japanese",
  "Turkish",
  "Hindi",
  "Malay",
  "Dutch",
  "Swedish",
  "Danish",
  "Finnish",
  "Polish",
  "Czech",
  "Filipino",
  "Persian",
  "Greek",
  "Romanian",
  "Hungarian",
  "Macedonian",
];

const tools = [
  {
    name: "transcribe",
    description:
      "Transcribe an audio file with Qwen3-ASR. Pass a local file path or an HTTP(S) URL as audio_upload.",
    inputSchema: {
      type: "object",
      properties: {
        audio_upload: {
          type: "string",
          description: "Local audio file path or HTTP(S) URL to transcribe.",
        },
        lang_disp: {
          type: "string",
          enum: langOptions,
          default: defaultLang,
          description: "Language hint. Use Auto when unsure.",
        },
        return_ts: {
          type: "boolean",
          default: defaultReturnTimestamps,
          description: "Return timestamps when the upstream service supports them.",
        },
      },
      required: ["audio_upload"],
    },
  },
  {
    name: "visualize_timestamps",
    description: "Generate a timestamp visualization from existing Qwen3-ASR timestamp results.",
    inputSchema: {
      type: "object",
      properties: {
        audio_upload: {
          type: "string",
          description: "Local audio file path or HTTP(S) URL used by the transcription.",
        },
        timestamps_json: {
          description: "Timestamp JSON returned by the transcribe tool.",
        },
      },
      required: ["audio_upload", "timestamps_json"],
    },
  },
];

let inputBuffer = Buffer.alloc(0);

process.stdin.on("data", (chunk) => {
  inputBuffer = Buffer.concat([inputBuffer, chunk]);
  drainInput().catch((error) => {
    logError(error);
  });
});

process.stdin.on("end", () => {
  process.exit(0);
});

process.on("uncaughtException", (error) => {
  logError(error);
});

process.on("unhandledRejection", (error) => {
  logError(error);
});

async function drainInput() {
  while (inputBuffer.length > 0) {
    const message = readNextMessage();
    if (!message) {
      return;
    }
    await handleMessage(message);
  }
}

function readNextMessage() {
  const firstNonWhitespace = firstNonWhitespaceByte(inputBuffer);
  if (firstNonWhitespace === "{".charCodeAt(0)) {
    const newline = inputBuffer.indexOf(0x0a);
    if (newline === -1) {
      return null;
    }
    const line = inputBuffer.subarray(0, newline).toString("utf8").trim();
    inputBuffer = inputBuffer.subarray(newline + 1);
    if (!line) {
      return null;
    }
    return JSON.parse(line);
  }

  const headerEnd = inputBuffer.indexOf("\r\n\r\n");
  if (headerEnd === -1) {
    return null;
  }

  const header = inputBuffer.subarray(0, headerEnd).toString("utf8");
  const match = /^Content-Length:\s*(\d+)$/im.exec(header);
  if (!match) {
    throw new Error("MCP message is missing Content-Length");
  }

  const contentLength = Number.parseInt(match[1], 10);
  const bodyStart = headerEnd + 4;
  const bodyEnd = bodyStart + contentLength;
  if (inputBuffer.length < bodyEnd) {
    return null;
  }

  const body = inputBuffer.subarray(bodyStart, bodyEnd).toString("utf8");
  inputBuffer = inputBuffer.subarray(bodyEnd);
  return JSON.parse(body);
}

function firstNonWhitespaceByte(buffer) {
  for (const byte of buffer) {
    if (byte !== 0x20 && byte !== 0x09 && byte !== 0x0d && byte !== 0x0a) {
      return byte;
    }
  }
  return undefined;
}

async function handleMessage(message) {
  if (!message || message.jsonrpc !== "2.0" || typeof message.method !== "string") {
    return;
  }

  const hasId = Object.prototype.hasOwnProperty.call(message, "id");
  if (!hasId) {
    return;
  }

  try {
    const result = await dispatchRequest(message.method, message.params || {});
    sendMessage({ jsonrpc: "2.0", id: message.id, result });
  } catch (error) {
    sendMessage({
      jsonrpc: "2.0",
      id: message.id,
      error: jsonRpcError(error),
    });
  }
}

async function dispatchRequest(method, params) {
  switch (method) {
    case "initialize":
      return {
        protocolVersion: params?.protocolVersion || "2024-11-05",
        capabilities: { tools: { listChanged: false } },
        serverInfo: { name: SERVER_NAME, version: SERVER_VERSION },
      };
    case "ping":
      return {};
    case "tools/list":
      return { tools };
    case "tools/call":
      return callTool(params);
    case "prompts/list":
      return { prompts: [] };
    case "resources/list":
      return { resources: [] };
    default:
      throw Object.assign(new Error(`Unknown MCP method: ${method}`), { code: -32601 });
  }
}

async function callTool(params) {
  const name = String(params?.name || "");
  const args = objectValue(params?.arguments);
  if (name === "transcribe") {
    const audioUpload = await resolveAudioUpload(args.audio_upload);
    const lang = langOptions.includes(args.lang_disp) ? args.lang_disp : defaultLang;
    const returnTs = typeof args.return_ts === "boolean" ? args.return_ts : defaultReturnTimestamps;
    const result = await callGradioEndpoint("transcribe", [audioUpload, lang, returnTs]);
    return gradioTranscribeToolResult(result);
  }
  if (name === "visualize_timestamps") {
    const audioUpload = await resolveAudioUpload(args.audio_upload);
    const result = await callGradioEndpoint("visualize_timestamps", [audioUpload, args.timestamps_json]);
    return normalizeToolResult(result);
  }
  throw Object.assign(new Error(`Unknown tool: ${name}`), { code: -32602 });
}

async function resolveAudioUpload(value) {
  const input = normalizeAudioInput(value);
  if (!input) {
    throw Object.assign(new Error("audio_upload is required"), { code: -32602 });
  }
  if (isHttpUrl(input)) {
    return {
      path: input,
      url: input,
      orig_name: basename(new URL(input).pathname) || "audio",
      meta: { _type: "gradio.FileData" },
    };
  }
  return uploadLocalFile(input);
}

function normalizeAudioInput(value) {
  if (typeof value === "string") {
    return value.trim();
  }
  if (value && typeof value === "object") {
    return String(value.path || value.url || "").trim();
  }
  return "";
}

async function uploadLocalFile(inputPath) {
  const filePath = isAbsolute(inputPath) ? inputPath : resolve(process.cwd(), inputPath);
  const info = await stat(filePath).catch((error) => {
    throw new Error(`Audio file not found: ${inputPath}; ${error.message}`);
  });
  if (!info.isFile()) {
    throw new Error(`Audio upload path is not a file: ${inputPath}`);
  }
  if (info.size > MAX_UPLOAD_BYTES) {
    throw new Error(`Audio file is too large: ${info.size} bytes`);
  }

  const bytes = await readFile(filePath);
  const form = new FormData();
  form.append("files", new Blob([bytes], { type: contentTypeForPath(filePath) }), basename(filePath));

  const response = await fetch(new URL("/gradio_api/upload", gradioUrl), {
    method: "POST",
    body: form,
    signal: AbortSignal.timeout(toolTimeoutMs),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`Gradio upload failed (${response.status}): ${truncate(text)}`);
  }

  const uploaded = parseUploadResponse(text);
  if (!uploaded) {
    throw new Error(`Gradio upload returned an unsupported response: ${truncate(text)}`);
  }
  return {
    path: uploaded,
    orig_name: basename(filePath),
    mime_type: contentTypeForPath(filePath),
    meta: { _type: "gradio.FileData" },
  };
}

function parseUploadResponse(text) {
  const value = JSON.parse(text);
  if (Array.isArray(value)) {
    return String(value[0] || "").trim();
  }
  if (value && typeof value === "object") {
    if (Array.isArray(value.files) && value.files[0]) {
      return String(value.files[0]).trim();
    }
    return String(value.path || value.url || "").trim();
  }
  return "";
}

async function callGradioEndpoint(endpoint, data) {
  const response = await fetch(new URL(`/gradio_api/call/${endpoint}`, gradioUrl), {
    method: "POST",
    headers: {
      "content-type": "application/json",
    },
    body: JSON.stringify({ data }),
    signal: AbortSignal.timeout(toolTimeoutMs),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`Qwen ASR request failed (${response.status}): ${truncate(text)}`);
  }

  const eventId = JSON.parse(text)?.event_id;
  if (!eventId) {
    throw new Error(`Qwen ASR returned an unsupported response: ${truncate(text)}`);
  }

  const eventResponse = await fetch(new URL(`/gradio_api/call/${endpoint}/${eventId}`, gradioUrl), {
    method: "GET",
    signal: AbortSignal.timeout(toolTimeoutMs),
  });
  const eventText = await eventResponse.text();
  if (!eventResponse.ok) {
    throw new Error(`Qwen ASR request failed (${eventResponse.status}): ${truncate(eventText)}`);
  }
  return parseGradioEventResponse(eventText);
}

function parseGradioEventResponse(text) {
  const messages = [];
  let data = [];
  for (const line of text.split(/\r?\n/)) {
    if (line.startsWith("data:")) {
      data.push(line.slice(5).trimStart());
    } else if (!line.trim() && data.length > 0) {
      messages.push(JSON.parse(data.join("\n")));
      data = [];
    }
  }
  if (data.length > 0) {
    messages.push(JSON.parse(data.join("\n")));
  }
  if (messages.length === 0) {
    throw new Error(`Unsupported Qwen ASR response: ${truncate(text)}`);
  }
  return messages[messages.length - 1];
}

function gradioTranscribeToolResult(result) {
  const body = {
    language: Array.isArray(result) ? result[0] || "" : "",
    text: Array.isArray(result) ? result[1] || "" : "",
    timestamps: Array.isArray(result) ? result[2] ?? null : null,
  };
  return {
    content: [
      {
        type: "text",
        text: JSON.stringify(body, null, 2),
      },
    ],
  };
}

function normalizeToolResult(result) {
  if (result && typeof result === "object" && Array.isArray(result.content)) {
    return result;
  }
  return {
    content: [
      {
        type: "text",
        text: typeof result === "string" ? result : JSON.stringify(result, null, 2),
      },
    ],
  };
}

function sendMessage(message) {
  const body = JSON.stringify(message);
  process.stdout.write(`Content-Length: ${Buffer.byteLength(body, "utf8")}\r\n\r\n${body}`);
}

function jsonRpcError(error) {
  return {
    code: typeof error?.code === "number" ? error.code : -32000,
    message: error instanceof Error ? error.message : String(error),
  };
}

function objectValue(value) {
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

function normalizeUrl(value) {
  const url = new URL(value);
  return url.toString();
}

function isHttpUrl(value) {
  try {
    const url = new URL(value);
    return url.protocol === "http:" || url.protocol === "https:";
  } catch {
    return false;
  }
}

function parseBoolean(value, fallback) {
  if (value == null || String(value).trim() === "") {
    return fallback;
  }
  return ["1", "true", "yes", "on"].includes(String(value).trim().toLowerCase());
}

function parsePositiveInteger(value, fallback) {
  const parsed = Number.parseInt(String(value || ""), 10);
  return Number.isFinite(parsed) && parsed > 0 ? parsed : fallback;
}

function contentTypeForPath(filePath) {
  switch (extname(filePath).toLowerCase()) {
    case ".aac":
      return "audio/aac";
    case ".flac":
      return "audio/flac";
    case ".m4a":
    case ".mp4":
      return "audio/mp4";
    case ".mp3":
      return "audio/mpeg";
    case ".ogg":
      return "audio/ogg";
    case ".wav":
      return "audio/wav";
    case ".webm":
      return "audio/webm";
    default:
      return "application/octet-stream";
  }
}

function truncate(value) {
  const text = String(value || "").replace(/\s+/g, " ").trim();
  return text.length > 500 ? `${text.slice(0, 500)}...` : text;
}

function logError(error) {
  const message = error instanceof Error ? `${error.stack || error.message}` : String(error);
  process.stderr.write(`${message}\n`);
}
