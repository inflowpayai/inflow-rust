import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, writeFileSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  crates,
  validatePackages,
  validatePublished,
  loadManifest,
  checksum,
  verifyPreparedPackages,
} from "./release.mjs";

test("preparation requires archive consumer success and invalidates a failed manifest", () => {
  const directory = mkdtempSync(join(tmpdir(), "inflow-release-test-"));
  const manifest = join(directory, "manifest.json");
  try {
    writeFileSync(manifest, "{}");
    verifyPreparedPackages(directory, (program, args) => {
      assert.equal(program, process.execPath);
      assert.equal(args[0], fileURLToPath(new URL("./verify-consumer.mjs", import.meta.url)));
      assert.deepEqual(args.slice(1), ["packages", directory]);
    });
    assert.ok(existsSync(manifest));
    const failure = new Error("Consumer compilation failed");
    assert.throws(() => verifyPreparedPackages(directory, () => { throw failure; }),
      (error) => error === failure);
    assert.equal(existsSync(manifest), false);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

const metadata = () => ({
  workspace_members: crates,
  packages: crates.map((name, i) => ({
    id: name,
    name,
    version: "0.1.0",
    publish: null,
    dependencies: i ? [{ name: crates[i - 1], req: "^0.1.0" }] : [],
  })),
});
test("release includes every public crate at a coordinated version and dependency order", () => {
  assert.equal(validatePackages(metadata()), "0.1.0");
  for (const mutate of [
    (m) => m.packages.pop(),
    (m) => (m.packages[0].version = "0.2.0"),
    (m) => (m.packages[0].version = "0.1.0-beta"),
    (m) => (m.packages[1].dependencies[0].req = "*"),
    (m) => m.packages[0].dependencies.push({ name: crates[1], req: "^0.1.0" }),
  ]) {
    const m = metadata();
    mutate(m);
    assert.throws(() => validatePackages(m));
  }
});
test("reruns reject yanked or different registry archives", () => {
  const expected = { name: "inflow-core", sha256: "abc" };
  validatePublished({ yanked: false, checksum: "abc" }, expected);
  assert.throws(() =>
    validatePublished({ yanked: true, checksum: "abc" }, expected),
  );
  assert.throws(() =>
    validatePublished({ yanked: false, checksum: "def" }, expected),
  );
});
test("manifest rejects missing, changed, or misnamed archives", () => {
  const directory = mkdtempSync(join(tmpdir(), "inflow-release-test-"));
  try {
    const manifest = {
      version: "0.1.0",
      commit: "a".repeat(40),
      crates: crates.map((name) => {
        const file = `${name}-0.1.0.crate`;
        writeFileSync(join(directory, file), name);
        return { name, file, sha256: checksum(join(directory, file)) };
      }),
    };
    const save = () =>
      writeFileSync(join(directory, "manifest.json"), JSON.stringify(manifest));
    save();
    assert.equal(loadManifest(directory).version, "0.1.0");
    writeFileSync(join(directory, manifest.crates[0].file), "changed");
    assert.throws(() => loadManifest(directory));
    writeFileSync(join(directory, manifest.crates[0].file), crates[0]);
    manifest.crates[0].file = "../escape";
    save();
    assert.throws(() => loadManifest(directory));
    manifest.crates.pop();
    save();
    assert.throws(() => loadManifest(directory));
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});
