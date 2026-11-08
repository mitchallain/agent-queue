import { execFile } from "node:child_process";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

const execFileAsync = promisify(execFile);

const UI_STATE_SYMBOL = Symbol.for("agent-queue/ui-state");
const MAX_SUMMARY_CHARS = 200;
const PANE_ID_RE = /^%\d+$/;

type ContentBlock = {
  type?: string;
  text?: string;
};

type BranchEntry = {
  type?: string;
  message?: {
    role?: string;
    content?: unknown;
    stopReason?: string;
    errorMessage?: string;
  };
};

function isExecutableFile(filePath: string): boolean {
  try {
    const stat = fs.statSync(filePath);
    if (!stat.isFile()) {
      return false;
    }
    fs.accessSync(filePath, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

function getMtimeMs(filePath: string): number | undefined {
  try {
    const stat = fs.statSync(filePath);
    if (!stat.isFile()) {
      return undefined;
    }
    fs.accessSync(filePath, fs.constants.X_OK);
    return stat.mtimeMs;
  } catch {
    return undefined;
  }
}

export function resolveAgentQueueBin(): string | undefined {
  // 1. $AGENT_QUEUE_BIN
  const envBin = process.env.AGENT_QUEUE_BIN?.trim();
  if (envBin && isExecutableFile(envBin)) {
    return envBin;
  }

  // 2. agent-queue on $PATH
  const pathEnv = process.env.PATH ?? "";
  for (const dir of pathEnv.split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, "agent-queue");
    if (isExecutableFile(candidate)) {
      return candidate;
    }
  }

  // 3. Newer mtime between <repo>/target/release/agent-queue and <repo>/target/debug/agent-queue
  try {
    const extDir = path.dirname(fileURLToPath(import.meta.url));
    const repoRoot = path.resolve(extDir, "..");
    const releaseBin = path.join(repoRoot, "target", "release", "agent-queue");
    const debugBin = path.join(repoRoot, "target", "debug", "agent-queue");
    const releaseMtime = getMtimeMs(releaseBin);
    const debugMtime = getMtimeMs(debugBin);

    if (releaseMtime !== undefined && debugMtime !== undefined) {
      return releaseMtime >= debugMtime ? releaseBin : debugBin;
    }
    if (releaseMtime !== undefined) {
      return releaseBin;
    }
    if (debugMtime !== undefined) {
      return debugBin;
    }
  } catch {
    // Ignore URL resolution errors
  }

  // 4. ~/.cargo/bin/agent-queue, ~/.local/bin/agent-queue
  const home = os.homedir();
  const fallbacks = [
    path.join(home, ".cargo", "bin", "agent-queue"),
    path.join(home, ".local", "bin", "agent-queue"),
  ];
  for (const candidate of fallbacks) {
    if (isExecutableFile(candidate)) {
      return candidate;
    }
  }

  return undefined;
}

export function extractFirstLine(text: string | undefined | null): string | undefined {
  if (!text) return undefined;
  for (const line of text.split(/\r?\n/)) {
    const trimmed = line.trim();
    if (trimmed.length > 0) {
      return Array.from(trimmed).slice(0, MAX_SUMMARY_CHARS).join("");
    }
  }
  return undefined;
}

export function formatSummary(
  ctx: ExtensionContext,
  rawSummary: string | undefined,
): string | undefined {
  const clean = extractFirstLine(rawSummary);
  if (!clean) return undefined;

  const sessionName = ctx.sessionManager?.getSessionName?.()?.trim();
  const combined = sessionName ? `${sessionName} - ${clean}` : clean;
  return Array.from(combined).slice(0, MAX_SUMMARY_CHARS).join("");
}

function extractMessageText(content: unknown): string | undefined {
  if (typeof content === "string") {
    return extractFirstLine(content);
  }
  if (!Array.isArray(content)) {
    return undefined;
  }
  for (const part of content) {
    if (!part || typeof part !== "object") continue;
    const block = part as ContentBlock;
    if ((block.type === "text" || block.type === undefined) && typeof block.text === "string") {
      const line = extractFirstLine(block.text);
      if (line) return line;
    }
  }
  return undefined;
}

function findLatestUnansweredUserPrompt(branch: BranchEntry[]): string | undefined {
  for (let i = branch.length - 1; i >= 0; i--) {
    const entry = branch[i];
    if (entry?.type !== "message" || !entry.message) continue;
    const role = entry.message.role;
    if (role === "assistant") {
      return undefined;
    }
    if (role === "user") {
      return extractMessageText(entry.message.content);
    }
  }
  return undefined;
}

function inspectLatestTurn(branch: BranchEntry[]): {
  userText?: string;
  assistant?: {
    stopReason?: string;
    errorMessage?: string;
    text?: string;
  };
} {
  let assistant:
    | {
        stopReason?: string;
        errorMessage?: string;
        text?: string;
      }
    | undefined;
  let userText: string | undefined;

  for (let i = branch.length - 1; i >= 0; i--) {
    const entry = branch[i];
    if (entry?.type !== "message" || !entry.message) continue;
    const msg = entry.message;

    if (msg.role === "assistant" && !assistant) {
      assistant = {
        stopReason: msg.stopReason,
        errorMessage: extractFirstLine(msg.errorMessage),
        text: extractMessageText(msg.content),
      };
    } else if (msg.role === "user") {
      userText = extractMessageText(msg.content);
      break;
    }
  }

  return { userText, assistant };
}

export default function (pi: ExtensionAPI) {
  const paneId = process.env.TMUX_PANE?.trim() ?? "";
  if (!PANE_ID_RE.test(paneId)) {
    return;
  }

  let pendingQueue: Promise<void> = Promise.resolve();
  let revision = 0;
  let sessionActive = false;
  let agentRunning = false;
  let activeDialogs = 0;
  let lastInputSummary: string | undefined;
  let activePromptSummary: string | undefined;
  let latestCtx: ExtensionContext | undefined;

  function isTuiMode(ctx: ExtensionContext): boolean {
    return ctx.mode === "tui";
  }

  function enqueue(task: () => Promise<void>): Promise<void> {
    pendingQueue = pendingQueue.then(task).catch(() => {});
    return pendingQueue;
  }

  function runAgentQueue(args: string[], guardRevision?: number): Promise<void> {
    return enqueue(async () => {
      if (guardRevision !== undefined && guardRevision !== revision) {
        return;
      }
      const bin = resolveAgentQueueBin();
      if (!bin) return;
      try {
        await execFileAsync(bin, args, { timeout: 5000 });
      } catch {
        // Ignore subprocess errors so Pi is never disrupted
      }
    });
  }

  function wrapUi(ctx: ExtensionContext): void {
    const ui = ctx.ui as Record<string | symbol, unknown> | undefined;
    if (!ui || typeof ui !== "object") return;
    if (ui[UI_STATE_SYMBOL]) return;
    ui[UI_STATE_SYMBOL] = true;

    const runWithDialog = async <T>(
      dialogTitle: string | undefined,
      invoke: () => Promise<T>,
    ): Promise<T> => {
      const currentCtx = latestCtx ?? ctx;
      activeDialogs += 1;
      revision += 1;
      const expectedRev = revision;

      const summary =
        formatSummary(currentCtx, dialogTitle || "Waiting for input") ?? "Waiting for input";
      const pushArgs = [
        "push",
        "--pane",
        paneId,
        "--stop-reason",
        "needs_input",
        "--cwd",
        currentCtx.cwd,
        "--summary",
        summary,
      ];
      void runAgentQueue(pushArgs, expectedRev);

      try {
        return await invoke();
      } finally {
        activeDialogs = Math.max(0, activeDialogs - 1);
        if (activeDialogs === 0 && sessionActive) {
          revision += 1;
          if (agentRunning) {
            const restoreArgs = [
              "clear",
              "--pane",
              paneId,
              "--running",
              "--cwd",
              currentCtx.cwd,
            ];
            const formattedActive = formatSummary(currentCtx, activePromptSummary);
            if (formattedActive) {
              restoreArgs.push("--summary", formattedActive);
            }
            void runAgentQueue(restoreArgs);
          } else {
            void runAgentQueue(["clear", "--pane", paneId]);
          }
        }
      }
    };

    const methodNames = ["confirm", "select", "input", "editor"] as const;
    for (const name of methodNames) {
      const orig = ui[name];
      if (typeof orig === "function") {
        ui[name] = function (this: unknown, ...args: unknown[]) {
          const title = typeof args[0] === "string" ? args[0] : undefined;
          return runWithDialog(title, () =>
            Promise.resolve((orig as (...a: unknown[]) => unknown).apply(this, args)),
          );
        };
      }
    }

    const origCustom = ui.custom;
    if (typeof origCustom === "function") {
      ui.custom = function (this: unknown, ...args: unknown[]) {
        const options = args[1] as { blocked?: boolean; title?: string } | undefined;
        if (options?.blocked) {
          const title = typeof options.title === "string" ? options.title : undefined;
          return runWithDialog(title, () =>
            Promise.resolve((origCustom as (...a: unknown[]) => unknown).apply(this, args)),
          );
        }
        return (origCustom as (...a: unknown[]) => unknown).apply(this, args);
      };
    }
  }

  pi.on("session_start", async (_event, ctx) => {
    if (!isTuiMode(ctx)) return;
    latestCtx = ctx;
    sessionActive = true;
    agentRunning = false;
    activeDialogs = 0;
    lastInputSummary = undefined;
    activePromptSummary = undefined;
    revision += 1;
    wrapUi(ctx);
    await runAgentQueue(["clear", "--pane", paneId, "--no-history"]);
  });

  pi.on("input", async (event, ctx) => {
    if (!isTuiMode(ctx)) return;
    latestCtx = ctx;
    wrapUi(ctx);

    const text = typeof event?.text === "string" ? event.text : undefined;
    const extracted = extractFirstLine(text);
    if (extracted) {
      lastInputSummary = extracted;
    }

    if (event?.source !== "extension") {
      revision += 1;
      await runAgentQueue(["clear", "--pane", paneId]);
    }
  });

  pi.on("agent_start", async (_event, ctx) => {
    if (!isTuiMode(ctx)) return;
    latestCtx = ctx;
    wrapUi(ctx);

    agentRunning = true;
    revision += 1;

    const branch = (ctx.sessionManager?.getBranch?.() ?? []) as BranchEntry[];
    const promptSummary = lastInputSummary ?? findLatestUnansweredUserPrompt(branch);
    activePromptSummary = promptSummary;
    lastInputSummary = undefined;

    const args = ["clear", "--pane", paneId, "--running", "--cwd", ctx.cwd];
    const formatted = formatSummary(ctx, activePromptSummary);
    if (formatted) {
      args.push("--summary", formatted);
    }
    await runAgentQueue(args);
  });

  pi.on("agent_settled", async (_event, ctx) => {
    if (!isTuiMode(ctx)) return;
    latestCtx = ctx;
    agentRunning = false;

    if (!ctx.isIdle() || ctx.hasPendingMessages() || activeDialogs > 0) {
      return;
    }

    revision += 1;
    const expectedRev = revision;

    const branch = (ctx.sessionManager?.getBranch?.() ?? []) as BranchEntry[];
    const { userText, assistant } = inspectLatestTurn(branch);

    if (!assistant || assistant.stopReason === "aborted") {
      await runAgentQueue(["clear", "--pane", paneId, "--no-history"], expectedRev);
      return;
    }

    if (assistant.stopReason === "error") {
      const rawError = assistant.errorMessage || assistant.text || userText || "Error";
      const summary = formatSummary(ctx, rawError) ?? "Error";
      await runAgentQueue(
        [
          "push",
          "--pane",
          paneId,
          "--stop-reason",
          "error",
          "--cwd",
          ctx.cwd,
          "--summary",
          summary,
        ],
        expectedRev,
      );
      return;
    }

    const rawSummary = assistant.text || userText || "Session complete";
    const summary = formatSummary(ctx, rawSummary) ?? "Session complete";
    await runAgentQueue(
      [
        "push",
        "--pane",
        paneId,
        "--stop-reason",
        "needs_input",
        "--cwd",
        ctx.cwd,
        "--summary",
        summary,
      ],
      expectedRev,
    );
  });

  pi.on("session_shutdown", async (_event, ctx) => {
    if (!isTuiMode(ctx)) return;
    sessionActive = false;
    agentRunning = false;
    activeDialogs = 0;
    revision += 1;
    await runAgentQueue(["clear", "--pane", paneId, "--no-history"]);
  });
}
