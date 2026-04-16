/**
 * OrbitBuild Setup Action — post cleanup.
 *
 * Runs when the job finishes. Tears down:
 * 1. The Mission Control daemon process
 * 2. The docker buildx builder
 */
import * as core from "@actions/core";
import * as exec from "@actions/exec";
import * as fs from "fs";

const SOCKET_DIR = "/tmp";

function platformToSocket(platform: string): string {
  const arch = platform.includes("arm64") ? "arm64" : "amd64";
  return `${SOCKET_DIR}/orbit-${arch}.sock`;
}

/**
 * Kill the Mission Control daemon process.
 */
async function killMissionControl(): Promise<void> {
  const pidStr = core.getState("mc_pid");
  if (!pidStr) {
    core.info("No Mission Control PID found — nothing to kill");
    return;
  }

  const pid = parseInt(pidStr, 10);
  if (isNaN(pid)) {
    core.warning(`Invalid MC PID: ${pidStr}`);
    return;
  }

  try {
    process.kill(pid, "SIGTERM");
    core.info(`Sent SIGTERM to Mission Control (PID: ${pid})`);

    // Give it a moment to shut down gracefully
    await new Promise((resolve) => setTimeout(resolve, 2000));

    // Check if still running, force kill if needed
    try {
      process.kill(pid, 0); // throws if process is gone
      process.kill(pid, "SIGKILL");
      core.info(`Force-killed Mission Control (PID: ${pid})`);
    } catch {
      // Process already gone — good
      core.info("Mission Control has exited");
    }
  } catch (error) {
    if (error instanceof Error) {
      core.warning(`Failed to kill Mission Control: ${error.message}`);
    }
  }
}

/**
 * Remove the docker buildx builder.
 */
async function removeBuilder(): Promise<void> {
  const builderName = core.getState("builder_name") || "orbit";

  core.info(`Removing buildx builder '${builderName}'...`);

  const exitCode = await exec.exec("docker", ["buildx", "rm", builderName], {
    ignoreReturnCode: true,
    silent: true,
  });

  if (exitCode === 0) {
    core.info(`Removed buildx builder '${builderName}'`);
  } else {
    core.info(`Builder '${builderName}' may not exist — ignoring`);
  }
}

/**
 * Clean up Unix socket files.
 */
async function cleanupSockets(): Promise<void> {
  const platformsStr = core.getState("platforms") || "linux/amd64,linux/arm64";
  const platforms = platformsStr.split(",").map((p) => p.trim());

  for (const platform of platforms) {
    const socketPath = platformToSocket(platform);
    try {
      await fs.promises.unlink(socketPath);
      core.info(`Removed socket ${socketPath}`);
    } catch {
      // Socket may not exist — that's fine
    }
  }
}

/**
 * Main cleanup entry point.
 */
async function run(): Promise<void> {
  core.info("OrbitBuild cleanup — tearing down...");

  await removeBuilder();
  await killMissionControl();
  await cleanupSockets();

  core.info("Cleanup complete");
}

run();
