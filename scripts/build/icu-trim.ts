/**
 * ICU data trimmer — repacks `sicudt.lib` down to a subset of locales.
 *
 * bunre's ICU data package is not in this repo. It arrives as `sicudt.lib`
 * inside the prebuilt WebKit tarball (see scripts/build/deps/webkit.ts,
 * `prebuiltIcuLibs`) and is 11.55 MB of the executable — the single largest
 * non-JSC item. Trimming it to English + Chinese keeps 890 of 3,990 items and
 * the package goes 11.55 MB -> 5.71 MB, which measured as
 * 74,130,944 -> 68,007,424 bytes on `bunre-dbg.exe` (−5.84 MB, 7.9%).
 *
 * ## What may be dropped
 *
 * Two rules, both learned the hard way:
 *
 * 1. Only locale bundles are candidates. Most `.res` items are locales
 *    (`en`, `zh_Hans_CN`, `coll/zh`), but every tree also holds language
 *    -independent data under names that look like locales at a glance:
 *    `res_index` in all nine trees, `pool` in six, plus top-level `keyTypeData`,
 *    `langInfo`, `metadata`, `supplementalData`, `numberingSystems`, `plurals`,
 *    `zoneinfo64`, `tzdbNames`, `curr/supplementalData` and more. ICU opens
 *    those before it looks for any locale, so a keep rule that treats every
 *    `.res` name as a locale deletes them and takes the whole tree down.
 *    `isLocaleLeaf` encodes the CLDR grammar instead, and defaults to keeping
 *    anything that is not a locale id.
 * 2. `pool.res` is not trimmable either. It carries the display names of
 *    locales with no bundle of their own, and *kept* locales read it: with the
 *    pools dropped, 110 of the en/zh probe outputs change. So the pools stay.
 *
 * ## Validation
 *
 * A relink of bunre takes ~16 minutes, which is far too slow to iterate on a
 * data file. `tmp/icu-probe/` links ICU's C API (sicuuc/sicuin) against a
 * candidate `sicudt.lib`, re-implements the zstd hook from
 * src/jsc/bindings/bun_icu_decompress.cpp, and prints ~1,900 Intl-surface
 * results over 10 kept locales plus 10 controls (numbers, dates, calendars,
 * collation, display names, plurals, segmentation, raw resources, normalization,
 * casing, UTS#46):
 *
 *   bun scripts/build/icu-trim.ts tmp/icu-trim-a
 *   bash tmp/icu-probe/build.sh tmp/icu-trim-a/sicudt.lib
 *   tmp/icu-probe/probe.exe > tmp/icu-probe/report-a.txt   # exit 0 == fail=0
 *   diff tmp/icu-probe/report-full.txt tmp/icu-probe/report-a.txt
 *
 * A trim is accepted when the probe reports `fail=0` *and* every differing line
 * belongs to a dropped control locale — kept locales must be byte-identical to
 * the untrimmed baseline. `--keep-all` is the no-op check: it reproduces the
 * prebuilt package byte for byte and its probe report is identical.
 *
 * The second layer is `tmp/intl-check.ts` (~2,000 Intl results through
 * NumberFormat, DateTimeFormat, DisplayNames, Collator, Segmenter, PluralRules,
 * ListFormat, RelativeTimeFormat, Locale, normalize, URL/IDNA) run under the
 * trimmed binary and diffed against official bun. Its en/zh/root/und lines —
 * 772 of them — match official exactly; only fr/de lines move, and they move to
 * the host's default locale, which is what ICU does for a missing bundle.
 *
 * What the trim does *not* survive is a bunre bug independent of the data file:
 * driving ~1,000 Intl constructors in one process hangs the binary, at the same
 * place and with the same line count, whether the ICU package is trimmed or
 * not. Official bun finishes it. That belongs to the event-loop work, not here.
 *
 * ## Usage
 *
 *   bun scripts/build/icu-trim.ts <out-dir> [--keep <regex>]
 *                            [--keep-all] [--keep-lang-dicts] [--webkit <lib>]
 *                            [--cc <clang-cl>] [--ar <llvm-ar>]
 *                            [--nm <llvm-nm>] [--readobj <llvm-readobj>]
 *
 * `--keep` matches the *leaf* of a locale bundle and is unanchored at the end,
 * so `^(root|und|en|zh)(_|$)` keeps `en`, `en_GB`, `zh_Hans_CN`, … Writes
 * <out-dir>/sicudt.lib plus the extracted icudt78l.dat and icu_dict.bin.
 *
 * The build runs this for every release Windows build: deps/webkit.ts adds a
 * `prebuild WebKit` edge whose output is <build-dir>/deps/WebKit/icu-trim/
 * sicudt.lib, and links that instead of the prebuilt `lib/sicudt.lib`. The
 * shared cache is left untouched, so re-extracting the tarball always restores
 * the untrimmed original. Run it by hand only to inspect a candidate trim —
 * copying the result over the cache is what this replaced.
 *
 * ## Package layout (derived empirically, see SLEEPING-PLAN.md section 8)
 *
 *   [0, 144)        UDataInfo header, copied verbatim
 *   [144]           u32 tocEntryCount
 *   [148, +8N)      N x (u32 nameOffset, u32 dataOffset)
 *   then            string area: N null-terminated item names, zero-padded
 *   then            item data, every item 16-byte aligned
 *
 * Both offsets are relative to base 144, not to the start of their region.
 * That is the non-obvious part: in the prebuilt, nameOffset[0]=31924 maps to
 * absolute 32068=144+31924, and dataOffset[0]=122256 maps to absolute
 * 122400=144+122256. Writing a chunk-relative offset instead produces a
 * package that parses but reads the wrong bytes.
 *
 * The second non-obvious part is the 16-byte alignment: all 3,990 items in the
 * prebuilt start on a 16-byte boundary (ICU's data writer aligns to
 * U_DATA_ALIGN), and ICU checks it. At 4-byte alignment the package still
 * loads, most locales still format, and `unorm2_getNFKCInstance` and friends
 * return NULL with `U_ILLEGAL_ARGUMENT_ERROR` — collators fail the same way.
 * So the padding between the last name and the first item is load-bearing.
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
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";

const stripAnsi = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, "");
const flag = (name: string) => {
  const i = process.argv.indexOf(name);
  return i >= 0 ? process.argv[i + 1] : "";
};
const has = (name: string) => process.argv.includes(name);

// Toolchain: the build passes its own clang-cl / llvm-ar / llvm-nm / llvm-readobj
// (cfg.cc / cfg.ar / cfg.nm / cfg.readobj) so the trimmed archive is produced by
// the same LLVM the link uses. The fallbacks keep a hand-run
// `bun scripts/build/icu-trim.ts` working on a machine with VS installed.
const LLVM_BIN = "d:/Program Files/Microsoft Visual Studio/18/Community/VC/Tools/Llvm/x64/bin";
const tool = (flagName: string, exe: string) => flag(flagName) || join(LLVM_BIN, exe);
const AR = tool("--ar", "llvm-ar.exe");
const CC = tool("--cc", "clang-cl.exe");
const NM = tool("--nm", "llvm-nm.exe");
const RE = tool("--readobj", "llvm-readobj.exe");

// config.ts <-> deps/webkit.ts form an import cycle that only resolves when
// config.ts is evaluated first, so importing WEBKIT_VERSION from here throws
// "Cannot access 'WEBKIT_VERSION' before initialization". Read the constant
// straight out of the source instead.
const readWebkitVersion = () => {
  const src = readFileSync(join(import.meta.dir, "deps", "webkit.ts"), "utf8");
  const m = /export const WEBKIT_VERSION = "([0-9a-f]{40})"/.exec(src);
  if (!m) throw new Error("could not read WEBKIT_VERSION from deps/webkit.ts");
  return m[1];
};

// The cache dir is `webkit-<sha16><os?>-<lto|debug?>` (scripts/build/deps/
// webkit.ts, prebuiltDestDir). Pick the one this pin actually extracted, and
// only fall back to a scan if the pin changed without a download.
const cacheRoot = join(process.env.USERPROFILE || homedir(), ".bun", "build-cache");
const v16 = readWebkitVersion().slice(0, 16);
const candidates = [
  join(cacheRoot, `webkit-${v16}-lto`, "lib", "sicudt.lib"),
  join(cacheRoot, `webkit-${v16}`, "lib", "sicudt.lib"),
  join(cacheRoot, `webkit-${v16}-debug`, "lib", "sicudt.lib"),
];
const WEBKIT_LIB = (() => {
  const i = process.argv.indexOf("--webkit");
  if (i >= 0 && process.argv[i + 1]) return resolve(process.argv[i + 1]);
  const hit = candidates.find(p => existsSync(p));
  if (hit) return hit;
  const dirs = readdirSync(cacheRoot)
    .filter(d => d.startsWith("webkit-") && d.includes(v16))
    .map(d => ({ d, m: statSync(join(cacheRoot, d)).mtimeMs }))
    .sort((a, b) => b.m - a.m);
  for (const { d } of dirs) {
    const p = join(cacheRoot, d, "lib", "sicudt.lib");
    if (existsSync(p)) return p;
  }
  throw new Error(`sicudt.lib for webkit ${v16} not found under ${cacheRoot}`);
})();

const BASE = 144; // TOC offsets are relative to the end of UDataInfo
const ENT = 148;
// Matches the *leaf* of a locale bundle, not a whole name: `en`, `en_GB`,
// `zh_Hans_CN` pass; `eng` or `en_USA` do not. Anchor it yourself if you want
// an exact set.
const DEFAULT_KEEP = "^(root|und|en|zh)(_|$)";

const outDir = resolve(process.argv[2] || "icu-trim-out");
mkdirSync(outDir, { recursive: true });

// --- 1. pull the single member out of the prebuilt archive -----------------
// The prebuilt ships `icudt78l_dat.o`; a package this script produced ships
// `icu_data.obj`. The latter means someone copied a trimmed archive over the
// cache (what the old instructions said to do) — there is nothing to trim, so
// the build links it unchanged instead of failing on the member name.
const members = stripAnsi(execFileSync(AR, ["t", WEBKIT_LIB], { encoding: "utf8" }))
  .trim()
  .split(/\r?\n/);
if (members.includes("icu_data.obj")) {
  const libPath = join(outDir, "sicudt.lib");
  writeFileSync(libPath, readFileSync(WEBKIT_LIB));
  console.log(`${WEBKIT_LIB} already holds a trimmed package (icu_data.obj); linking it as-is`);
  console.log(`wrote ${libPath}`);
  process.exit(0);
}
if (!members.includes("icudt78l_dat.o")) {
  throw new Error(`unexpected archive members in ${WEBKIT_LIB}: ${members.join(", ")}`);
}
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
const items: { name: string; off: number; len: number; newOff?: number }[] = [];
for (let i = 0; i < COUNT; i++) {
  items.push({ name: nameOf(pkg.readUInt32LE(ENT + i * 8)), off: BASE + pkg.readUInt32LE(ENT + i * 8 + 4), len: 0 });
}
for (let i = 0; i < items.length; i++) items[i].len = (i + 1 < items.length ? items[i + 1].off : pkg.length) - items[i].off;

// --- 4. choose what to keep ------------------------------------------------
const isRes = (n: string) => /\.res$/.test(n);
const bare = (n: string) => n.replace(/^icudt78l\//, "");
// `en`, `zh_Hans_CN`, `coll/zh`, `zone/pool`, `curr/res_index` …
const leafOf = (n: string) => bare(n).split("/").pop()!.replace(/\.res$/, "");

// A locale bundle is a CLDR locale id: a 2-3 letter language (or `root`/`und`)
// followed by script/region/variant subtags. Anything else sharing the
// directory — `res_index`, `pool`, `tzdbNames`, `supplementalData`, `keyTypeData`,
// `langInfo`, `plurals`, … — is tree-wide data that ICU reads *before* it looks
// for any locale, so it is never a trim candidate. Dropping one of those takes
// the whole tree with it, which is what made earlier trims fail.
const SCRIPT = /^[A-Z][a-z]{3}$/;
const REGION = /^([A-Z]{2}[0-9]?|[A-Z]{3}|[0-9]{3})$/;
const VARIANT = /^(POSIX|TRADITIONAL|PHONEBOOK|USE|HEPBURN|ALALCAPS|VALENCIA)$/;
const isLocaleLeaf = (leaf: string) => {
  const parts = leaf.split("_");
  if (!/^(root|und|[a-z]{2,3})$/.test(parts[0])) return false;
  return parts.length <= 5 && parts.slice(1).every(p => SCRIPT.test(p) || REGION.test(p) || VARIANT.test(p));
};

// Dictionary-based word breaking for four scripts that need a per-script word
// list rather than rules. They are not locale bundles, so the locale rule above
// cannot reach them; each is dropped when its own locale is not kept
// (`cjdict.dict` covers Han/Hiragana/Katakana/Hangul and is kept by `zh`).
const DICT_LOCALE: Record<string, string> = {
  thaidict: "th",
  laodict: "lo",
  burmesedict: "my",
  khmerdict: "km",
};

const keepRe = has("--keep") ? flag("--keep") : DEFAULT_KEEP;
const keepLocale = new RegExp(keepRe, "i");
const keepAll = has("--keep-all");
const keepDicts = has("--keep-lang-dicts");

const keepItem = (name: string): boolean => {
  if (keepAll) return true;
  const b = bare(name);
  if (!isRes(name)) {
    const m = /^brkitr\/([a-z]+)\.dict$/.exec(b);
    const lang = m ? DICT_LOCALE[m[1]] : undefined;
    return !lang || keepDicts || keepLocale.test(lang);
  }
  const leaf = leafOf(name);
  // `pool.res` in every tree holds the display names of the locales that have
  // no bundle of their own — kept locales read it too (en's list of region and
  // language names is mostly pool data). Measured: dropping the pools changes
  // 110 otherwise-identical output lines for en/zh, so they are never trimmed.
  if (leaf === "pool") return true;
  if (!isLocaleLeaf(leaf)) return true;
  return keepLocale.test(leaf);
};

const keep = items.filter(it => keepItem(it.name));

const label = keepAll
  ? " --keep-all"
  : ` --keep ${keepRe}${keepDicts ? " --keep-lang-dicts" : ""}`;
console.log(`keeping ${keep.length}/${items.length} items${label}`);
for (const t of ["", "brkitr", "coll", "curr", "lang", "region", "rbnf", "unit", "zone"]) {
  const pre = t ? `icudt78l/${t}/` : "icudt78l/";
  const all = items.filter(i => bare(i.name).includes("/") === !!t && i.name.startsWith(pre));
  const kept = all.filter(i => keepItem(i.name));
  const saved = all.filter(i => !keepItem(i.name)).reduce((a, i) => a + i.len, 0);
  console.log(
    `  ${(t || "(top)").padEnd(8)} ${String(kept.length).padStart(4)}/${String(all.length).padStart(4)} items, ${(saved / 1024).toFixed(0).padStart(6)}k dropped`,
  );
}

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

// The prebuilt aligns every item to 16 bytes (U_DATA_ALIGN in ICU's data
// writer); the header, TOC and string area together must therefore be padded
// so the data area starts 16-aligned. ICU checks item alignment against
// UDataInfo and rejects misaligned normalization data with
// U_ILLEGAL_ARGUMENT_ERROR (unorm2_getNFKC/NFKD/... return NULL), so 4-byte
// alignment is not enough even though most of the package tolerates it.
const ALIGN = 16;
const alignUp = (n: number) => (n + ALIGN - 1) & ~(ALIGN - 1);

const tableLen = 4 + N * 8;
const dataBaseRel = alignUp(tableLen + strArea.length);
const chunks: Buffer[] = [];
let w = 0;
for (const it of keep) {
  w = alignUp(w);
  it.newOff = dataBaseRel + w;
  chunks.push(pkg.subarray(it.off, it.off + it.len));
  w += it.len;
}
const dataArea = Buffer.alloc(alignUp(w));
{
  let ww = 0;
  for (const c of chunks) {
    ww = alignUp(ww);
    c.copy(dataArea, ww);
    ww += c.length;
  }
}

// The string area is zero-padded out to dataBaseRel: ICU reads the data offset
// from the TOC, so the bytes between the last name and the first item must
// exist even though nothing reads them.
const strPad = Buffer.alloc(dataBaseRel - tableLen);
strArea.copy(strPad, 0);

const out = Buffer.alloc(BASE + dataBaseRel + dataArea.length);
header.copy(out, 0);
out.writeUInt32LE(N, 144);
keep.forEach((it, i) => {
  out.writeUInt32LE(strs[i].o, ENT + i * 8);
  out.writeUInt32LE(it.newOff as number, ENT + i * 8 + 4);
});
strPad.copy(out, BASE + tableLen);
dataArea.copy(out, BASE + dataBaseRel);

// Round-trip check using the same offset rule the reader uses.
for (let i = 0; i < N; i++) {
  const no = BASE + out.readUInt32LE(ENT + i * 8);
  const dobs = BASE + out.readUInt32LE(ENT + i * 8 + 4);
  let e = no;
  while (e < out.length && out[e] !== 0) e++;
  if (out.subarray(no, e).toString("latin1") !== keep[i].name) throw new Error(`name mismatch at ${i}`);
  const next = i + 1 < N ? BASE + out.readUInt32LE(ENT + (i + 1) * 8 + 4) : out.length;
  if (next - dobs < chunks[i].length) throw new Error(`entry ${i} overruns the next item`);
  if (!out.subarray(dobs, dobs + chunks[i].length).equals(chunks[i])) throw new Error(`data mismatch at ${i}`);
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
console.log(`validate: bash tmp/icu-probe/build.sh ${libPath} && tmp/icu-probe/probe.exe`);
console.log(
  `the build links this file directly (deps/webkit.ts prebuild WebKit); nothing to install by hand`,
);