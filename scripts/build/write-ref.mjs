#!/usr/bin/env node
// Compute .ref identity values for offline builds.
// Writes the 16-char identity hash to vendor/<name>/.ref
// so ninja's fetch step no-ops without hitting github.com.
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, "..", "..");

function identity(commit, patchContents) {
  const h = createHash("sha256");
  h.update(commit);
  for (const content of patchContents) {
    h.update("\0");
    h.update(content.replace(/\r\n/g, "\n"));
  }
  return h.digest("hex").slice(0, 16);
}

// No patches for either dep.
writeFileSync(
  join(repoRoot, "vendor", "boringssl", ".ref"),
  identity("41bf9b59c2ebf277a7aa427e1ecad5cc80dd4d4f", []) + "\n",
);
writeFileSync(
  join(repoRoot, "vendor", "mimalloc", ".ref"),
  identity("6a64e1ba7f5b2130d4efccb67ec87fd0003f0f6a", []) + "\n",
);

console.log("wrote .ref stamps to vendor/boringssl/.ref and vendor/mimalloc/.ref");
