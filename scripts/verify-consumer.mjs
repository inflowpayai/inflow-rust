import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { resolve, join } from "node:path";
import { crates, loadManifest, run, root } from "./release.mjs";

const [mode, path] = process.argv.slice(2);
assert.ok(
  ["packages", "registry"].includes(mode) && path,
  "Usage: node scripts/verify-consumer.mjs packages|registry PREPARED_DIRECTORY",
);
const directory = resolve(path);
const manifest = loadManifest(directory);
process.env.RUSTUP_TOOLCHAIN = run("rustup", ["show", "active-toolchain"]).split(/\s/)[0];
const consumer = mkdtempSync(join(tmpdir(), "inflow-rust-consumer-"));
const target = process.env.CARGO_TARGET_DIR
  ? resolve(process.env.CARGO_TARGET_DIR)
  : join(root, "target", "consumer-check");
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
    let _ = inflow_tap_seller::Verifier::new(Default::default()).unwrap();
    assert_eq!(Environment::Production.api_base_url(), "https://api.inflowpay.ai");
    let _ = inflow_mpp::decode("e30").unwrap();
    let _ = inflow_x402::generate_payment_id("pay_").unwrap();
    let _ = std::mem::size_of::<inflow_mpp_buyer::Buyer>();
    let _ = inflow_mpp_buyer::Buyer::prepare_card;
    let _ = inflow_mpp_buyer::Buyer::get_payment_status;
    let _ = inflow_x402_buyer::Buyer::get_payment_status;
    let _ = inflow_mpp_buyer::PaymentStatusOptions::default();
    let _ = inflow_x402_buyer::PaymentStatusOptions { retries: 1 };
    let _ = inflow_mpp_buyer::CardPaymentOptions {
        merchant: inflow_mpp_buyer::Merchant {
            name: "Example Store".into(),
            url: "https://example.com".into(),
            country_code: "US".into(),
        },
        instrument_id: None,
    };
    let _ = [inflow_mpp_seller::Method::Stripe, inflow_mpp_seller::Method::Card];
    let _ = std::mem::size_of::<inflow_mpp_seller::Seller>();
    let _ = std::mem::size_of::<inflow_x402_buyer::HttpBuyer>();
    let mut offers = inflow_x402_seller::OfferOptions::new("0.01 USDC");
    assert_eq!(offers.payment_flow, inflow_x402_seller::PaymentFlow::Authorization);
    offers.payment_flow = inflow_x402_seller::PaymentFlow::Upfront;
    assert_eq!(offers.payment_flow, "upfront".parse::<inflow_x402_seller::PaymentFlow>().unwrap());
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
    run("cargo", ["run", "--locked", "--target-dir", target, ...features], consumer);
  }
  console.log(`Consumer passed: ${mode}, default/EVM/Solana/combined features`);
  const standalone = join(consumer, "standalone");
  mkdirSync(join(standalone, "src"), { recursive: true });
  writeFileSync(join(standalone, "src/lib.rs"), "");
  for (const name of crates) {
    writeFileSync(
      join(standalone, "Cargo.toml"),
      `[package]\nname="inflow-standalone-consumer"\nversion="0.1.0"\nedition="2024"\n[dependencies]\n${name}="=${manifest.version}"\n${patches}\n`,
    );
    run("cargo", ["build", "--target-dir", target], standalone);
  }
  console.log(`Consumer passed: ${mode}, every crate as the sole SDK dependency`);
  const tap = join(consumer, "tap-only");
  mkdirSync(join(tap, "src"), { recursive: true });
  const tapPatch = mode === "packages"
    ? `\n[patch.crates-io]\ninflow-tap-seller = { path = ${JSON.stringify(join(consumer, `inflow-tap-seller-${manifest.version}`))} }\n`
    : "";
  writeFileSync(
    join(tap, "Cargo.toml"),
    `[package]\nname="inflow-tap-consumer"\nversion="0.1.0"\nedition="2024"\n[dependencies]\ninflow-tap-seller="=${manifest.version}"\n${tapPatch}\n`,
  );
  writeFileSync(
    join(tap, "src/main.rs"),
    "fn main() { let _ = inflow_tap_seller::Verifier::new(Default::default()).unwrap(); }\n",
  );
  run("cargo", ["run", "--target-dir", target], tap);
  const tree = run("cargo", ["tree", "--locked", "--prefix", "none"], tap);
  assert.ok(!/^inflow-(?:core|mpp|x402)(?:[- ]|$)/m.test(tree));
  console.log(`Consumer passed: ${mode}, TAP without InFlow payment crates`);
} finally {
  rmSync(consumer, { recursive: true, force: true });
}
