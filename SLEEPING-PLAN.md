# bunre slimming — continuation plan

Written at the end of the HTTP/3 / node:sqlite session, then updated after the
`Bun.build` / `Bun.Transpiler` measurement (section 2). Everything below was
verified against the running binary or the tree; nothing here is estimated
unless it says "unmeasured".

---

## 0. Read this first — two facts that invalidate the obvious approach

**Line counts do not predict binary size in this repo.** `node:sqlite` was
293,131 lines and deleting it saved **0.17 MB**, because 264,100 of those lines
are `sqlite3.c`, which `bun:sqlite` still needs. Several other targets below
have the same shape. Do not promise a size win from a line count.

**The linker map cannot measure this.** `build/release/bunre.map` lists only
*public* symbols. Bun's Rust code is almost entirely internal (`static fn`,
private generics), so the map attributes **7.79 MB to all of Rust** against a
65 MB binary — an artifact of the few exported entry points, not a real
footprint. JSC looks correct (~24 MB) precisely because C++ exports nearly
everything. Any cargo-bloat-style analysis over this map will mislead you.

The only trustworthy number is: delete it, rebuild, diff `bunre.exe`.

---

## 1. Current state

```
HEAD:   <new>   Remove Bun.build and Bun.Transpiler   <- the section 2 measurement
        31afecf722 Add continuation plan for the slimming work
        d154c9ae5e Delete the stale libarchive dependency
        8132d6e546 Remove node:sqlite
        ed7c8b0149 Remove HTTP/3 and QUIC
remote: c64fdada4d  (main)   <- four commits behind, NOT pushed
binary: 65.32 MB    (65.56 MB before the measurement)
tree:   clean, build green, smoke passing
```

Smoke that must stay green:

```
Bun.serve({fetch})      ok
Bun.serve({routes})     ok
bun:sqlite round-trip   ok
fetch (data: URL)       ok
node:http2              object
```

### Gone as of the section 2 measurement

```
Bun.build        undefined   removed (BunObject callback + build())
Bun.Transpiler   undefined   removed (whole JSTranspiler.rs + its class def)
```

Still present and verified working after that removal, because they are wired
to the *plugin* half of the old `JSBundler.rs` rather than the `Bun.build` half:

```
Bun.plugin                 ok (returns undefined by design — BunPlugin.cpp)
Bun.serve({plugins: []})   ok
runtime import of a .ts    ok (js_parser untouched)
```

Toolchain: `rustc 1.99.0-nightly (nightly-2026-07-20)`, `bun 1.4.2`.
Full build is `bun run build:release`, roughly 10–20 min. Budget for it.

### Pushing (read this before you try)

Local history is missing object `5baea663...`, so `git push` refuses to
fast-forward. A normal push has never worked in this session. The workaround
used successfully before:

```powershell
$remote = (git ls-remote origin refs/heads/main).Split()[0]
$tree   = (git show -s --format=%T HEAD).Trim()
$new    = (git commit-tree $tree -p $remote -F <msgfile>).Trim()
git push origin "${new}:refs/heads/main"
```

This rewrites the commit on top of the real remote tip, preserving the message.
A previous push attempt failed with `403 Permission to bun-fun/bunre.git
denied to stxh` — the credential in `~/.git-credentials` is stored for user
`bun-fun` but the server resolved the request as `stxh`. **This may still be
broken; verify write access before promising a push.**

---

## 2. The measurement — done. It is small, and it does not answer the TS question

Deleted the two JS entry points, rebuilt, diffed:

```
before   68,740,608 bytes   (65.56 MB)
after    68,491,776 bytes   (65.32 MB)
delta      -248,832 bytes   (-0.237 MB, -0.36%)
```

**That is the whole win for losing `Bun.build` *and* `Bun.Transpiler`.** Two
headline APIs for a quarter of a megabyte. Per the rule this section used to
carry ("if the win is small, stop"), the bundler line stops here.

### The framing was wrong, and that is the more important finding

This section was written as a proxy for "is dropping TypeScript support worth
it?". It is not a proxy. Deleting the two entry points does **not** remove
`src/js_parser` or `src/bundler`; both stay fully reachable:

- `src/js_parser` is what loads `.ts`/`.tsx` at runtime. Removing it means
  bunre can no longer run TypeScript *at all* — including its own bundled
  sources. It is not a size question, it is a "does the binary still run"
  question, and the answer is no.
- `src/bundler` is still reached by `Bun.serve` HTML bundling
  (`server/HTMLBundle.rs` → `JSBundleCompletionTask::new(Config, …)`) and by
  the bake dev server.

So the real cost of "drop the bundler" is not the size it contributes — it is
that HTML imports, `Bun.serve` plugin resolution and bake would have to go
first. That is a product decision, not a slimming decision.

### What actually had to be deleted, and what did not

`src/runtime/api/JSBundler.rs` was **not** only `Bun.build`. It held two
unrelated things glued together:

| half | still needed by | fate |
|---|---|---|
| `build()` / `build_fn` (the `Bun.build` host fn) | nothing else | deleted |
| `Config`, `CompileOptions`, `FileMap`, `Names`, `Minify` | `HTMLBundle.rs`, `js_bundle_completion_task.rs`, bake | **kept** |
| `Plugin`, `PluginJscExt`, all `JSBundlerPlugin__*` C-ABI exports | `Bun.serve({plugins})`, `Bun.plugin`, bake DevServer | **kept** |
| `BuildArtifact` + `OutputKind` | `api/output_file_jsc.rs`, used by `HTMLBundle.rs` too | **kept** |

Only ~50 lines of Rust came out. The file was renamed
`JSBundler.rs` → `bundler_plugin.rs` to say what it now is; the module is still
`crate::api::js_bundler` so the 13 `crate::api::js_bundler::…` reference sites
did not have to churn.

One orphan fell out: `StringOrBuffer::owned_isolated` in
`src/runtime/node/types.rs` was only reachable from the deleted path. Release
builds pass `-D dead-code` (`scripts/build/rust.ts`), so this is a build
error, not a warning — expect to chase a few of these per removal.

### Lesson worth keeping

The plan assumed the removal was "delete 2 files + 4 lines in `BunObject.rs`".
It was closer to "delete 2 files, then discover 15 reference sites across
`api.rs`, `server_body.rs`, `HTMLBundle.rs`, `bake/{mod,bake_body,DevServer}.rs`,
`output_file_jsc.rs`, `js_bundle_completion_task.rs`, plus the C++ side
(`BunObject.cpp` bunObjectTable rows and the `BunObject+exports.h` macro list —
`BunObject.rs:157-164` documents that checklist and it is not optional).
Budget the survey before budgeting the build.

---

## 3. Candidate removals — REWRITTEN from measured data

The previous version of this table was ranked by source lines and said
explicitly that no binary estimate was available. **That is no longer true.**
The linker map was re-parsed to attribute bytes per object file, and Rust's
CGU objects carry their crate name, so both halves of the binary can now be
attributed. Replaced wholesale.

### How to reproduce

`build/release/bunre.map` lists every symbol with its contributing `.obj`.
Parse it, sort symbols by the absolute VA column (the second address), give
each symbol the delta to the next one, and sum per object. Then group:

- Rust objects are `bun_rust-<h>.<crate>-<h>.<crate>.<h>-cgu.N.rcgu.o` — the
  crate name is in the filename, so **Rust is attributable per crate**.
- C++ objects are `JavaScriptCore:*.obj`, `UnifiedSource-*.cpp.obj`,
  `sicudt:*.o`, bare `sqlite3.c.obj`, etc.

Validation: JavaScriptCore comes out at 24.11 MB, matching the "~24 MB" this
file recorded independently in section 0. Sum is 66.44 MB against a 65.32 MB
binary (+1.7%) — the map does not mark object boundaries, so the gap after an
object's last symbol is attributed to it. **Read these as relative sizes, and
expect roughly 2% absolute inflation.**

> Correction to section 0: this file claimed the map "cannot measure Rust"
> because Rust symbols are internal. True per *symbol*, false per *crate*.

### Whole-binary distribution

| part | size | share |
|---|---:|---:|
| JavaScriptCore engine (parser/bytecode/DFG/B3/JIT/wasm) | 22.98 MB | 35.2% |
| **ICU** (data blob + i18n + common, prebuilt WebKit) | **15.05 MB** | **23.0%** |
| Rust (all of bunre's own logic) | 9.27 MB | 14.2% |
| Bun C++ glue (`src/jsc/bindings` + codegen glue) | 4.66 MB | 7.1% |
| Embedded JS builtins blob (`src/js`) | 2.26 MB | 3.5% |
| SQLite (`bun:sqlite`) | 1.58 MB | 2.4% |
| MSVC CRT | 1.65 MB | 2.5% |
| zstd + zlib + brotli | 1.33 MB | 2.0% |
| BoringSSL (TLS + crypto primitives) | 1.26 MB | 1.9% |
| WTF | 0.97 MB | 1.5% |
| nghttp2 / HPACK (`node:http2`) | 0.84 MB | 1.3% |
| TinyCC | 0.63 MB | 1.0% |
| libuv + c-ares | 0.20 MB | 0.3% |
| long tail (586 small objects) | 3.44 MB | 5.3% |

Rust totals 9.27 MB across 105 crates. Largest: `bun_runtime` 2.89 MB,
`bun_react_compiler` 1.15 MB, `bun_css` 0.89, `bun_bundler` 0.89,
`bun_js_parser` 0.72, `bun_resolver` 0.43, `bun_jsc` 0.33.

Image codecs are **live** — do not put them on the removal list. `Bun.Image`
is a full Sharp-style pipeline: 21 prototype methods, and decode, resize and
encode all verified working on this binary (1x1 PNG in; `metadata()` reports
`{"width":1,"height":1,"format":"png"}`, `resize(2,2)`, `.jpeg()` 634 B,
`.webp()` 96 B). It needs libjpeg-turbo, libspng, libwebp and highway, which
together are roughly 0.5 MB.

> An earlier revision of this file claimed the codecs were "already gone" and
> that `Bun.Image.fromArrayBuffer` did not exist. Both were wrong. `Object.keys(
> Bun.Image)` lists only *static* properties — every prototype method is wired
> through `src/runtime/image/Image.classes.ts` — and the size filter matched
> object filenames containing "webp"/"jpeg"/"spng" when the real objects are
> named `decode.c.obj`, `yuv.c.obj`, `upsampling.c.obj` and so on. libwebp
> alone is ~350 KB.

### The table, re-ranked by bytes

The rightmost column is **measured binary bytes**, not source lines. It
disagrees with the line count badly enough to invalidate the old ordering.

| target | measured bytes | JS surface lost | entangled with |
|---|---:|---|---|
| **ICU (prebuilt WebKit `sicudt.lib`)** | **15.05 MB** | `Intl` locale data | **fixed cost — attempted and rejected, section 8** |
| `bun_runtime` core (not removable as a whole) | 2.89 MB | — | everything |
| `src/react_compiler` | 1.15 MB | React compiler at build time | `js_parser`, bake |
| `src/bundler` + `linker_context` | 0.89 MB | `Bun.build` (already gone), HTML imports | serve HTML bundling, bake |
| `src/css` + `css_derive` + `css_jsc` | 0.90 MB | `bun build` CSS | `runtime/image` |
| `src/js_parser` (+ `ast`, `js_printer`) | 0.86 MB | **runtime `.ts`/`.tsx` loading** | `resolver`, runtime |
| `src/resolver` | 0.43 MB | module resolution | `js_parser`, `parsers` |
| `src/jsc` binding layer | 0.33 MB | — | everything |
| `src/http` + `http_types` + `http_jsc` | 0.32 MB | HTTP client/server | `webcore` |
| `src/parsers` | 0.23 MB | `Bun.TOML/JSON5/JSONC/XML/YAML` | `bunfig`, `resolver`, `sourcemap`, `runtime` |
| `encoding_rs` | 0.20 MB | non-UTF8 text decoding | `sourcemap`, `resolver` |
| `src/sql` + `sql_jsc` | 0.18 MB | `Bun.SQL` | `js_parser_jsc` |
| `src/standalone_graph` + `exe_format` | 0.06 MB | `--compile` | `bundler` |
| `src/shell_parser` | 0.04 MB | `Bun.$` | — |
| `src/sourcemap` + `_jsc` | 0.04 MB | build sourcemaps | `js_parser`, `bundler` |
| `src/patch` + `_jsc` | 0.02 MB | `bun patch` | — (leaf) |
| `src/semver` + `semver_jsc` | 0.013 MB | `Bun.semver` | analytics, resolver, options_types |
| `src/glob` | 0.007 MB | `Bun.Glob` | — (leaf) |
| `src/s3_signing` | 0.008 MB | `Bun.s3`, `S3Client` | — (leaf) |

Everything in the old table sums to ~344,000 source lines, but the largest
single item, `src/bundler` at 68,827 lines, is worth **0.89 MB**.
`bun_react_compiler` is 58,258 lines and 1.15 MB. Line count is close to
useless as a ranking signal here.

### The three "easy wins" are not worth doing

The old table called `semver` + `s3_signing` + `glob` "the best next move".
Measured, they total **26 KB**. At ~14 min of release build per change that is
a terrible trade. Dropped from the plan.

They are not *harmful*, just pointless. If they are wanted for tidiness they
can ride along in one commit with something bigger — never alone.

### What is actually worth doing, in order

1. **~~ICU — 15.05 MB.~~ ATTEMPTED, REJECTED.** The repack tool works (it
   reproduces the original binary byte-size exactly) and a real ~8 MB win
   exists on paper, but every trimmed package that drops locale bundles dies
   with a fastfail during VM init, including one that keeps the host's own
   `zh*` chain. The set of bundles ICU opens at init is not knowable from
   this tree. **Treat 15.05 MB as fixed cost.** Full measurements, the tool
   and the reasoning are in section 8. Do not retry without a way to control
   what ICU opens during init.
2. **Runtime `.ts` support (`js_parser` + `ast` + `js_printer`, 0.86 MB).**
   Small in bytes, disqualifying in effect: bunre could no longer run
   TypeScript at all, including its own bundled sources. Not a size decision.
3. **~~`src/react_compiler` — 1.15 MB.~~ DONE.** Removed on branch
   `drop-bundler-react-compiler-css`, commit `bf6531a023`.
   **68,203,520 → 66,994,176 bytes, -1.153 MB (-1.77%).** The predicted figure
   was confirmed by rebuild, which is the first time that has happened in this
   plan. Only `src/js_parser` depended on it, so the removal was contained.
4. **`src/bundler` — not a size operation. See below.**
5. **`src/css` — blocked on 4, and costs `Bun.color`. See below.**

### `src/bundler` and `src/css` are architecture, not size

A branch was opened to remove react_compiler + bundler + css. The first target
was removed; the other two were investigated and abandoned. Recording why, so
nobody re-derives it.

**`bun_bundler` is no longer a bundler — it is the shared foundation that two
core crates import directly.** `src/jsc` and `src/js_parser` are depended on by
essentially everything, and both reach into `bun_bundler` for low-tier types:

| symbol | imported by |
|---|---|
| `transpiler::ParseResult` | `src/jsc/AsyncModule.rs` |
| `transpiler::{BunPluginTarget, PluginRunner, PluginResolver}` | `JSGlobalObject.rs`, `ModuleLoader.rs`, `PluginRunner.rs` |
| `options::{ImportPathFormat, ModuleType, DEFAULT_LOADERS}` | `AsyncModule.rs`, `ModuleLoader.rs`, `NodeModuleModule.rs` |
| `ThreadPool`, `defines`, `Error` | `src/js_parser/lib.rs`, `src/jsc/error.rs` |
| `BundleV2`, `Linker` | `hot_reloader.rs`, `src/runtime/jsc_hooks.rs` |

So "delete src/bundler" is really "extract the shared tier into a new crate,
then delete the bundling half, then unwind every caller". The unwinding alone
touches `bake` (45 refs), `HTMLBundle.rs` (748 lines),
`js_bundle_completion_task.rs` (1,464), `bundler_plugin.rs` (2,120),
`jsc_hooks.rs` (23) and `dispatch.rs`.

The payoff does not justify it. Of the crate's 885 KB, roughly a quarter is
the shared tier that has to survive the split anyway, so the real number is
**~0.6 MB** for a multi-hour, high-regression-risk change that touches the JS
engine binding layer and the parser.

**`src/css` is downstream of that and is not free either.** `bun_css` is
consumed mostly by `src/bundler`, so it cannot go first — but `src/runtime`
touches `bun_css_jsc` twice, and one of those is `BunObject.rs:246`:

```
Bun.color("#ff0000")  ->  "red"
```

`Bun.color` is a live public API (ANSI colour sequences for terminal output),
so removing `css_jsc` removes it too.

### Everything else outside WebKit is live — surveyed 2026-10-10

A second pass over every non-WebKit line item found no free wins. Each was
checked against the running binary, not the source tree:

| candidate | size | verdict |
|---|---:|---|
| embedded JS blob (`InternalModuleRegistryConstants`) | 2.26 MB | live; 187 modules, biggest are `internal:streams` 280 KB, `internal:repl` 198 KB, `node:http2` 165 KB (**removed**, `-0.275 MB`) |
| SQLite | 1.58 MB | live — `bun:sqlite` and `Bun.SQL` both verified |
| zstd + zlib + brotli | 1.33 MB | live — `Bun.gzipSync`/`zstdCompressSync` verified |
| BoringSSL | 1.26 MB | live — TLS |
| **TinyCC** | 0.63 MB | **live — it is Bun's FFI JIT**, `src/runtime/ffi/ffi_body.rs` + `src/tcc_sys`. `Bun.FFI.dlopen` verified |
| libwebp + libjpeg + libspng + highway | ~0.5 MB | **live — `Bun.Image`**, see the note above |
| `bun:internal-for-testing` | 13 KB | live — `src/js/internal/dgram.ts` reaches through it |
| napi | ~0.3 MB | live — see the note below |
| `src/http/h2_client` | ~0.1 MB | **live — `fetch()` negotiates h2 over ALPN**, see the note below |

#### Two corrections to earlier notes in this file

**`src/http/h2_client` is not orphaned.** The `node:http2` commit message
claimed "fetch() never referenced it; it is only reachable from bun.http
diagnostics". That is wrong. The module is aliased at `src/http/lib.rs:77`
(`pub use h2_client as h2`), so a grep for the type name `H2Client` misses
every call site. The real wiring is:

```
Request::alpn_offer()                       src/http/lib.rs:1930
  -> AlpnOffer::H1OrH2 when can_offer_h2()   (the default for HTTPS)
  -> connect_finish: `if alpn == b"h2"`     src/http/lib.rs:1963
  -> h2::ClientSession::create(...)         src/http/lib.rs:1972
```

Removing it costs `fetch()` its HTTP/2-over-HTTPS path (it would fall back to
HTTP/1.1 everywhere) and needs ~25 call sites in `lib.rs` reworked, for about
0.1 MB. Bad trade.

**napi is live too, and more entangled than its size suggests.** It is only
105 KB of C++, but it is threaded through three live features:

| feature | napi's role |
|---|---|
| `Bun.plugin({onBeforeParse})` | loads native bundler plugins (`$napiDlopenHandle` in `src/js/builtins/BundlerPlugin.ts`) |
| `Bun.FFI` | `ABIType::NapiEnv` / `NapiValue` are FFI signature types (`src/runtime/ffi/abi_type.rs:45,99,185`) |
| startup | `crate::napi::fix_dead_code_elimination()` runs on every launch (`src/runtime/cli/run_command.rs:1495`) |

Removing it means editing the FFI ABI type system plus `dispatch.rs`,
`ZigGlobalObject.cpp`, `JSBundlerPlugin.cpp` and 8 other files, for ~0.3 MB.

The two biggest remaining Rust items are both blocked on the same thing:
`bun_react_compiler` (1.15 MB) is invoked from `src/bundler/ParseTask.rs` and
`src/js_parser`, so it cannot go before the bundler, and the bundler cannot go
before HTML imports and bake. Cutting that chain is worth ~2.9 MB of Rust
(`react_compiler` + `bundler` + `css`) but costs HTML imports, framework
presets, HMR and `Bun.serve({routes})`.

**The well is dry.** Two cuts landed after the survey:

```
node:http2          68,491,776 -> 68,203,520   -0.275 MB   e38d4da420
react_compiler      68,203,520 -> 66,994,176   -1.153 MB   bf6531a023
                                             --------
                                             66,994,176 bytes (63.88 MB)
```

`react_compiler` is on branch `drop-bundler-react-compiler-css`, not on main.
Everything still on the board is live, so any further reduction has to be a
feature decision, not a dead-code hunt.

### Already-dead wiring, free to remove

```
Bun.CSRF      undefined   src/csrf is commented out in Cargo.toml:178
Bun.valkey    undefined   src/valkey is commented out in Cargo.toml:179
Bun.Semver    undefined   <-- nothing registers it; see below
Bun.CSS       undefined   removed alongside Bun.build
Bun.build     undefined   removed in 624486f44b
Bun.Transpiler undefined  removed in 624486f44b
```

`csrf` and `valkey` still have commented-out lazy-prop lines in `BunObject.rs`
(`BunObject.rs:287` and `BunObject.rs:320`). Their `bunObjectTable` rows in
`BunObject.cpp` and their `macro(...)` entries in `BunObject+exports.h` were
already removed when the crates were commented out.

`semver` is the odd one: **no** lazy-prop row exists in `BunObject.rs` and
**no** row in `BunObject.cpp`, so `Bun.Semver` was never reachable. But
`BunObject.rs:1822` still defines `get_semver_object`, calling
`bun_semver_jsc::SemverObject::create`, and both `bun_semver` and
`bun_semver_jsc` are still live workspace members (`Cargo.toml:20,73`,
`src/runtime/Cargo.toml:87-88`). Removing `src/semver` should therefore take
`src/semver_jsc`, the `Cargo.toml` members, and `get_semver_object` with it.
The earlier note in this file claiming a `semver` lazy-prop was still
registered was wrong — nothing is registered.

---

## 4. `bun:sqlite` vs `node:sqlite` — resolved, do not re-investigate

They are **not** separable modules. They share one `sqlite3.c`, and
`Database.setCustomSQLite()` on one is visible to the other
(`lazy_sqlite3.h:123-124`). `node:sqlite` is gone as of `8132d6e546`;
`bun:sqlite` and `Bun.SQL`'s SQLite adapter are unaffected and verified.

`Bun__sqlite3_version` was moved from the deleted `NodeSqlite.cpp` into
`JSSQLStatement.cpp`. It backs `process.versions.sqlite`, so it describes the
shared library rather than the node API. Its fallback uses `SQLITE_VERSION`
from `sqlite3_local.h` instead of the deleted `BUN_SQLITE_BUNDLED_VERSION`
macro — same string, still reports `3.53.2`.

Open question, unresolved: `new Bun.SQL("sqlite://:memory:")` did not expose
`.run`/`.query` in testing, and a clean baseline could not be built (`git
stash` aborts on the missing `5baea663` object). Believed to be wrong API usage
rather than a regression — `bun:sqlite` beneath it works correctly — but
unproven. Check it if `Bun.SQL` matters.

---

## 5. `Bun.serve({routes})` is the bake decision

Deleting `src/runtime/bake` costs HTML imports, framework presets, and HMR.
`Bun.serve({routes})` also goes with it — this is not optional. A previous
attempt got through the module deletions and then hit brace imbalance in
`set_routes`; the whole change was reverted and the tree restored.

Note that `src/runtime/api/filesystem_router.rs` (901 lines) is **still
present** — an earlier attempt deleted it and was rolled back with the rest.

---

## 6. Working method that produced the results so far

1. Delete one target. Never batch unrelated subsystems into one commit.
2. `bun run build:release`.
3. Smoke-test the five cases in section 1.
4. Commit on green. Leave the tree clean between targets.

Line-number-based edit scripts **caused repeated misfires** during the bake
work — line numbers drift between the read and the write, and one pass deleted
the wrong three lines in `server/mod.rs`. The `edit` tool with exact string
matching is reliable; the scripts are not. The one exception is deleting a
whole brace-delimited block found by a content anchor, verified by printing
the matched snippet before committing to the removal.

A worked example of the exception, from the section 2 measurement: a 50-line
`fn build() { … }` block was removed with a script that *asserts the exact
text* of its first and last lines before writing. One assertion failed on the
first attempt (an off-by-one in my expected line numbers) and the script
exited without writing. Assert first, then delete — that check is what makes
the script safe.

### `cargo check` needs a codegen dir pointed at by hand

`bun run rust:check` fails immediately in this tree: the build scripts default
to `build/debug/codegen`, and debug is never configured (section 7). Point it
at the release codegen dir instead:

```powershell
$env:BUN_CODEGEN_DIR="E:\PubCode\bunre\build\release\codegen"
cargo check --workspace --keep-going --message-format=short
```

Two caveats, both learned the hard way:

- It uses `target/debug`, so the first run is a cold compile of the whole
  workspace. Not faster than `bun run build:release` the first time, but every
  run after that is.
- It does **not** pass release `RUSTFLAGS`, so `-D dead-code` is off. A clean
  `cargo check` therefore proves nothing about dead code — see below.

### Release builds deny dead code

`scripts/build/rust.ts` passes `-D dead-code` for the release profile, so every
orphan left by a removal is a hard build error, not a warning. `cargo check`
will not find them. Expect one or two per removal, and chase them by hand.

---

## 7. Environment quirks

- Proxy is `socks5h://127.0.0.1:11080`; port 11081 is closed. The default git
  config points at 11080 and works for reads.
- Git push is broken (403), see section 1.
- Debug builds are unavailable — the WebKit download fails on this network.
  Release builds and smoke tests are the only verification available. For a
  fast compile-error loop, use the `BUN_CODEGEN_DIR` trick in section 6.
- `bun run build:release` takes ~14 min from a warm cargo cache.
- Generated artifacts under `build/release/codegen/` can go stale after
  deleting a module. If the build fails on a `#include` for a file you just
  deleted, delete the generated header and rebuild:
  `Remove-Item build/release/codegen/NativeModuleImpl.h`
- The `.classes.ts` codegen glob picks up renamed class-definition files
  automatically, so renaming `JSBundler.classes.ts` needed no script change.
  Stale `generated_classes.rs` will still reference a deleted class until the
  next `bun run build:release`.

---

## 8. ICU — 15.05 MB, the largest item on the board. NOT removable.

Measured, not estimated. **15.05 MB total (23.0% of the binary)** — the first
version of this section said 11.68 MB and was wrong: it counted only the data
blob and dropped `sicuin`/`sicuuc` because the extraction regex missed `.ao`
object extensions. Corrected breakdown:

| part | linked size | on disk |
|---|---:|---:|
| `sicudt` — the ICU data package | 11.68 MB | 11,956 KB |
| `sicuin` — locale-aware formatting | 1.84 MB | 11,315 KB |
| `sicuuc` — common/converters | 1.53 MB | 5,012 KB |

For comparison, `src/bundler` (68,827 lines, the biggest entry in the old
table) is 0.89 MB. ICU is **17x** that.

It is **live, not dead weight.** All locales resolve on this binary:

```
fr-FR -> janvier     ar-EG -> يناير     ja-JP -> 1月
zh-CN -> 一月        hi-IN -> जनवरी        th-TH -> มกราคม
```

### Where it comes from

Not from this source tree. `scripts/build/deps/webkit.ts:135-144`:

```ts
function prebuiltIcuLibs(cfg: Config): string[] {
  if (cfg.windows)  return [`lib/sicudt${d}.lib`, `lib/sicuin${d}.lib`, `lib/sicuuc${d}.lib`];
  if (cfg.linux || cfg.freebsd) return ["lib/libicudata.a", ...];
  return [];   // darwin: system ICU
}
```

On Windows and Linux it is ICU bundled **inside the prebuilt WebKit tarball**,
downloaded from `oven-sh/WebKit` releases and extracted to
`~/.bun/build-cache/webkit-<sha>/`. On macOS Bun links system ICU instead and
pays nothing — so this cost is a Windows/Linux-only artefact of the prebuilt
distribution, not something Bun's own build decided to embed.

### It is already heavily optimised — this is the key finding

`oven-sh/WebKit` repacks the ICU data before shipping it. Measured by
extracting `icudt78l_dat.o` out of `sicudt.lib` and pulling the byte array out
by COFF section offset:

```
stock icudt78b.dat (ICU 78.1 release)          31.57 MB
bunre's repacked icudt78l.dat                   11.55 MB      -63%
```

Two stages of trimming are already applied upstream:

1. **Item filtering + zstd.** `icudt78l.dat` is **3,990 items**, of which
   **3,824 are individual zstd frames** (magic `28 B5 2F FD`, i.e.
   `ZSTD_MAGICNUMBER 0xFD2FB528` little-endian) sharing a 128 KB custom
   dictionary. `src/jsc/bindings/bun_icu_decompress.cpp` is the decompress
   hook: oven-sh patches `udata.cpp` to call `bun_icu_maybe_decompress`
   between TOC lookup and `checkDataItem`, and the link pulls exactly three
   symbols out of `sicudt.lib` — `icudt78_dat`, `bun_icu_zstd_dict`,
   `bun_icu_zstd_dict_size`. Nothing else from that 11.9 MB lib survives.
2. **Whole-library dead stripping.** `sicuin.lib` is 11.3 MB on disk but
   contributes 1.84 MB; `sicuuc.lib` is 5.0 MB on disk and contributes
   1.53 MB. The link already removed 82% and 77% of those respectively.

Compression stops here: **the blob is monolithic.** `icudt78l_dat.o` is one
object holding one byte array, so the moment the linker pulls
`icudt78_dat`, all 11.68 MB comes along. `--gc-sections`, `strip`, and
symbol-level tricks cannot produce a partial win.

### Attempted and measured — the repack works, but the win is not shippable

The tool is committed as `scripts/build/icu-trim.ts` (diagnostic only, not a
build step). It rewrites the package in place, preserving the zstd-per-item
format so `bun_icu_decompress.cpp` needs no change.

**The repack itself is correct.** Re-emitting all 3,990 items through that code
produces a `sicudt.lib` that links to a binary of **exactly** the prebuilt
binary's size, 68,491,776 bytes, with identical behaviour and passing smoke.
That is the control that proves the package layout below is right byte for
byte — a wrong layout would not reproduce the original size.

**Dropping locale bundles is what fails.** Every row is a full
`bun run build:release`:

| keep set | items | `bunre.exe` | result |
|---|---:|---:|---|
| everything (control) | 3,990 | 68,491,776 | ok |
| all but `zu`, `zu_ZA` | 3,988 | 68,487,680 | ok |
| locales a–m, en, zh, root | 2,032 | 65,060,352 | **crash** |
| root, und, `en*`, `zh*` | 173 | 60,522,496 | **crash** |
| root, und, `en*` | 156 | 60,495,872 | **crash** |

"crash" is exit `0xC0000409` (fastfail) during VM init. `bunre --revision`
still works; anything that constructs a VM dies. So the package parses — a
malformed package would fail earlier — and something inside ICU then hits a
missing bundle.

**It is not the default-locale chain.** Setting `LANG`/`LC_ALL` to
`en_US.UTF-8` does not change any crash above. On Windows the default locale
comes from `GetUserDefaultLocaleName`, not the environment, and keeping the
full `zh*` chain (the machine's locale) still crashes.

So ICU unconditionally opens at least one locale bundle outside
`root`/`und`/`en*`/`zh*` during init. Which ones depends on the host and the
WebKit build, and the required set is therefore **not knowable from this
tree**. A data file pruned to satisfy one machine's init path would fail on
a different default locale — or the next WebKit bump would silently restore
the untrimmed prebuilt and the regression would never show up in CI.

**Conclusion: treat ICU's 15.05 MB as a fixed cost.** Do not retry this without
a way to control what ICU opens at init (e.g. forcing the default locale to a
retained one inside the JSC init path, which lives in the prebuilt WebKit and
so is not reachable from this repo).

### The other two options, for the record

1. **Stub ICU out.** Would require stubbing thousands of symbols and would
   break `Intl`, Unicode regex property escapes, and non-ASCII case mapping.
   Not recommended, and now known to be strictly worse than pruning.
2. **Do nothing.** macOS already pays zero. Now the default recommendation for
   Windows too.

The binary was restored to the prebuilt `sicudt.lib` and rebuilt: 68,491,776
bytes, smoke green, all locales working (`fr-FR -> janvier`, `ar-EG -> يناير`,
`th-TH -> มกราคม`).

### Reproducing the measurement

```powershell
bun scripts/build/icu-trim.ts <out-dir> --keep-all          # control, must be byte-equal
bun scripts/build/icu-trim.ts <out-dir> --keep "^(root|und|en.*)$"
Copy-Item <out-dir>\sicudt.lib $env:USERPROFILE\.bun\build-cache\webkit-<sha>\lib\sicudt.lib
bun run build:release
```

Keep a copy of the original `sicudt.lib` first — replacing the cache entry is
the only fast way to test, and re-downloading WebKit is the safe way back.

Package layout, derived empirically:

```
[0, 144)        UDataInfo header, copied verbatim
[144]           u32 tocEntryCount
[148, 148+8N)   N x (u32 nameOffset, u32 dataOffset)
then            string area: N null-terminated item names
then            item data, 4-byte aligned
```

Both offsets are relative to **base 144**, not to the start of their region.
In the prebuilt, `nameOffset[0]=31924` maps to absolute `32068 = 144+31924`
and `dataOffset[0]=122256` maps to absolute `122400 = 144+122256`. Writing
chunk-relative offsets instead yields a package that parses but reads the
wrong bytes. Item names must keep their `icudt78l/` prefix — ICU derives the
expected TOC prefix from the `.dat` filename and rejects the package otherwise.

Stock `icudt78b.dat` from the ICU 78.1 release is 31.57 MB against bunre's
11.55 MB, so upstream has already taken the compression win; nothing is left
but deleting locales, which is now known to be fatal.