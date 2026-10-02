import { spawnSync } from "bun";
import { dlopen, FFIType } from "bun:ffi";
import { describe, expect, test } from "bun:test";
import { bunEnv, bunExe, isDebug, isMusl, isWindows, tempDir } from "harness";
import fs from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

describe("bun", () => {
  describe("NO_COLOR", () => {
    for (const value of ["1", "0", "foo", " "]) {
      test(`respects NO_COLOR=${JSON.stringify(value)} to disable color`, () => {
        const { stdout } = spawnSync({
          cmd: [bunExe()],
          env: {
            NO_COLOR: value,
          },
        });
        expect(stdout.toString()).not.toMatch(/\u001b\[\d+m/);
      });
    }
    for (const value of ["", undefined]) {
      // TODO: need a way to fake a tty in order to test this,
      // and cannot use FORCE_COLOR since that will always override NO_COLOR.
      test.todo(`respects NO_COLOR=${JSON.stringify(value)} to enable color`, () => {
        const { stdout } = spawnSync({
          cmd: [bunExe()],
          env:
            value === undefined
              ? {}
              : {
                  NO_COLOR: value,
                },
        });
        expect(stdout.toString()).toMatch(/\u001b\[\d+m/);
      });
    }
  });

  // #39762: a piped stream must not get ANSI codes because the other stream
  // is a TTY. `bun test | pbcopy` copied raw escape codes to the clipboard
  // since stderr (a TTY) forced colors onto the piped stdout.
  //
  // openpty via bun:ffi so one stdio fd can be a real TTY while the other is
  // a pipe. glibc keeps openpty in libutil; musl and macOS keep everything in
  // libc. Same pattern as test/js/bun/terminal/terminal-spawn.test.ts.
  describe.skipIf(isWindows)("per-stream color detection", () => {
    const colorEnv = {
      ...bunEnv,
      NO_COLOR: undefined,
      FORCE_COLOR: undefined,
      TERM: "xterm-256color",
    };

    const openptyDecl = {
      openpty: {
        args: [FFIType.ptr, FFIType.ptr, FFIType.ptr, FFIType.ptr, FFIType.ptr],
        returns: FFIType.i32,
      },
    } as const;
    const closeDecl = {
      close: { args: [FFIType.i32], returns: FFIType.i32 },
    } as const;

    function openPty(): { slave: number; close(): void } {
      const lib =
        process.platform === "darwin"
          ? dlopen("libc.dylib", { ...openptyDecl, ...closeDecl })
          : isMusl
            ? dlopen(process.arch === "arm64" ? "libc.musl-aarch64.so.1" : "libc.musl-x86_64.so.1", {
                ...openptyDecl,
                ...closeDecl,
              })
            : dlopen("libutil.so.1", openptyDecl);
      const libc = process.platform === "darwin" || isMusl ? lib : dlopen("libc.so.6", closeDecl);

      const masterBuf = new Int32Array(1);
      const slaveBuf = new Int32Array(1);
      expect((lib.symbols as any).openpty(masterBuf, slaveBuf, null, null, null)).toBe(0);
      return {
        slave: slaveBuf[0],
        close() {
          (libc.symbols as any).close(masterBuf[0]);
          (libc.symbols as any).close(slaveBuf[0]);
        },
      };
    }

    test.concurrent("piped stdout stays plain when stderr is a tty", async () => {
      const pty = openPty();
      try {
        await using proc = Bun.spawn({
          cmd: [bunExe(), "-e", "console.log({ a: 1 })"],
          env: colorEnv,
          stdin: "ignore",
          stdout: "pipe",
          stderr: pty.slave,
        });
        const [stdout, exitCode] = await Promise.all([proc.stdout.text(), proc.exited]);
        expect(stdout).not.toMatch(/\u001b\[/);
        expect(stdout).toContain("a: 1");
        expect(exitCode).toBe(0);
      } finally {
        pty.close();
      }
    });

    test.concurrent("piped stderr stays plain when stdout is a tty", async () => {
      const pty = openPty();
      try {
        await using proc = Bun.spawn({
          cmd: [bunExe(), "-e", "console.error({ a: 1 })"],
          env: colorEnv,
          stdin: "ignore",
          stdout: pty.slave,
          stderr: "pipe",
        });
        const [stderr, exitCode] = await Promise.all([proc.stderr.text(), proc.exited]);
        expect(stderr).not.toMatch(/\u001b\[/);
        expect(stderr).toContain("a: 1");
        expect(exitCode).toBe(0);
      } finally {
        pty.close();
      }
    });

    // Guard against overcorrection: a stream that is itself a TTY keeps
    // colors.
    test.concurrent("a tty stdout still gets colors", async () => {
      let output = "";
      const decoder = new TextDecoder();
      await using terminal = new Bun.Terminal({
        data(_t, chunk: Uint8Array) {
          output += decoder.decode(chunk, { stream: true });
        },
      });
      const proc = Bun.spawn({
        cmd: [bunExe(), "-e", "console.log({ a: 1 })"],
        env: colorEnv,
        terminal,
      });
      await proc.exited;
      // PTY data can still be in flight after waitpid. Poll with a deadline
      // (below the 5s test timeout) so a regression fails here with the
      // captured output, not by timeout.
      const deadline = Date.now() + 2_000;
      while (!/\u001b\[\d+m/.test(output) && Date.now() < deadline) {
        await Bun.sleep(10);
      }
      expect(output).toMatch(/\u001b\[\d+m/);
    });
  });

  describe("revision", () => {
    test("revision generates version numbers correctly", () => {
      var { stdout, exitCode } = Bun.spawnSync({
        cmd: [bunExe(), "--version"],
        env: bunEnv,
        stderr: "inherit",
      });
      var version = stdout.toString().trim();

      var { stdout, exitCode } = Bun.spawnSync({
        cmd: [bunExe(), "--revision"],
        env: bunEnv,
        stderr: "inherit",
      });
      var revision = stdout.toString().trim();

      expect(exitCode).toBe(0);
      expect(revision).toStartWith(version);
      // https://semver.org/#is-there-a-suggested-regular-expression-regex-to-check-a-semver-string
      expect(revision).toMatch(
        /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-((?:0|[1-9]\\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*)(?:\.(?:0|[1-9]\d*|\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?(?:\+([0-9a-zA-Z-]+(?:\.[0-9a-zA-Z-]+)*))?$/,
      );
    });
  });
  // On Windows `bun completions` installs bunx as a hardlink (or a .cmd shim) instead of a symlink.
  describe.skipIf(isWindows)("completions", () => {
    const bunxName = isDebug ? "bunx-debug" : "bunx";

    test("installs a bunx symlink to the executable, falling back through the install directories", async () => {
      using dir = tempDir("completions-bunx", {
        "bin": {},
        "empty-path": {},
        "install": { bin: {} },
        "home-empty": {},
        "home-bun": { ".bun": { bin: {} } },
        "home-local": { ".local": { bin: {} } },
      });
      // Run a private copy of the executable so that the first candidate location, the executable's
      // own directory, is inside the temporary directory. A hardlink avoids copying the binary; fall
      // back to a copy when the temporary directory is on another filesystem.
      const exe = join(String(dir), "bin", "bun");
      try {
        fs.linkSync(fs.realpathSync(bunExe()), exe);
      } catch {
        fs.copyFileSync(bunExe(), exe);
      }
      // The link is created against the resolved executable path.
      const exeRealpath = fs.realpathSync(exe);

      async function installBunx(env: Record<string, string | undefined>) {
        await using proc = Bun.spawn({
          cmd: [exe, "completions"],
          // No bunx on PATH, so the symlink gets installed. No SHELL, so the command stops right
          // after that step instead of writing shell completions.
          env: { ...bunEnv, PATH: join(String(dir), "empty-path"), SHELL: undefined, BUN_INSTALL: undefined, ...env },
          stdout: "pipe",
          stderr: "pipe",
        });
        const [stdout, stderr, exitCode] = await Promise.all([proc.stdout.text(), proc.stderr.text(), proc.exited]);
        expect(stdout).toBe("");
        expect(stderr).toContain("Unknown or unsupported shell");
        expect(exitCode).toBe(1);
      }

      // 1. Next to the executable.
      await installBunx({ HOME: join(String(dir), "home-empty") });
      expect(fs.readlinkSync(join(String(dir), "bin", bunxName))).toBe(exeRealpath);

      // That link now exists, so every following run falls through to the next location.
      // 2. $BUN_INSTALL/bin
      await installBunx({ HOME: join(String(dir), "home-empty"), BUN_INSTALL: join(String(dir), "install") });
      expect(fs.readlinkSync(join(String(dir), "install", "bin", bunxName))).toBe(exeRealpath);

      // 3. $HOME/.bun/bin
      await installBunx({ HOME: join(String(dir), "home-bun") });
      expect(fs.readlinkSync(join(String(dir), "home-bun", ".bun", "bin", bunxName))).toBe(exeRealpath);

      // 4. $HOME/.local/bin, once $HOME/.bun/bin does not exist.
      await installBunx({ HOME: join(String(dir), "home-local") });
      expect(fs.readlinkSync(join(String(dir), "home-local", ".local", "bin", bunxName))).toBe(exeRealpath);
    });

    test("reports that PowerShell completions do not exist when $SHELL is pwsh", async () => {
      // An empty home keeps the bunx symlink fallbacks ($HOME/.bun/bin, $HOME/.local/bin) out of the
      // real home directory. The first candidate, the executable's own directory, is unaffected.
      using home = tempDir("completions-pwsh-home", {});

      async function run(env: Record<string, string>) {
        await using proc = Bun.spawn({
          cmd: [bunExe(), "completions"],
          env: { ...bunEnv, HOME: String(home), BUN_INSTALL: undefined, ...env },
          stdout: "pipe",
          stderr: "pipe",
        });
        const [stdout, stderr, exitCode] = await Promise.all([proc.stdout.text(), proc.stderr.text(), proc.exited]);
        expect(stdout).toBe("");
        expect(stderr).toContain("PowerShell completions are not yet written for Bun.");
        expect(stderr).toContain("https://github.com/oven-sh/bun/issues/8939");
        return exitCode;
      }

      expect(await run({ SHELL: "/usr/local/bin/pwsh" })).toBe(1);
      expect(await run({ SHELL: "/usr/bin/powershell" })).toBe(1);

      // `bun upgrade` runs `bun completions` with IS_BUN_AUTO_UPDATE=true. That skips the "stdout is a
      // pipe" shortcut and makes a failure exit 0. Without the Pwsh arm this path went on to the
      // directory search and its `unreachable!()`.
      expect(await run({ SHELL: "/usr/local/bin/pwsh", IS_BUN_AUTO_UPDATE: "true" })).toBe(0);

      // When getcwd fails, the "stdout is a pipe" shortcut runs before the shell check. For a shell
      // without a script it must report the failure instead of writing nothing and exiting 0. The cwd
      // has to go away after the process starts, so a shell wrapper removes it and then execs bun.
      using cwdDir = tempDir("completions-pwsh-gone-cwd", {});
      const gone = String(cwdDir);
      await using proc = Bun.spawn({
        cmd: ["/bin/sh", "-c", `cd "${gone}" && rmdir "${gone}" && exec "${bunExe()}" completions`],
        env: { ...bunEnv, HOME: String(home), BUN_INSTALL: undefined, SHELL: "/usr/local/bin/pwsh" },
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr, exitCode] = await Promise.all([proc.stdout.text(), proc.stderr.text(), proc.exited]);
      expect(stdout).toBe("");
      expect(stderr).toContain("Could not get current working directory");
      expect(exitCode).toBe(1);
    });
  });
  describe("--help preserves <placeholder> text", () => {
    const env = { ...bunEnv, NO_COLOR: "1" };
    const flags: [string, string, string][] = [["run", "--elide-lines", "Number of lines of script output shown"]];
    test.concurrent.each(flags)("bun %s --help keeps placeholder in %s description", async (cmd, flag, expected) => {
      await using proc = Bun.spawn({ cmd: [bunExe(), cmd, "--help"], env, stderr: "pipe" });
      const [stdout, stderr, exitCode] = await Promise.all([proc.stdout.text(), proc.stderr.text(), proc.exited]);
      const line = (stdout + stderr).split(/\r?\n/).find(l => l.includes(flag)) ?? "";
      expect(line).toContain(expected);
      expect(exitCode).toBe(0);
    });
  });

  describe("--help lists the commands this build ships", () => {
    const env = { ...bunEnv, NO_COLOR: "1" };
    test("bun --help", async () => {
      await using proc = Bun.spawn({ cmd: [bunExe(), "--help"], env, stderr: "pipe" });
      const [stdout, stderr, exitCode] = await Promise.all([proc.stdout.text(), proc.stderr.text(), proc.exited]);
      const out = (stdout + stderr).replaceAll("\r\n", "\n");
      for (const re of [
        /^ {2}run\s+\.\/my-script\.ts\s+Execute a file with Bun$/m,
        /^ {12}lint\s+Run a package\.json script$/m,
      ]) {
        expect(out).toMatch(re);
      }
      // The package manager CLI (install/add/remove/pm/…), bunx, the project
      // scaffolding commands, and the test/repl/build/upgrade commands are
      // stripped from this build, so the help text must not advertise them.
      for (const removed of [
        /^ {2}install\s/m,
        /^ {2}add\s/m,
        /^ {2}remove\s/m,
        /^ {2}update\s/m,
        /^ {2}publish\s/m,
        /^ {2}pm\s/m,
        /^ {2}why\s/m,
        /^ {2}init\s/m,
        /^ {2}create\s/m,
        /^ {2}x\s/m,
        /^ {2}test\s/m,
        /^ {2}repl\s/m,
        /^ {2}build\s/m,
        /^ {2}upgrade\s/m,
        /^ {2}exec\s/m,
      ]) {
        expect(out).not.toMatch(removed);
      }
      expect(exitCode).toBe(0);
    });
  });

  describe("test command line arguments", () => {
    test("test --config, issue #4128", () => {
      const path = `${tmpdir()}/bunfig-${Date.now()}.toml`;
      fs.writeFileSync(path, "[debug]");

      const p = Bun.spawnSync({
        cmd: [bunExe(), "--config=" + path],
        env: {},
        stderr: "inherit",
      });
      try {
        expect(p.exitCode).toBe(0);
      } finally {
        fs.unlinkSync(path);
      }
    });
  });
});
