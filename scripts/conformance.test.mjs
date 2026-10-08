import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import {
  buildAdapter,
  checkContract,
  metadata,
  signedSellerCases,
} from "./conformance.mjs";
import { runtimeCases } from "../conformance/runtime-cases.mjs";

const binary = buildAdapter();
test("Seller signatures update fixture identities without erasing negative controls", () => {
  const challenge = { id: "synthetic", method: "card", request: "request" };
  const index = {
    cases: [
      {
        operation: "mpp.seller.verify",
        input: { credential: { challenge } },
        expect: {
          result: {
            challenge,
            receipt: { challengeId: "wrong" },
            reference: "synthetic",
          },
        },
      },
      { operation: "mpp.buyer.fulfil", input: { credential: { challenge } } },
    ],
  };
  const result = signedSellerCases(index, () => ({
    id: "signed",
    expires: "2099-01-01T00:00:00Z",
  }));
  assert.equal(result.cases[0].input.credential.challenge.id, "signed");
  assert.equal(result.cases[0].expect.result.receipt.challengeId, "wrong");
  assert.equal(result.cases[0].expect.result.reference, "synthetic");
  assert.equal(
    result.cases[0].input.credential.challenge.expires,
    "2099-01-01T00:00:00Z",
  );
  assert.equal(result.cases[1].input.credential.challenge.id, "synthetic");
  assert.equal(index.cases[0].input.credential.challenge.id, "synthetic");
});
const root = fileURLToPath(new URL("..", import.meta.url));
const request = (operation, input, adapter_version = "1") => ({
  adapter_version,
  sequence: 1,
  case_id: "tooling.case",
  operation,
  input,
});
function respond(message) {
  return JSON.parse(
    execFileSync(binary, ["--adapter"], {
      input: JSON.stringify(message) + "\n",
      encoding: "utf8",
      timeout: 5000,
    }),
  );
}

test("JSON-lines adapter echoes identifiers and invokes real codecs", () => {
  const message = request("mpp.core.encode", { value: { hello: "world" } });
  assert.deepEqual(respond(message), {
    adapter_version: "1",
    sequence: 1,
    case_id: "tooling.case",
    result: Buffer.from('{"hello":"world"}').toString("base64url"),
  });
  const output = execFileSync(binary, ["--adapter"], {
    input: [message, message].map(JSON.stringify).join("\n") + "\n",
    encoding: "utf8",
  });
  assert.equal(output.trim().split("\n").length, 2);
});
test("unknown operations and malformed input fail instead of becoming expected SDK errors", () => {
  for (const message of [
    request("wrong.operation", {}),
    request("mpp.core.decode", {}),
    request("mpp.core.encode", {}, "2"),
  ])
    assert.equal(respond(message).error.code, "ADAPTER_ERROR");
  assert.equal(
    respond(request("mpp.core.decode", { value: "!" })).error.code,
    "invalid-input",
  );
  assert.notEqual(
    spawnSync(binary, ["--adapter"], { input: "not json\n", encoding: "utf8" })
      .status,
    0,
  );
});
test("network adapter refuses remote origins and credential-bearing URLs", () => {
  for (const base_url of [
    "https://api.inflowpay.ai",
    "http://localhost:8080",
    "http://127.0.0.1:1234/path",
    "http://secret@127.0.0.1:1234",
    "http://127.0.0.1:1234?q=1",
    "http://127.0.0.1:1234#fragment",
  ]) {
    assert.equal(
      respond(request("x402.buyer.sign", { base_url })).error.code,
      "ADAPTER_ERROR",
    );
  }
});
test("metadata records resolved versions without conflating duplicate dependency versions", () => {
  const value = metadata({
    workspace_members: ["sdk", "tool"],
    packages: [
      { id: "sdk", name: "inflow-core", version: "0.1.0" },
      { id: "tool", name: "inflow-conformance", version: "0.1.0" },
      { id: "a", name: "http", version: "1.0.0" },
      { id: "b", name: "http", version: "0.2.0" },
    ],
  });
  assert.deepEqual(value.packages, { "inflow-core": "0.1.0" });
  assert.deepEqual(value.dependencies, { http: "0.2.0, 1.0.0" });
  assert.match(value.runtime, /^rustc /);
});
test("Seller configuration errors cannot pass negative price cases", () => {
  const output = respond(
    request("x402.seller.offers", {
      config: {},
      options: { price: "invalid" },
    }),
  );
  assert.equal(output.error.code, "ADAPTER_ERROR");
});
test("unimplemented Seller intents fail at the adapter boundary", () => {
  assert.equal(
    respond(
      request("mpp.seller.prepare", {
        method: "inflow",
        intent: "subscription",
      }),
    ).error.code,
    "ADAPTER_ERROR",
  );
});
test("contract rejects moving branch names and wrong commits", () => {
  assert.throws(() => checkContract(root, "main"), /full contract commit/);
  assert.throws(
    () => checkContract(root, "0".repeat(40)),
    /clean contract checkout/,
  );
});
test("runtime fixtures represent operation-specific retries and supported environments", () => {
  const cases = runtimeCases({}).cases;
  assert.equal(
    cases.filter((v) => v.operation === "runtime.environment").length,
    12,
  );
  for (const item of cases.filter((v) => v.id.endsWith("retry-policy")))
    assert.equal(
      item.platform.exchanges.length,
      item.input.product === "mpp-buyer" ? 1 : 2,
    );
  assert.ok(
    cases
      .filter((v) => v.id.endsWith("http.500"))
      .every((v) => v.platform.exchanges.length === 1),
  );
});
