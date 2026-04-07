import { mkdtemp, rm } from "node:fs/promises";
import { existsSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";
import { spawn, type ChildProcessByStdio } from "node:child_process";
import type { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

const repoRoot =
  process.env.OPENYAK_REPO_ROOT ??
  path.resolve(fileURLToPath(new URL("../../../../", import.meta.url)));
const rustRoot = path.join(repoRoot, "rust");

interface ProcessCommand {
  command: string;
  args: string[];
}

type ManagedChildProcess = ChildProcessByStdio<null, Readable, Readable>;

interface ManagedProcess {
  child: ManagedChildProcess;
  close(): Promise<void>;
}

export interface MockAnthropicHarness extends ManagedProcess {
  baseUrl: string;
}

export interface OpenyakServerHarness extends ManagedProcess {
  baseUrl: string;
  workspace: string;
}

export async function startMockAnthropicService(): Promise<MockAnthropicHarness> {
  const proc = await startProcess(
    resolveStandaloneRustBinary(
      "mock-anthropic-service",
      "MOCK_ANTHROPIC_SERVICE_BIN",
    ),
    {},
    /^MOCK_ANTHROPIC_BASE_URL=(.+)$/,
  );
  const baseUrl = proc.match[1];
  if (!baseUrl) {
    throw new Error("mock-anthropic-service did not report a base URL");
  }
  return {
    ...proc,
    baseUrl,
  };
}

export async function startOpenyakServer(
  env: NodeJS.ProcessEnv,
): Promise<OpenyakServerHarness> {
  const workspace = await mkdtemp(path.join(os.tmpdir(), "openyak-sdk-alpha-"));
  const proc = await startProcess(
    resolveOpenyakServerCommand(),
    {
      cwd: workspace,
      env,
    },
    /^Local thread server listening on (http:\/\/.+)$/,
  );
  const baseUrl = proc.match[1];
  if (!baseUrl) {
    throw new Error("openyak server did not report a base URL");
  }
  return {
    ...proc,
    baseUrl,
    workspace,
    async close() {
      await proc.close();
      await rm(workspace, { recursive: true, force: true });
    },
  };
}

export async function withServerHarness<T>(
  fn: (harness: { mock: MockAnthropicHarness; server: OpenyakServerHarness }) => Promise<T>,
): Promise<T> {
  const mock = await startMockAnthropicService();
  const server = await startOpenyakServer({
    ...process.env,
    ANTHROPIC_API_KEY: "test-sdk-key",
    ANTHROPIC_BASE_URL: mock.baseUrl,
  });
  try {
    return await fn({ mock, server });
  } finally {
    await server.close();
    await mock.close();
  }
}

function resolveStandaloneRustBinary(name: string, envVar: string): ProcessCommand {
  const override = process.env[envVar];
  if (override) {
    return { command: override, args: [] };
  }

  const binaryName = process.platform === "win32" ? `${name}.exe` : name;
  const builtBinary = path.join(rustRoot, "target", "debug", binaryName);
  if (existsSync(builtBinary)) {
    return { command: builtBinary, args: [] };
  }

  return {
    command: process.env.CARGO ?? "cargo",
    args: [
      "run",
      "--manifest-path",
      path.join(rustRoot, "Cargo.toml"),
      "--quiet",
      "--bin",
      name,
      "--",
    ],
  };
}

function resolveOpenyakServerCommand(): ProcessCommand {
  const override = process.env.OPENYAK_SERVER_BIN;
  if (override) {
    return { command: override, args: ["server"] };
  }

  const binaryName = process.platform === "win32" ? "openyak.exe" : "openyak";
  const builtBinary = path.join(rustRoot, "target", "debug", binaryName);
  if (existsSync(builtBinary)) {
    return { command: builtBinary, args: ["server"] };
  }

  return {
    command: process.env.CARGO ?? "cargo",
    args: [
      "run",
      "--manifest-path",
      path.join(rustRoot, "Cargo.toml"),
      "--quiet",
      "--bin",
      "openyak",
      "--",
      "server",
    ],
  };
}

async function startProcess(
  proc: ProcessCommand,
  options: {
    cwd?: string;
    env?: NodeJS.ProcessEnv;
  },
  matcher: RegExp,
): Promise<ManagedProcess & { match: RegExpMatchArray }> {
  const child = spawn(proc.command, proc.args.concat(["--bind", "127.0.0.1:0"]), {
    cwd: options.cwd ?? repoRoot,
    env: options.env ?? process.env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const match = await waitForMatch(child, matcher, 90_000);
  return {
    child,
    match,
    async close() {
      await terminate(child);
    },
  };
}

function waitForMatch(
  child: ManagedChildProcess,
  matcher: RegExp,
  timeoutMs: number,
): Promise<RegExpMatchArray> {
  return new Promise((resolve, reject) => {
    const stderrChunks: string[] = [];
    child.stderr.on("data", (chunk: Buffer | string) => {
      stderrChunks.push(String(chunk));
    });

    const timer = setTimeout(() => {
      cleanup();
      reject(
        new Error(
          `process did not emit startup line within ${timeoutMs}ms: ${stderrChunks.join("")}`,
        ),
      );
    }, timeoutMs);

    const rl = readline.createInterface({ input: child.stdout });

    const onExit = (code: number | null, signal: NodeJS.Signals | null) => {
      cleanup();
      reject(
        new Error(
          `process exited before startup line (code=${code}, signal=${signal}): ${stderrChunks.join("")}`,
        ),
      );
    };

    const onLine = (line: string) => {
      const match = line.match(matcher);
      if (!match) {
        return;
      }
      cleanup();
      resolve(match);
    };

    const cleanup = () => {
      clearTimeout(timer);
      rl.close();
      child.off("exit", onExit);
      rl.off("line", onLine);
    };

    child.on("exit", onExit);
    rl.on("line", onLine);
  });
}

async function terminate(child: ManagedChildProcess): Promise<void> {
  if (child.exitCode !== null) {
    return;
  }
  child.kill();
  await new Promise<void>((resolve) => {
    const timer = setTimeout(() => {
      if (child.exitCode === null) {
        child.kill("SIGKILL");
      }
      resolve();
    }, 5_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}
