import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { crates, loadManifest, run } from "./release.mjs";

const [mode, path] = process.argv.slice(2);
assert.ok(
  ["packages", "registry"].includes(mode) && path,
  "Usage: node scripts/verify-consumer.mjs packages|registry PREPARED_DIRECTORY",
);
const directory = resolve(path);
const manifest = loadManifest(directory);
const consumer = mkdtempSync(join(tmpdir(), "inflow-rust-consumer-"));
try {
  let patches = "";
  if (mode === "packages") {
    for (const c of manifest.crates)
      run("tar", ["-xzf", join(directory, c.file), "-C", consumer]);
    patches =
      "\n[patch.crates-io]\n" +
      crates
        .map(
          (name) =>
            `${name} = { path = ${JSON.stringify(join(consumer, `${name}-${manifest.version}`))} }`,
        )
        .join("\n");
  }
  const dependencies = crates
    .map((name) => `${name} = "=${manifest.version}"`)
    .join("\n");
  writeFileSync(
    join(consumer, "Cargo.toml"),
    `[package]\nname="inflow-release-consumer"\nversion="0.1.0"\nedition="2024"\n[features]\nevm=["inflow-x402-buyer/evm"]\nsolana=["inflow-x402-buyer/solana"]\n[dependencies]\n${dependencies}\n${patches}\n`,
  );
  mkdirSync(join(consumer, "src"));
  writeFileSync(
    join(consumer, "src/main.rs"),
    `use inflow_core::{ClientOptions, Environment};
fn main() {
    let _ = ClientOptions::default();
    assert_eq!(Environment::Production.api_base_url(), "https://api.inflowpay.ai");
    let _ = inflow_mpp::decode("e30").unwrap();
    let _ = inflow_x402::generate_payment_id("pay_").unwrap();
    let _ = std::mem::size_of::<inflow_mpp_buyer::Buyer>();
    let _ = std::mem::size_of::<inflow_mpp_seller::Seller>();
    let _ = std::mem::size_of::<inflow_x402_buyer::HttpBuyer>();
    let _ = inflow_x402_seller::OfferOptions::new("0.01 USDC");
    let _ = std::mem::size_of::<inflow_x402_axum::PaymentLayer>();
    #[cfg(feature="evm")] { let _ = inflow_x402_buyer::eip7702::DELEGATION; }
    #[cfg(feature="solana")] { use inflow_x402_buyer::solana as _; }
}
`,
  );
  run("cargo", ["generate-lockfile"], consumer);
  for (const features of [
    [],
    ["--features", "evm"],
    ["--features", "solana"],
    ["--all-features"],
  ]) {
    run("cargo", ["run", "--locked", ...features], consumer);
  }
  console.log(`Consumer passed: ${mode}, default/EVM/Solana/combined features`);
} finally {
  rmSync(consumer, { recursive: true, force: true });
}
