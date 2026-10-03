import { execFileSync } from "node:child_process";
import { open, readFile } from "node:fs/promises";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { runtimeCases } from "../conformance/runtime-cases.mjs";

const root = fileURLToPath(new URL("..", import.meta.url));
const command = (program, args, cwd = root) =>
  execFileSync(program, args, {
    cwd,
    encoding: "utf8",
    timeout: 300000,
    maxBuffer: 32 * 1024 * 1024,
  }).trim();

export function checkContract(directory, revision) {
  if (!/^[0-9a-f]{40}$/.test(revision))
    throw new Error("Expected a full contract commit SHA");
  if (
    command("git", ["rev-parse", "HEAD"], directory) !== revision ||
    command("git", ["status", "--porcelain"], directory)
  ) {
    throw new Error(`Use a clean contract checkout at ${revision}`);
  }
}

export function metadata(cargo) {
  const packages = {},
    dependencies = {};
  for (const item of cargo.packages) {
    if (cargo.workspace_members.includes(item.id)) {
      if (
        item.name.startsWith("inflow-") &&
        !["inflow-conformance", "inflow-examples"].includes(item.name)
      )
        packages[item.name] = item.version;
    } else {
      const versions = dependencies[item.name]?.split(", ") ?? [];
      if (!versions.includes(item.version)) versions.push(item.version);
      dependencies[item.name] = versions.sort().join(", ");
    }
  }
  return {
    name: "inflow-rust",
    runtime: command("rustc", ["--version"]),
    packages,
    dependencies,
  };
}

export function buildAdapter(name = "adapter") {
  const build = command("cargo", [
    "test",
    "-p",
    "inflow-conformance",
    "--test",
    name,
    "--no-run",
    "--locked",
    "--message-format=json",
  ]);
  const artifacts = build.split("\n").map((line) => JSON.parse(line));
  const binary = artifacts.find(
    (item) =>
      item.reason === "compiler-artifact" &&
      item.target.name === name &&
      item.executable,
  )?.executable;
  if (!binary) throw new Error("No conformance test executable produced");
  return binary;
}

async function main() {
  const { values } = parseArgs({
    options: {
      "contract-root": { type: "string" },
      "output-dir": { type: "string" },
    },
  });
  if (!values["contract-root"] || !values["output-dir"])
    throw new Error(
      "Usage: node scripts/conformance.mjs --contract-root PATH --output-dir EXISTING_DIRECTORY",
    );
  const contractRoot = resolve(values["contract-root"]);
  const pin = JSON.parse(
    await readFile(
      new URL("../conformance/inflow-specs.lock.json", import.meta.url),
      "utf8",
    ),
  );
  checkContract(contractRoot, pin.revision);
  const binary = buildAdapter();
  const implementation = metadata(
    JSON.parse(
      command("cargo", ["metadata", "--locked", "--format-version=1"]),
    ),
  );
  const { run } = await import(
    pathToFileURL(join(contractRoot, "runner/run.mjs"))
  );
  const controller = new AbortController();
  const abort = () => controller.abort();
  process.once("SIGINT", abort);
  process.once("SIGTERM", abort);
  try {
    for (const suite of ["runtime", "mpp", "x402"]) {
      if (controller.signal.aborted) throw new Error("Conformance interrupted");
      const fixtures = await import(
        pathToFileURL(join(contractRoot, `fixtures/${suite}.mjs`))
      );
      const index =
        suite === "runtime"
          ? runtimeCases(fixtures.runtimeScenarios)
          : suite === "mpp"
            ? fixtures.mppCasesWithSellerChallenges((challenge) =>
                JSON.parse(
                  command(binary, [
                    "--sign-challenge",
                    JSON.stringify(challenge),
                  ]),
                ),
              )
            : fixtures.x402CasesForOwnedPayments();
      const capabilities = {
        suites:
          suite === "runtime"
            ? ["runtime"]
            : [`${suite}-core`, `${suite}-buyer`, `${suite}-seller`],
        supported_features: [],
        unsupported_features:
          suite === "runtime"
            ? []
            : suite === "mpp"
              ? [
                  {
                    id: "mpp-seller-subscriptions",
                    reason:
                      "Upstream mpp does not implement Seller subscription intents; Rust supports Seller charges and Buyer subscriptions.",
                  },
                ]
              : [
                  {
                    id: "x402-shared-payment-wait",
                    reason:
                      "Waiting consumes the Rust payment handle; a second wait cannot be called on the same handle.",
                  },
                ],
      };
      for (const [name, value] of [
        ["cases", index],
        ["capabilities", capabilities],
        ["implementation", implementation],
      ]) {
        const file = await open(
          join(resolve(values["output-dir"]), `${suite}-${name}.json`),
          "wx",
          0o600,
        );
        try {
          await file.writeFile(`${JSON.stringify(value, null, 2)}\n`);
        } finally {
          await file.close();
        }
      }
      const file = await open(
        join(resolve(values["output-dir"]), `${suite}.json`),
        "wx",
        0o600,
      );
      try {
        const report = await run({
          index,
          capabilities,
          implementation,
          command: [binary, "--adapter"],
          contractRoot,
          sdkRoot: root,
          signal: controller.signal,
        });
        await file.writeFile(`${JSON.stringify(report, null, 2)}\n`);
        console.log(
          `${suite}: ${report.results.filter((v) => v.status === "passed").length} passed, ${report.results.filter((v) => v.status === "skipped").length} unsupported`,
        );
        if (!report.passed) {
          process.exitCode = 1;
          console.error(
            report.runner_error ??
              report.results.filter(
                (v) => !["passed", "skipped"].includes(v.status),
              ),
          );
        }
      } finally {
        await file.close();
      }
    }
  } finally {
    process.removeListener("SIGINT", abort);
    process.removeListener("SIGTERM", abort);
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
