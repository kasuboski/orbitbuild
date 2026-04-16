/**
 * OrbitBuild Setup Action — main entry point.
 *
 * 1. Downloads the orbitbuild binary (from GitHub releases)
 * 2. Starts mission-control in the background
 * 3. Waits for bridge sockets to become ready
 * 4. Returns control to the workflow
 *
 * The post action (cleanup.ts) tears everything down when the job finishes.
 */
import * as core from "@actions/core";
import * as exec from "@actions/exec";
import * as tc from "@actions/tool-cache";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { ChildProcess, spawn } from "child_process";

const BUILDER_NAME = "orbit";
const SOCKET_DIR = "/tmp";
const DEFAULT_TIMEOUT_SECS = 60;

interface PlatformSocket {
  platform: string;
  socketPath: string;
}

function platformToSocket(platform: string): string {
  const arch = platform.includes("arm64") ? "arm64" : "amd64";
  return path.join(SOCKET_DIR, `orbit-${arch}.sock`);
}

function getRequestedPlatforms(): string[] {
  return core.getInput("platforms").split(",").map((p) => p.trim());
}

function getSocketPaths(platforms: string[]): PlatformSocket[] {
  return platforms.map((p) => ({
    platform: p,
    socketPath: platformToSocket(p),
  }));
}

/**
 * Download the orbitbuild binary from GitHub releases.
 */
async function installOrbitbuild(version: string): Promise<string> {
  // Check if already installed (e.g., via mise or manual install)
  const existing = await findBinary("orbitbuild");
  if (existing) {
    core.info(`Found existing orbitbuild at ${existing}`);
    return existing;
  }

  const arch = os.arch() === "arm64" ? "aarch64" : "x86_64";
  const ext = os.platform() === "win32" ? ".exe" : "";
  const archiveName = `orbitbuild-${arch}-unknown-linux-musl${ext}`;

  core.info(`Downloading orbitbuild ${version} for ${arch}...`);

  const url =
    version === "latest"
      ? `https://github.com/kasuboski/orbitbuild/releases/latest/download/${archiveName}`
      : `https://github.com/kasuboski/orbitbuild/releases/download/${version}/${archiveName}`;

  const downloadPath = await tc.downloadTool(url);

  // Make executable
  const binDir = path.join(os.homedir(), ".orbitbuild", "bin");
  await fs.promises.mkdir(binDir, { recursive: true });
  const binPath = path.join(binDir, "orbitbuild");
  await fs.promises.rename(downloadPath, binPath);
  await fs.promises.chmod(binPath, 0o755);

  // Add to PATH
  core.addPath(binDir);
  core.info(`Installed orbitbuild to ${binPath}`);

  return binPath;
}

async function findBinary(name: string): Promise<string | null> {
  const isWindows = os.platform() === "win32";
  const ext = isWindows ? ".exe" : "";

  // Check PATH
  const pathDirs = (process.env.PATH || "").split(path.delimiter);
  for (const dir of pathDirs) {
    const fullPath = path.join(dir, name + ext);
    try {
      await fs.promises.access(fullPath, fs.constants.X_OK);
      return fullPath;
    } catch {
      // continue
    }
  }
  return null;
}

/**
 * Start mission-control as a background process.
 * Returns the child process handle.
 */
function startMissionControl(
  orbitbuildBin: string,
  beacon: string,
  platforms: string
): ChildProcess {
  const dataDir = path.join(os.homedir(), ".orbitbuild");

  core.info("Starting Mission Control daemon...");

  const child = spawn(
    orbitbuildBin,
    [
      "--data-dir",
      dataDir,
      "mission-control",
      "--beacon",
      beacon,
      "--platforms",
      platforms,
    ],
    {
      detached: true,
      stdio: "ignore",
    }
  );

  child.unref();

  // Save PID for cleanup
  core.saveState("mc_pid", child.pid!.toString());

  core.info(`Mission Control started (PID: ${child.pid})`);

  return child;
}

/**
 * Poll socket readiness using `orbitbuild status --wait`.
 */
async function waitForReadiness(
  orbitbuildBin: string,
  platforms: string
): Promise<void> {
  const timeout = DEFAULT_TIMEOUT_SECS;

  core.info(`Waiting for bridge sockets (timeout: ${timeout}s)...`);

  let stderr = "";
  const exitCode = await exec.exec(
    orbitbuildBin,
    ["status", "--wait", `--timeout-secs=${timeout}`, `--platforms=${platforms}`],
    {
      ignoreReturnCode: true,
      listeners: {
        stderr: (data: Buffer) => {
          stderr += data.toString();
        },
        stdout: (data: Buffer) => {
          core.info(data.toString().trim());
        },
      },
    }
  );

  if (exitCode !== 0) {
    throw new Error(
      `Bridge sockets not ready after ${timeout}s. Mission Control may have failed to connect.\n${stderr}`
    );
  }
}

/**
 * Main entry point.
 */
async function run(): Promise<void> {
  try {
    const beacon = core.getInput("beacon", { required: true });
    const platforms = core.getInput("platforms");
    const version = core.getInput("version");

    core.setSecret(beacon);

    // Save inputs for cleanup
    core.saveState("platforms", platforms);
    core.saveState("builder_name", BUILDER_NAME);

    // 1. Install orbitbuild
    const orbitbuildBin = await installOrbitbuild(version);
    core.saveState("orbitbuild_bin", orbitbuildBin);

    // 2. Start Mission Control in background
    startMissionControl(orbitbuildBin, beacon, platforms);

    // 3. Wait for bridge readiness
    await waitForReadiness(orbitbuildBin, platforms);

    // 4. Report success
    const socketPaths = getSocketPaths(platforms.split(","));
    core.info("");
    core.info("✓ Linked to Constellation!");
    core.info("");
    core.info("Connected platforms:");
    for (const { platform, socketPath } of socketPaths) {
      core.info(`  ${platform} → ${socketPath}`);
    }
    core.info("");
    core.info(
      `Build multi-arch images:\n  docker buildx build --builder ${BUILDER_NAME} --platform ${platforms} -t myapp .`
    );

    core.setOutput("builder", BUILDER_NAME);
    core.setOutput("platforms", platforms);
  } catch (error) {
    if (error instanceof Error) {
      core.setFailed(error.message);
    } else {
      core.setFailed("Unexpected error during OrbitBuild setup");
    }
  }
}

run();
