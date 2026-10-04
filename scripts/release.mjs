import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFileSync, copyFileSync, mkdirSync, writeFileSync } from "node:fs";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";

export const root = resolve(import.meta.dirname, "..");
export const crates = [
  "inflow-core",
  "inflow-tap-seller",
  "inflow-mpp",
  "inflow-mpp-buyer",
  "inflow-mpp-seller",
  "inflow-x402",
  "inflow-x402-buyer",
  "inflow-x402-seller",
  "inflow-x402-axum",
];
export const run = (program, args, cwd = root) =>
  execFileSync(program, args, {
    cwd,
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
    maxBuffer: 32 * 1024 * 1024,
  }).trim();
export const checksum = (path) =>
  createHash("sha256").update(readFileSync(path)).digest("hex");

export function validatePackages(metadata) {
  const packages = metadata.packages.filter(
    (p) => metadata.workspace_members.includes(p.id) && p.publish?.length !== 0,
  );
  assert.deepEqual(
    packages.map((p) => p.name).sort(),
    [...crates].sort(),
    "Unexpected publishable crates",
  );
  const version = packages[0].version;
  assert.match(version, /^\d+\.\d+\.\d+$/, "Use a stable workspace version");
  for (const p of packages) {
    assert.equal(p.version, version, `Version mismatch: ${p.name}`);
    for (const d of p.dependencies.filter((d) => crates.includes(d.name))) {
      assert.equal(
        d.req,
        `^${version}`,
        `Dependency version mismatch: ${p.name}/${d.name}`,
      );
      assert.ok(
        crates.indexOf(d.name) < crates.indexOf(p.name),
        "Crate publication order is invalid",
      );
    }
  }
  return version;
}

export function inspect() {
  assert.equal(
    run("git", ["status", "--porcelain"]),
    "",
    "Release requires a clean checkout",
  );
  const metadata = JSON.parse(
    run("cargo", ["metadata", "--no-deps", "--locked", "--format-version=1"]),
  );
  return {
    version: validatePackages(metadata),
    commit: run("git", ["rev-parse", "HEAD"]),
    target: metadata.target_directory,
  };
}

export function loadManifest(directory) {
  const manifest = JSON.parse(readFileSync(join(directory, "manifest.json")));
  assert.match(manifest.version, /^\d+\.\d+\.\d+$/);
  assert.match(manifest.commit, /^[0-9a-f]{40}$/);
  assert.deepEqual(
    manifest.crates.map((c) => c.name),
    crates,
  );
  for (const c of manifest.crates) {
    assert.equal(c.file, `${c.name}-${manifest.version}.crate`);
    assert.equal(
      checksum(join(directory, c.file)),
      c.sha256,
      `Archive checksum mismatch: ${c.name}`,
    );
  }
  return manifest;
}

async function registryVersion(name, version) {
  const response = await fetch(
    `https://crates.io/api/v1/crates/${name}/${version}`,
    {
      headers: { "User-Agent": "inflow-rust-release (nas@inflowpay.ai)" },
      signal: AbortSignal.timeout(30000),
    },
  );
  if (response.status === 404) return null;
  if (!response.ok) throw Error(`Registry lookup failed: ${response.status}`);
  return (await response.json()).version;
}

export function validatePublished(remote, expected) {
  assert.equal(
    remote.yanked,
    false,
    `Published crate is yanked: ${expected.name}`,
  );
  assert.equal(
    remote.checksum,
    expected.sha256,
    `Existing registry archive differs: ${expected.name}; do not overwrite or silently skip it`,
  );
}

async function main() {
  const [operation, path] = process.argv.slice(2);
  assert.ok(
    ["prepare", "publish"].includes(operation) && path,
    "Usage: node scripts/release.mjs prepare|publish DIRECTORY",
  );
  const directory = resolve(path);
  const state = inspect();
  if (operation === "prepare") {
    mkdirSync(directory);
    run("cargo", [
      "package",
      "--workspace",
      "--exclude",
      "inflow-examples",
      "--exclude",
      "inflow-conformance",
      "--locked",
    ]);
    const manifest = {
      version: state.version,
      commit: state.commit,
      crates: crates.map((name) => {
        const file = `${name}-${state.version}.crate`;
        // Cargo uploads tmp-crate; its copied package artifact can retain trailing bytes on repeated builds.
        const source = join(state.target, "package", "tmp-crate", file);
        copyFileSync(source, join(directory, file));
        return { name, file, sha256: checksum(source) };
      }),
    };
    writeFileSync(
      join(directory, "manifest.json"),
      JSON.stringify(manifest, null, 2) + "\n",
      { flag: "wx" },
    );
    console.log(`Prepared ${crates.length} crates at ${directory}`);
    return;
  }
  const manifest = loadManifest(directory);
  assert.equal(
    state.commit,
    manifest.commit,
    "Publish from the prepared commit",
  );
  assert.equal(state.version, manifest.version);
  if (process.env.GITHUB_ACTIONS === "true") {
    assert.equal(
      process.env.GITHUB_REF,
      "refs/heads/main",
      "Publish only from main",
    );
    assert.equal(
      process.env.GITHUB_SHA,
      state.commit,
      "Publish only the selected workflow commit",
    );
  } else
    assert.equal(
      run("git", ["branch", "--show-current"]),
      "main",
      "Publish only from main",
    );
  // Cargo owns dependency verification and index visibility; retries never replace a published version.
  for (const c of manifest.crates) {
    let remote = await registryVersion(c.name, state.version);
    if (remote) {
      validatePublished(remote, c);
      console.log(`${c.name} already published with the expected checksum`);
      continue;
    }
    run("cargo", ["publish", "--dry-run", "--locked", "--package", c.name]);
    assert.equal(
      checksum(join(state.target, "package", "tmp-crate", c.file)),
      c.sha256,
      "Cargo package differs from prepared archive; stop before upload",
    );
    run("cargo", ["publish", "--locked", "--package", c.name]);
    for (let attempt = 0; attempt < 60; attempt++) {
      remote = await registryVersion(c.name, state.version);
      if (remote) break;
      await new Promise((resolve) => setTimeout(resolve, 5000));
    }
    assert.ok(remote, `${c.name} not visible; check crates.io before retrying`);
    validatePublished(remote, c);
  }
}
if (process.argv[1] === fileURLToPath(import.meta.url)) await main();
