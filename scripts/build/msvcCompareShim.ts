/**
 * MSVC STL `<compare>` shim for native-Windows clang-cl builds.
 *
 * The MSVC STL ships `<compare>` with a deleted-constructor "trick"
 * per ordering type:
 *
 *     template <int> // Prevent unwanted construction without interfering with overload resolution or the MSVC ABI.
 *     constexpr partial_ordering() = delete; // constexpr is necessary, see N5032 [basic.types.general]/10.5.3.4.
 *
 * clang-cl honors that deleted ctor the same way MSVC cl does *except* for
 * the return-ABI of `std::strong_ordering`-returning functions: with the
 * trick it lowers them as SRET (hidden struct pointer `%rcx`), without it
 * as REGISTER (`%al`). The prebuilt WebKit was compiled with the REGISTER
 * convention, so locally-compiled bun C++ that calls into WTF (e.g.
 * `WTF::codePointCompareLessThan` via `new Date(...).toString()`) corrupts
 * the stack when both sides disagree on where the return value lands.
 *
 * The fix: copy the installed `<compare>`, drop the 3 trick blocks, and put
 * the copy in `buildDir/msvc-stl-shim/`. An attached `-imsvc<dir>` system
 * include flag (clang-cl searches it before the `INCLUDE` env — verified)
 * makes every bun TU + the PCH read the trick-less header, so clang-cl
 * classifies `strong_ordering` returns as REGISTER, matching WebKit.
 *
 * Generation happens at configure time (writeIfChanged, like
 * `generateDepVersionsHeader`) — the input is the host's installed STL,
 * which doesn't change during a build. See workarounds.ts ("msvc-stl-
 * compare-deleted-ctor") for the self-obsoleting check.
 */

import { existsSync, readFileSync, mkdirSync } from "node:fs";
import { isAbsolute, join } from "node:path";
import type { Config } from "./config.ts";
import { writeIfChanged } from "./fs.ts";

/** The `template <int>` line that opens a deleted-ctor trick block. */
const TRICK_LINE = /^\s*template <int>(\s*\/\/.*)?$/;
/** The `constexpr <X>_ordering() = delete;` line that closes it. */
const DELETE_LINE = /^\s*constexpr (?:partial_ordering|weak_ordering|strong_ordering)\(\) = delete;.*$/;

/**
 * Locate the installed MSVC STL `<compare>` via the dev shell's `INCLUDE`
 * env. Returns undefined when the STL isn't resolvable (non-Windows host,
 * no dev shell, MSVC not installed). `yvals_core.h` marks the actual MSVC
 * STL directory (Windows SDK dirs in INCLUDE don't carry it).
 */
export function installedMsvcStlCompare(): string | undefined {
  const inc = process.env.INCLUDE;
  if (!inc) return undefined;
  for (const entry of inc.split(";")) {
    const dir = entry.trim();
    if (!dir || !isAbsolute(dir)) continue;
    if (existsSync(join(dir, "yvals_core.h")) && existsSync(join(dir, "compare"))) {
      return join(dir, "compare");
    }
  }
  return undefined;
}

/** Does the installed (host) MSVC STL `<compare>` still carry the trick? */
export function installedStlCompareHasTrick(): boolean {
  const installed = installedMsvcStlCompare();
  if (installed === undefined) return false;
  try {
    return compareHasTrick(readFileSync(installed, "utf8"));
  } catch {
    return false;
  }
}

/** True when `<compare>` content contains at least one deleted-ctor trick block. */
export function compareHasTrick(content: string): boolean {
  const lines = content.split(/\r?\n/);
  for (let i = 0; i + 1 < lines.length; i++) {
    const line = lines[i]!;
    const next = lines[i + 1]!;
    if (TRICK_LINE.test(line) && DELETE_LINE.test(next)) return true;
  }
  return false;
}

/**
 * Copy of `<compare>` content with every `template <int>` /
 * `constexpr X() = delete;` pair removed. Line-based so it tolerates
 * comment-text drift between STL releases.
 */
export function stripCompareTrick(content: string): string {
  const lines = content.split(/\r?\n/);
  const out: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    const next = lines[i + 1];
    if (TRICK_LINE.test(line) && next !== undefined && DELETE_LINE.test(next)) {
      i++; // skip both lines
      continue;
    }
    out.push(line);
  }
  return out.join("\n");
}

/**
 * Write the trick-stripped `<compare>` into buildDir when the installed STL
 * still has the trick. No-op otherwise (no bug to work around — clang-cl
 * classifies REGISTER without the trick, matching WebKit). Idempotent via
 * writeIfChanged, and depfiles (/showIncludes) track the shim so an STL
 * update that changes it rebuilds the affected TUs.
 */
export function generateStlCompareShim(cfg: Config): void {
  if (cfg.msvcStlCompareShimDir === undefined) return;
  const installed = installedMsvcStlCompare();
  if (installed === undefined) return;
  let content: string;
  try {
    content = readFileSync(installed, "utf8");
  } catch {
    return;
  }
  mkdirSync(cfg.msvcStlCompareShimDir, { recursive: true });
  writeIfChanged(join(cfg.msvcStlCompareShimDir, "compare"), stripCompareTrick(content));
}