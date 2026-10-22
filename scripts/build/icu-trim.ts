/**
 * ICU data trimmer — diagnostic tool, NOT wired into the build.
 *
 * bunre's ICU data package is not in this repo. It arrives as `sicudt.lib`
 * inside the prebuilt WebKit tarball (see scripts/build/deps/webkit.ts,
 * `prebuiltIcuLibs`) and is 11.68 MB of the 65.32 MB binary — the single
 * largest non-JSC item. This script can rewrite it down to a subset of
 * locales, which looks like an ~8 MB win.
 *
 * ## Status: it works, and the win is NOT shippable
 *
 * The repack itself is correct. Emitting all 3,990 items through this code
 * produces a `sicudt.lib` that links to a binary of *exactly* the prebuilt
 * binary's size (68,491,776 bytes) with identical behaviour — so the package
 * layout below is right, byte for byte.
 *
 * Dropping locale bundles is what fails. Measured with `bun run build:release`:
 *
 *   keep 3,990 items (all)            -> 68,491,776  ok
 *   drop 2 items (zu, zu_ZA)          -> 68,487,680  ok
 *   keep 2,032 (locales a-m)          -> 65,060,352  CRASH fastfail
 *   keep   173 (root, und, en*, zh*)   -> 60,522,496  CRASH fastfail
 *   keep    34 (root, und, en*)       -> 60,465,152  CRASH fastfail
 *
 * "CRASH" is exit code 0xC0000409 during VM init — `bunre --revision` works,
 * anything that creates a VM dies. It is not the default-locale chain:
 * setting LANG/LC_ALL to en_US.UTF-8 does not change the outcome, because on
 * Windows the default locale comes from GetUserDefaultLocaleName, not the
 * environment.
 *
 * So ICU opens at least one locale bundle unconditionally during init, from
 * the n-z range, and which ones it opens depends on the host and the WebKit
 * build. A data file pruned to satisfy one machine's init path would break on
 * another default locale — or on the next WebKit bump, which silently
 * restores the untrimmed prebuilt. That is why this is a diagnostic and not
 * a build step.
 *
 * ## Usage
 *
 *   bun scripts/build/icu-trim.ts <out-dir> [--keep <regex>] [--drop <prefix>]
 *                                       [--keep-all]
 *
 * Writes <out-dir>/sicudt.lib plus the extracted icudt78l.dat and
 * icu_dict.bin. Then copy it over
 * ~/.bun/build-cache/webkit-<sha>/lib/sicudt.lib and rebuild to test.
 * ALWAYS keep a copy of the original: the prebuilt is re-downloaded on a
 * WebKit version change, which is the safe way back.
 *
 * ## Package layout (derived empirically, see SLEEPING-PLAN.md section 8)
 *
 *   [0, 144)        UDataInfo header, copied verbatim
 *   [144]           u32 tocEntryCount
 *   [148, +8N)      N x (u32 nameOffset, u32 dataOffset)
 *   then            string area: N null-terminated item names
 *   then            item data, 4-byte aligned
 *
 * Both offsets are relative to base 144, not to the start of their region.
 * That is the non-obvious part: in the prebuilt, nameOffset[0]=31924 maps to
 * absolute 32068=144+31924, and dataOffset[0]=122256 maps to absolute
 * 122400=144+122256. Writing a chunk-relative offset instead produces a
 * package that parses but reads the wrong bytes.
 *
 * Item names must keep their `icudt78l/` prefix — ICU derives the expected
 * TOC prefix from the .dat filename and rejects the package otherwise.
 *
 * Item bytes are copied verbatim. The prebuilt stores each item as an
 * individual zstd frame sharing a 128 KB dictionary, decompressed on demand
 * by src/jsc/bindings/bun_icu_decompress.cpp; keeping the frames keeps that
 * hook working with no source change.
 */

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

const AR = "d:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/Llvm/x64/bin/llvm-ar.exe";
const CC = "d:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/Llvm/x64/bin/clang-cl.exe";
const NM = "d:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/Llvm/x64/bin/llvm-nm.exe";
const RE = "d:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/Llvm/x64/bin/llvm-readobj.exe";

const WEBKIT_LIB = join(
  process.env.USERPROFILE || join(homedir(), ".bun"),
  ".bun/build-cache/webkit-2e2aa2290fac856d/lib/sicudt.lib",
);

const BASE = 144; // TOC offsets are relative to the end of UDataInfo
const ENT = 148;
const DEFAULT_KEEP = "^(root|und|en.*)$";

const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, "");
const flag = (name: string) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : "";
};
const has = (name: string) => process.argv.includes(name);

const outDir = resolve(process.argv[2] || "icu-trim-out");
mkdirSync(outDir, { recursive: true });

// --- 1. pull the single member out of the prebuilt archive -----------------
execFileSync(AR, ["x", WEBKIT_LIB, `--output=${outDir}`]);
const objPath = join(outDir, "icudt78l_dat.o");
const obj = readFileSync(objPath);

// --- 2. locate .rdata and the three symbols --------------------------------
const sections = stripAnsi(execFileSync(RE, ["--sections", objPath], { encoding: "utf8" }));
const ri = sections.indexOf("Name: .rdata");
if (ri < 0) throw new Error("no .rdata section in icudt78l_dat.o");
const rEnd = sections.indexOf("Section {", ri);
const block = sections.slice(ri, rEnd < 0 ? sections.length : rEnd);
const field = (f: string) => {
  const m = new RegExp(`${f}:\\s*(0x)?([0-9a-fA-F]+)`).exec(block);
  if (!m) throw new Error(`field not found: ${f}`);
  return m[1] ? parseInt(m[2], 16) : parseInt(m[2], 10);
};
const RAW = field("PointerToRawData");

const syms = stripAnsi(execFileSync(NM, ["--print-size", objPath], { encoding: "utf8" }));
const addr = (n: string) => {
  const m = new RegExp(`^([0-9a-f]+)\\s+[0-9a-f]+\\s+R\\s+${n}\\s*$`, "m").exec(syms);
  if (!m) throw new Error(`symbol not found: ${n}`);
  return parseInt(m[1], 16);
};

// The dictionary length is the gap between the two symbols; the u32 stored at
// the size symbol's address is not the size (reading it yields garbage).
const DAT = addr("icudt78_dat");
const DICT = addr("bun_icu_zstd_dict");
const DICTSZ = addr("bun_icu_zstd_dict_size");

const pkg = obj.subarray(RAW + DAT, RAW + DICT);
const dict = obj.subarray(RAW + DICT, RAW + DICTSZ);
if (pkg[2] !== 0xda || pkg[3] !== 0x27) throw new Error("bad ICU data magic");

// --- 3. read the TOC -------------------------------------------------------
const COUNT = pkg.readUInt32LE(144);
const nameOf = (o: number) => {
  const p = BASE + o;
  let e = p;
  while (e < pkg.length && pkg[e] !== 0) e++;
  return pkg.subarray(p, e).toString("latin1");
};
const items: { name: string; off: number; len: number }[] = [];
for (let i = 0; i < COUNT; i++) {
  items.push({ name: nameOf(pkg.readUInt32LE(ENT + i * 8)), off: BASE + pkg.readUInt32LE(ENT + i * 8 + 4), len: 0 });
}
for (let i = 0; i < items.length; i++) items[i].len = (i + 1 < items.length ? items[i + 1].off : pkg.length) - items[i].off;

// --- 4. choose what to keep ------------------------------------------------
const isRes = (n: string) => /\.res$/.test(n);
const locOf = (n: string) => n.replace(/^icudt78l\//, "").replace(/\.res$/, "");
const keepRe = has("--keep") ? flag("--keep") : has("--keep-all") ? "" : DEFAULT_KEEP;
const dropRe = flag("--drop");

const keep = has("--keep-all")
  ? items
  : dropRe
    ? items.filter(it => !(isRes(it.name) && new RegExp(`^${dropRe}($|_)`).test(locOf(it.name))))
    : items.filter(it => !isRes(it.name) || (keepRe ? new RegExp(keepRe).test(locOf(it.name)) : true));

const label = has("--keep-all") ? " --keep-all" : dropRe ? ` --drop ${dropRe}` : ` --keep ${keepRe}`;
console.log(`keeping ${keep.length}/${items.length} items${label}`);

// --- 5. re-emit ------------------------------------------------------------
const N = keep.length;
const header = Buffer.from(pkg.subarray(0, BASE));

let sp = 0;
const strs = keep.map(it => {
  const b = Buffer.from(it.name, "latin1");
  const inArea = sp;
  sp += b.length + 1;
  return { b, inArea, o: 4 + N * 8 + inArea };
});
const strArea = Buffer.alloc(sp);
keep.forEach((it, i) => {
  strArea.set(strs[i].b, strs[i].inArea);
  strArea[strs[i].inArea + strs[i].b.length] = 0;
});

const dataBaseRel = 4 + N * 8 + strArea.length;
const chunks: Buffer[] = [];
let w = 0;
for (const it of keep) {
  while (w % 4 !== 0) w++;
  it.newOff = dataBaseRel + w;
  chunks.push(pkg.subarray(it.off, it.off + it.len));
  w += it.len;
}
const dataArea = Buffer.alloc(w);
{
  let ww = 0;
  for (const c of chunks) {
    while (ww % 4 !== 0) ww++;
    c.copy(dataArea, ww);
    ww += c.length;
  }
}

const out = Buffer.alloc(BASE + 4 + N * 8 + strArea.length + dataArea.length);
header.copy(out, 0);
out.writeUInt32LE(N, 144);
keep.forEach((it, i) => {
  out.writeUInt32LE(strs[i].o, ENT + i * 8);
  out.writeUInt32LE(it.newOff as number, ENT + i * 8 + 4);
});
strArea.copy(out, BASE + 4 + N * 8);
dataArea.copy(out, BASE + 4 + N * 8 + strArea.length);

// Round-trip check using the same offset rule the reader uses.
for (let i = 0; i < N; i++) {
  const no = BASE + out.readUInt32LE(ENT + i * 8);
  const dobs = BASE + out.readUInt32LE(ENT + i * 8 + 4);
  let e = no;
  while (e < out.length && out[e] !== 0) e++;
  if (out.subarray(no, e).toString("latin1") !== keep[i].name) throw new Error(`name mismatch at ${i}`);
  const next = i + 1 < N ? BASE + out.readUInt32LE(ENT + (i + 1) * 8 + 4) : out.length;
  if (!out.subarray(dobs, next).equals(chunks[i])) throw new Error(`data mismatch at ${i}`);
}

const mb = (n: number) => `${(n / 1048576).toFixed(2)} MB`;
console.log(`verified ${N} entries round-trip`);
console.log(`package: ${mb(out.length)} (was ${mb(pkg.length)}), saving ${mb(pkg.length - out.length)}`);

writeFileSync(join(outDir, "icudt78l.dat"), out);
writeFileSync(join(outDir, "icu_dict.bin"), dict);

// `extern` is load-bearing: `const` at namespace scope has internal linkage in
// C++, which would leave icudt78_dat local and icuuc's reference unresolved.
// The prebuilt's object has all three symbols global (llvm-nm uppercase 'R').
const cpp = `extern "C" {

extern const unsigned char icudt78_dat[] = {
#embed "icudt78l.dat"
};

extern const unsigned char bun_icu_zstd_dict[] = {
#embed "icu_dict.bin"
};

extern const unsigned int bun_icu_zstd_dict_size = ${dict.length}u;

}
`;
writeFileSync(join(outDir, "icu_data.cpp"), cpp);

const objOut = join(outDir, "icu_data.obj");
execFileSync(CC, ["/nologo", "/std:c++20", "/c", "/O2", `/Fo${objOut}`, join(outDir, "icu_data.cpp")], {
  stdio: "inherit",
});
if (!existsSync(objOut)) throw new Error("compile produced no object");

const libPath = join(outDir, "sicudt.lib");
if (existsSync(libPath)) unlinkSync(libPath);
execFileSync(AR, ["rcs", libPath, objOut]);

const nmOut = stripAnsi(execFileSync(NM, ["--print-size", objOut], { encoding: "utf8" }));
for (const s of ["icudt78_dat", "bun_icu_zstd_dict", "bun_icu_zstd_dict_size"]) {
  const m = new RegExp(`^([0-9a-f]+)\\s+\\S+\\s+(\\S)\\s+${s}\\s*$`, "m").exec(nmOut);
  if (!m) throw new Error(`symbol missing from output object: ${s}`);
  if (m[2] !== "R") throw new Error(`symbol ${s} is ${m[2]} (local), expected R (global)`);
}
console.log(`wrote ${libPath} — all three symbols global`);
console.log("verify with: bun run build:release, then bunre.exe -e 'console.log(new Intl.NumberFormat(\"en-US\").format(1))'");