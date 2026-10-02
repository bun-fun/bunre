//! `parse()` runs `clap::parse()` against the per-tag table, handles
//! `--help`/`-v`/`--revision`, and populates the full `api::TransformOptions`
//! / `Context` from every recognised flag. All param tables — leaf and
//! concatenated — are const `&'static [ParamType]` via the
//! `bun_clap::parse_param!` proc-macro (compile-time spec parsing) plus a
//! const-fn slice concat (`bun_clap::concat_params!`).

use bun_options_types::LoaderExt as _;

use bstr::BStr;
use bun_bundler::options;
use bun_clap as clap;
use bun_clap::parse_param;
use bun_core::strings;
use bun_core::{self, Global, Output, env_var};
use bun_options_types::context::{Debugger, DebuggerEnable, HotReload, MacroOptions};
use bun_options_types::schema::api;
use bun_paths::platform;
use bun_paths::resolve_path;

use crate::cli;
use crate::cli::colon_list_type::ColonListType;
use crate::cli::command::{self, Context, Tag as CommandTag};
use crate::cli::concat_params;
use crate::cli::{DefineColonList, LoaderColonList};

/// Clone borrowed argv slices into the owning `Vec<Box<[u8]>>` shape used by
/// `api::TransformOptions` / `Context` fields.
#[inline]
fn slice_to_owned(input: &[&[u8]]) -> Vec<Box<[u8]>> {
    input.iter().map(|s| Box::<[u8]>::from(*s)).collect()
}

pub(crate) fn loader_resolver(input: &[u8]) -> crate::Result<api::Loader> {
    let option_loader = bun_ast::Loader::from_string(input).ok_or(crate::Error::InvalidLoader)?;
    Ok(option_loader.to_api())
}

fn resolve_jsx_runtime(s: &[u8]) -> api::JsxRuntime {
    if s == b"automatic" {
        api::JsxRuntime::Automatic
    } else if s == b"fallback" || s == b"classic" {
        api::JsxRuntime::Classic
    } else {
        bun_core::pretty_errorln!(
            "<r><red>error<r>: Invalid --jsx-runtime: \"{}\", expected \"automatic\" or \"classic\"",
            BStr::new(s)
        );
        Global::exit(1);
    }
}

pub(crate) type ParamType = clap::Param<clap::Help>;

// ─── param tables ────────────────────────────────────────────────────────────
// `bun_clap::parse_param!` expands to a const
// `Param<Help>` literal, and `concat_params!` is a const-fn slice concat, so
// every table — leaf and combined — lands in rodata with zero runtime init.
//
// All tables are `const` (const-eval cannot read `static`s) so they can feed
// both `concat_params!` and `comptime_table!`. The single rodata copy of each
// is the `static __CONV` / `static __TABLE` inside `comptime_table!` below.

// `SHOW_CRASH_TRACE` is a `const bool`, so the dead branch is eliminated.
macro_rules! maybe_debug_params {
    () => {
        if bun_core::env::SHOW_CRASH_TRACE {
            DEBUG_PARAMS
        } else {
            &[] as &[ParamType]
        }
    };
}

const BASE_PARAMS_: &[ParamType] = concat_params!(
    maybe_debug_params!(),
    &[
        parse_param!(
            "--env-file <STR>...               Load environment variables from the specified file(s)"
        ),
        parse_param!("--no-env-file                     Disable automatic loading of .env files"),
        parse_param!(
            "--cwd <STR>                       Absolute path to resolve files & entry points from. This just changes the process' cwd."
        ),
        parse_param!(
            "-c, --config <PATH>?              Specify path to Bun config file. Default <d>$cwd<r>/bunfig.toml"
        ),
        parse_param!("-h, --help                        Display this menu and exit"),
    ],
    &[parse_param!("<POS>...")],
);

const DEBUG_PARAMS: &[ParamType] = &[parse_param!(
    "--breakpoint-resolve <STR>...     DEBUG MODE: breakpoint when resolving something that includes this string"
)];

const TRANSPILER_PARAMS_: &[ParamType] = &[
    parse_param!(
        "--main-fields <STR>...             Main fields to lookup in package.json. Defaults to --target dependent"
    ),
    parse_param!("--preserve-symlinks               Preserve symlinks when resolving files"),
    parse_param!(
        "--preserve-symlinks-main          Preserve symlinks when resolving the main entry point"
    ),
    parse_param!("--extension-order <STR>...        Defaults to: .tsx,.ts,.jsx,.js,.json "),
    parse_param!(
        "--tsconfig-override <STR>          Specify custom tsconfig.json. Default <d>$cwd<r>/tsconfig.json"
    ),
    parse_param!(
        "-d, --define <STR>...              Substitute K:V while parsing, e.g. --define process.env.NODE_ENV:\"development\". Values are parsed as JSON."
    ),
    parse_param!(
        "--drop <STR>...                   Remove function calls, e.g. --drop=console removes all console.* calls."
    ),
    parse_param!(
        "--feature <STR>...               Enable a feature flag for dead-code elimination, e.g. --feature=SUPER_SECRET"
    ),
    parse_param!(
        "-l, --loader <STR>...             Parse files with .ext:loader, e.g. --loader .js:jsx. Valid loaders: js, jsx, ts, tsx, json, toml, text, file, wasm, napi"
    ),
    parse_param!(
        "--no-macros                       Disable macros from being executed in the bundler, transpiler and runtime"
    ),
    parse_param!(
        "--jsx-factory <STR>               Changes the function called when compiling JSX elements using the classic JSX runtime"
    ),
    parse_param!(
        "--jsx-fragment <STR>              Changes the function called when compiling JSX fragments"
    ),
    parse_param!(
        "--jsx-import-source <STR>         Declares the module specifier to be used for importing the jsx and jsxs factory functions. Default: \"react\""
    ),
    parse_param!("--jsx-runtime <STR>               \"automatic\" (default) or \"classic\""),
    parse_param!(
        "--jsx-side-effects                Treat JSX elements as having side effects (disable pure annotations)"
    ),
    parse_param!(
        "--ignore-dce-annotations          Ignore tree-shaking annotations such as @__PURE__"
    ),
];

const RUNTIME_PARAMS_: &[ParamType] = &[
    parse_param!(
        "--watch                           Automatically restart the process on file change"
    ),
    parse_param!(
        "--watch-kill-signal <STR>         Signal whose handlers run when --watch restarts the process (default: \"SIGTERM\")"
    ),
    parse_param!(
        "--hot                             Enable auto reload in the Bun runtime, test runner, or bundler"
    ),
    parse_param!(
        "--no-clear-screen                 Disable clearing the terminal screen on reload when --hot or --watch is enabled"
    ),
    parse_param!(
        "--smol                            Use less memory, but run garbage collection more often"
    ),
    parse_param!(
        "--interactive                     Start a Node.js-compatible REPL, like node --interactive"
    ),
    parse_param!(
        "-r, --preload <STR>...            Import a module before other modules are loaded"
    ),
    parse_param!("--require <STR>...                Alias of --preload, for Node.js compatibility"),
    parse_param!("--import <STR>...                 Alias of --preload, for Node.js compatibility"),
    parse_param!("--inspect <STR>?                  Activate Bun's debugger"),
    parse_param!(
        "--inspect-wait <STR>?             Activate Bun's debugger, wait for a connection before executing"
    ),
    parse_param!(
        "--inspect-brk <STR>?              Activate Bun's debugger, set breakpoint on first line of code and wait"
    ),
    parse_param!(
        "--cpu-prof                        Start CPU profiler and write profile to disk on exit"
    ),
    parse_param!("--cpu-prof-name <STR>             Specify the name of the CPU profile file"),
    parse_param!(
        "--cpu-prof-dir <STR>              Specify the directory where the CPU profile will be saved"
    ),
    parse_param!(
        "--cpu-prof-md                     Output CPU profile in markdown format (grep-friendly, designed for LLM analysis)"
    ),
    parse_param!(
        "--cpu-prof-interval <STR>         Specify the sampling interval in microseconds for CPU profiling (default: 1000)"
    ),
    parse_param!(
        "--heap-prof                       Write a heap profile to disk on exit (.heapprofile)"
    ),
    parse_param!("--heap-prof-name <STR>            Specify the name of the heap profile file"),
    parse_param!(
        "--heap-prof-dir <STR>             Specify the directory where the heap profile will be saved"
    ),
    parse_param!(
        "--heap-prof-md                    Generate markdown heap profile on exit (for CLI analysis)"
    ),
    parse_param!(
        "--heap-prof-interval <STR>        Specify the average sampling interval in bytes for heap profiling (default: 524288)"
    ),
    parse_param!(
        "--if-present                      Exit without an error if the entrypoint does not exist"
    ),
    parse_param!("--no-install                      Disable auto install in the Bun runtime"),
    parse_param!(
        "--install <STR>                   Configure auto-install behavior. One of \"auto\" (default, auto-installs when no node_modules), \"fallback\" (missing packages only), \"force\" (always)."
    ),
    parse_param!(
        "-i                                Auto-install dependencies during execution. Equivalent to --install=fallback."
    ),
    parse_param!("-e, --eval <STR>                  Evaluate argument as a script"),
    parse_param!(
        "-p, --print <STR>                 Evaluate argument as a script and print the result"
    ),
    parse_param!(
        "--prefer-offline                  Skip staleness checks for packages in the Bun runtime and resolve from disk"
    ),
    parse_param!(
        "--prefer-latest                   Use the latest matching versions of packages in the Bun runtime, always checking npm"
    ),
    parse_param!("--port <STR>                      Set the default port for Bun.serve"),
    parse_param!("-u, --origin <STR>"),
    parse_param!("--conditions <STR>...             Pass custom conditions to resolve"),
    parse_param!("--fetch-preconnect <STR>...       Preconnect to a URL while code is loading"),
    parse_param!(
        "--experimental-http2-fetch        Offer h2 in fetch() TLS ALPN. Same as BUN_FEATURE_FLAG_EXPERIMENTAL_HTTP2_CLIENT=1"
    ),
    parse_param!(
        "--experimental-http3-fetch        Honor Alt-Svc: h3 in fetch() and upgrade to HTTP/3. Same as BUN_FEATURE_FLAG_EXPERIMENTAL_HTTP3_CLIENT=1"
    ),
    parse_param!(
        "--max-http-header-size <INT>      Set the maximum size of HTTP headers in bytes. Default is 16KiB"
    ),
    parse_param!(
        "--insecure-http-parser            Use an insecure HTTP parser that accepts invalid HTTP headers"
    ),
    parse_param!(
        "--dns-result-order <STR>          Set the default order of DNS lookup results. Valid orders: verbatim (default), ipv4first, ipv6first"
    ),
    parse_param!(
        "--experimental-stream-iter        Enable the experimental stream/iter API (node:stream/iter, node:zlib/iter)."
    ),
    parse_param!(
        "--expose-gc                       Expose gc() on the global object. Has no effect on Bun.gc()."
    ),
    parse_param!(
        "--no-deprecation                  Suppress all reporting of the custom deprecation."
    ),
    parse_param!(
        "--throw-deprecation               Determine whether or not deprecation warnings result in errors."
    ),
    parse_param!("--no-warnings                     Silence all process warnings"),
    parse_param!("--trace-warnings                  Show stack traces on process warnings"),
    parse_param!("--trace-deprecation               Show stack traces on deprecations"),
    parse_param!("--pending-deprecation             Emit pending deprecation warnings"),
    parse_param!(
        "--redirect-warnings <STR>         Write process warnings to the given file instead of printing to stderr"
    ),
    parse_param!(
        "--disable-warning <STR>...        Silence specific process warnings by code or type"
    ),
    parse_param!("--title <STR>                     Set the process title"),
    parse_param!(
        "--zero-fill-buffers                Boolean to force Buffer.allocUnsafe(size) to be zero-filled."
    ),
    parse_param!(
        "--use-system-ca                   Use the system's trusted certificate authorities"
    ),
    parse_param!("--use-openssl-ca                  Use OpenSSL's default CA store"),
    parse_param!("--use-bundled-ca                  Use bundled CA store"),
    parse_param!("--tls-min-v1.0                    Set the default TLS minimum to TLSv1.0"),
    parse_param!("--tls-min-v1.1                    Set the default TLS minimum to TLSv1.1"),
    parse_param!("--tls-min-v1.2                    Set the default TLS minimum to TLSv1.2"),
    parse_param!("--tls-min-v1.3                    Set the default TLS minimum to TLSv1.3"),
    parse_param!("--tls-max-v1.2                    Set the default TLS maximum to TLSv1.2"),
    parse_param!("--tls-max-v1.3                    Set the default TLS maximum to TLSv1.3"),
    parse_param!("--redis-preconnect                Preconnect to $REDIS_URL at startup"),
    parse_param!("--sql-preconnect                  Preconnect to PostgreSQL at startup"),
    parse_param!(
        "--no-addons                       Throw an error if process.dlopen or bun:ffi cc() is called, and disable export condition \"node-addons\""
    ),
    parse_param!(
        "--no-ffi-cc                       Throw an error if bun:ffi cc() is called (disables the C compiler)"
    ),
    parse_param!(
        "--unhandled-rejections <STR>      One of \"strict\", \"throw\", \"warn\", \"none\", or \"warn-with-error-code\""
    ),
    parse_param!(
        "--console-depth <NUMBER>          Set the default depth for console.log object inspection (default: 2)"
    ),
    parse_param!(
        "--user-agent <STR>               Set the default User-Agent header for HTTP requests"
    ),
    parse_param!("--cron-title <STR>               Title for cron execution mode"),
    parse_param!("--cron-period <STR>              Cron period for cron execution mode"),
    // Node.js trace/diagnostics flags. Intentionally no help text: params with
    // an empty description are hidden from `--help` (see `simple_help`).
    // Declaring them (vs. relying on unknown-flag skipping) matters for the
    // value-taking ones — otherwise the value argument is parsed as the
    // entrypoint — and for `process.execArgv`'s re-parser, which derives its
    // value-consuming set from `AUTO_PARAMS`.
    parse_param!("--trace-events-enabled"),
    parse_param!("--trace-event-categories <STR>"),
    parse_param!("--trace-event-file-pattern <STR>"),
    parse_param!("--trace-env"),
    parse_param!("--trace-env-js-stack"),
    parse_param!("--trace-env-native-stack"),
    parse_param!("--trace-exit"),
    parse_param!("--expose-internals"),
    parse_param!("--stack-trace-limit <STR>"),
];

const AUTO_OR_RUN_PARAMS: &[ParamType] = &[
    parse_param!(
        "-F, --filter <STR>...             Run a script in all workspace packages matching the pattern"
    ),
    parse_param!(
        "-b, --bun                         Force a script or package to use Bun's runtime instead of Node.js (via symlinking node)"
    ),
    parse_param!(
        "--no-orphans                      Exit when the parent process dies, and on exit kill every descendant."
    ),
    parse_param!(
        "--shell <STR>                     Control the shell used for package.json scripts. Supports either 'bun' or 'system'"
    ),
    parse_param!(
        "--workspaces                      Run a script in all workspace packages (from the \"workspaces\" field in package.json)"
    ),
    parse_param!(
        "--parallel                        Run multiple scripts concurrently with Foreman-style output"
    ),
    parse_param!(
        "--sequential                      Run multiple scripts sequentially with Foreman-style output"
    ),
    parse_param!(
        "--no-exit-on-error                Continue running other scripts when one fails (with --parallel/--sequential)"
    ),
];

const AUTO_ONLY_PARAMS: &[ParamType] = concat_params!(
    &[
        // parse_param!("--all"),
        parse_param!("--silent                          Don't print the script command"),
        parse_param!(
            "--elide-lines <NUMBER>            Number of lines of script output shown when using --filter (default: 0, show all lines)"
        ),
        parse_param!("-v, --version                     Print version and exit"),
        parse_param!("--revision                        Print version with revision and exit"),
    ],
    AUTO_OR_RUN_PARAMS,
);
pub(crate) const AUTO_PARAMS: &[ParamType] = concat_params!(
    AUTO_ONLY_PARAMS,
    RUNTIME_PARAMS_,
    TRANSPILER_PARAMS_,
    BASE_PARAMS_
);

const RUN_ONLY_PARAMS: &[ParamType] = concat_params!(
    &[
        parse_param!("--silent                          Don't print the script command"),
        parse_param!(
            "--elide-lines <NUMBER>            Number of lines of script output shown when using --filter (default: 0, show all lines)"
        ),
    ],
    AUTO_OR_RUN_PARAMS,
);
pub(crate) const RUN_PARAMS: &[ParamType] = concat_params!(
    RUN_ONLY_PARAMS,
    RUNTIME_PARAMS_,
    TRANSPILER_PARAMS_,
    BASE_PARAMS_
);

/// Fallback table for `Command::tag_params`.
const BASE_RUNTIME_TRANSPILER_PARAMS: &[ParamType] =
    concat_params!(BASE_PARAMS_, RUNTIME_PARAMS_, TRANSPILER_PARAMS_);

// ─── pre-converted tables (rodata) ───────────────────────────────────────────
// `comptime_table!` converts `*_PARAMS` → `[Param<usize>; N]` + category counts
// + short-index entirely at const-eval, so `parse_with_table` does zero runtime
// conversion / allocation / sorting / locking. perf: `ConvertedTable::build` +
// quicksort + RawVec::grow was 8.7 % of `bun --version` userland samples.
//
// `.rodata.startup`: `comptime_table!` clusters the default-command table's
// nested `__CONV` / `__LONG` / `__TABLE` payloads there (see `src/clap/lib.rs`),
// otherwise each gets its own `.rodata.<sym>` input section that fat-LTO
// scatters across distinct pages in crate order. Pinning AUTO next to its
// `__TABLE` payload packs the trivial-script / `bun --version` arg-parse
// working set onto a couple of shared fault-around pages. Non-PIE `bun` has
// zero runtime relocations, so a `&'static` pointer literal stays in plain
// rodata even in `.rodata.startup`. Linux-only: ELF section syntax.
//
// Only `AUTO_TABLE` lives in `.rodata.startup`. `.rodata.startup` is
// deliberately one contiguous block faulted in with a single read-around on
// every cold start (including `bun --version` / `bun .` / `bun <file>`) —
// padding it with tables those paths never touch just grows that run. So
// `RUN_TABLE` (`bun run` / `bun x`), the subcommand tables (`build` / `test`)
// and the `BASE_RUNTIME_TRANSPILER` catch-all (`install` / `pm` / …) are all
// built with `comptime_table!(.., cold)` and stay in plain `.rodata`, where
// `src/startup.order` can still cluster the ones a sampled cold path actually
// hits without weighing down the `.rodata.startup` fault-around window.
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    unsafe(link_section = ".rodata.startup")
)]
pub(crate) static AUTO_TABLE: &clap::ConvertedTable = clap::comptime_table!(AUTO_PARAMS);
pub(crate) static RUN_TABLE: &clap::ConvertedTable = clap::comptime_table!(RUN_PARAMS, cold);
static BASE_RUNTIME_TRANSPILER_TABLE: &clap::ConvertedTable =
    clap::comptime_table!(BASE_RUNTIME_TRANSPILER_PARAMS, cold);

/// Per-tag pre-converted clap table (rodata, built at compile time via
/// `comptime_table!`). This is what `parse` consumes so the startup path never
/// hits `ConvertedTable::build`'s alloc/sort/lock.
#[inline]
fn tag_table(cmd: CommandTag) -> &'static clap::ConvertedTable {
    match cmd {
        CommandTag::AutoCommand => AUTO_TABLE,
        CommandTag::RunCommand | CommandTag::RunAsNodeCommand => RUN_TABLE,
        _ => BASE_RUNTIME_TRANSPILER_TABLE,
    }
}

// ─── exported FFI globals (written by parse(), read from C++) ────────────────
// `AtomicBool` has the same size/alignment/bit-validity as `bool`, so the
// `#[no_mangle]` symbol layout is unchanged for the C++ side that reads these
// as plain `bool`. Rust writes go through `.store(.., Relaxed)`.
#[unsafe(no_mangle)]
static Bun__Node__ZeroFillBuffers: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
static Bun__Node__ProcessNoDeprecation: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
static Bun__Node__ProcessThrowDeprecation: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
pub(crate) static Bun__Node__ProcessNoWarnings: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
pub(crate) static Bun__Node__ProcessTraceWarnings: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
pub(crate) static Bun__Node__ProcessTraceDeprecation: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
#[unsafe(no_mangle)]
pub(crate) static Bun__Node__ProcessPendingDeprecation: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Node parity: `--cpu-prof-name` supports a `${pid}` placeholder.
fn replace_pid_placeholder(name: &[u8]) -> Box<[u8]> {
    if !bun_core::strings::contains(name, b"${pid}") {
        return name.into();
    }
    let pid = std::process::id().to_string();
    let mut out = Vec::with_capacity(name.len() + pid.len());
    let mut i = 0;
    while i < name.len() {
        if name[i..].starts_with(b"${pid}") {
            out.extend_from_slice(pid.as_bytes());
            i += 6;
        } else {
            out.push(name[i]);
            i += 1;
        }
    }
    out.into_boxed_slice()
}

#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq)]
pub(crate) enum BunCAStore {
    Bundled,
    Openssl,
    System,
}
#[unsafe(no_mangle)]
static Bun__Node__CAStore: core::sync::atomic::AtomicU8 =
    core::sync::atomic::AtomicU8::new(BunCAStore::Bundled as u8);
#[unsafe(no_mangle)]
pub(crate) static Bun__Node__UseSystemCA: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

// ─── bunfig loading ──────────────────────────────────────────────────────────
// their private helpers moved to `bun_bunfig::arguments` so `bun_install` can
// call them without a tier-6 dependency. Re-export here so existing
// `crate::cli::arguments::load_config*` callers are unaffected.
pub use bun_bunfig::arguments::{load_config_path, load_config_with_cmd_args};

/// node aliases `-pe` to `--print --eval` as a whole token (node_options.cc):
/// it can't be a short in either runtime, being ambiguous with `-p` carrying
/// the attached value `e`. Bun's `-p` takes the code, so `-pe X` is `-p X`.
pub const NODE_SHORT_ALIASES: &[(&[u8], &[u8])] = &[(b"-pe", b"-p")];

/// Parse `argv` into `api::TransformOptions` for the given subcommand.
///
/// `command::tag_params(cmd)` does a runtime lookup of the per-subcommand
/// param table, and the per-`cmd` blocks below are guarded by
/// `if matches!(cmd, …)`.
pub(crate) fn parse(cmd: CommandTag, ctx: Context<'_>) -> crate::Result<api::TransformOptions> {
    let mut diag = clap::Diagnostic::default();
    let table = tag_table(cmd);

    let args = match clap::parse_with_table::<clap::Help>(
        table,
        clap::ParseOptions {
            diagnostic: Some(&mut diag),
            stop_after_positional_at: match cmd {
                CommandTag::RunCommand => 2,
                CommandTag::AutoCommand | CommandTag::RunAsNodeCommand => 1,
                _ => 0,
            },
            // Only the paths standing in for `node` get node's aliases.
            short_aliases: match cmd {
                CommandTag::AutoCommand | CommandTag::RunAsNodeCommand => NODE_SHORT_ALIASES,
                _ => &[],
            },
        },
    ) {
        Ok(a) => a,
        Err(err) => {
            // Report useful error and exit
            let _ = diag.report(Output::error_writer(), err);
            command::tag_print_help(cmd, false);
            Global::exit(1);
        }
    };

    if args.flag(b"--help") {
        command::tag_print_help(cmd, true);
        Output::flush();
        Global::exit(0);
    }

    if cmd == CommandTag::AutoCommand {
        if args.flag(b"--version") {
            cli::print_version_and_exit();
        }
        if args.flag(b"--revision") {
            cli::print_revision_and_exit();
        }
    }

    // ── --cwd ────────────────────────────────────────────────────────────────
    // `api::TransformOptions.absolute_working_dir` is `Option<Box<[u8]>>`,
    // so we dupe into a plain `Box<[u8]>`.
    let cwd: Box<[u8]> = if let Some(cwd_arg) = args.option(b"--cwd") {
        let mut outbuf = bun_paths::path_buffer_pool::get();
        // An absolute --cwd needs no base; a relative one still requires a
        // live cwd (an exe-dir base would silently chdir somewhere else).
        let base: &[u8] = if bun_paths::is_absolute(cwd_arg) {
            b"/"
        } else {
            bun_core::getcwd(&mut outbuf)?.as_bytes()
        };
        let mut spill = Vec::new();
        let out =
            resolve_path::join_abs_string_spill::<platform::Loose>(base, &mut spill, &[cwd_arg]);
        // `chdir` wants a NUL-terminated path, so dupe-Z once and reuse for both
        // the `chdir` arg and the stored `absolute_working_dir`.
        let out_z = bun_core::ZBox::from_bytes(out);
        if let bun_sys::Result::Err(err) = bun_sys::chdir(&out_z) {
            Output::err(
                err,
                "Could not change directory to \"{}\"\n",
                format_args!("{}", BStr::new(cwd_arg)),
            );
            Global::exit(1);
        }
        // Store the post-chdir physical path (mirrors process.chdir) so
        // process.cwd(), path.resolve, and the resolver agree on one form.
        let mut phys = bun_paths::path_buffer_pool::get();
        match bun_core::getcwd(&mut phys) {
            Ok(p) => Box::<[u8]>::from(p.as_bytes()),
            Err(_) => Box::<[u8]>::from(out_z.as_bytes()),
        }
    } else if matches!(
        cmd,
        CommandTag::AutoCommand | CommandTag::RunCommand | CommandTag::RunAsNodeCommand
    ) {
        // A deleted cwd must not abort the runtime (Node boots and lets
        // `process.cwd()` throw later); fall back to the executable's dir.
        let mut temp = bun_paths::path_buffer_pool::get();
        Box::<[u8]>::from(bun_core::getcwd_or_exe_dir(&mut temp).as_bytes())
    } else {
        // Everything else (install/test/build/...) must not silently act on
        // whatever project happens to live above the executable.
        let mut temp = bun_paths::path_buffer_pool::get();
        Box::<[u8]>::from(bun_core::getcwd(&mut temp)?.as_bytes())
    };

    // Not gated on .BunxCommand: bunx skips Arguments.parse entirely
    // (uses_global_options=false). bunx picks up no-orphans via the
    // BUN_FEATURE_FLAG_NO_ORPHANS env var in main()→install() instead.
    if matches!(cmd, CommandTag::RunCommand | CommandTag::AutoCommand) {
        if args.flag(b"--no-orphans") {
            bun_io::parent_death_watchdog::enable();
        }
    }

    if matches!(cmd, CommandTag::RunCommand | CommandTag::AutoCommand) {
        ctx.filters = slice_to_owned(args.options(b"--filter"));
        ctx.workspaces = args.flag(b"--workspaces");
        ctx.if_present = args.flag(b"--if-present");
        ctx.parallel = args.flag(b"--parallel");
        ctx.sequential = args.flag(b"--sequential");
        ctx.no_exit_on_error = args.flag(b"--no-exit-on-error");

        if let Some(elide_lines) = args.option(b"--elide-lines") {
            if !elide_lines.is_empty() {
                ctx.bundler_options.elide_lines = match strings::parse_int::<usize>(elide_lines, 10)
                {
                    Ok(v) => Some(v),
                    Err(_) => {
                        bun_core::pretty_errorln!(
                            "<r><red>error<r>: Invalid elide-lines: \"{}\"",
                            BStr::new(elide_lines)
                        );
                        Global::exit(1);
                    }
                };
            }
        }
    }
    ctx.args.absolute_working_dir = Some(cwd);
    ctx.positionals = slice_to_owned(args.positionals());

    if command::LOADS_CONFIG[cmd] {
        load_config_with_cmd_args(cmd, &args, ctx)?;
    }

    let mut opts: api::TransformOptions = ctx.args.clone();

    let defines_tuple = DefineColonList::resolve(args.options(b"--define"))?;

    if !defines_tuple.keys.is_empty() {
        opts.define = Some(api::StringMap {
            keys: defines_tuple
                .keys
                .iter()
                .map(|s| Box::<[u8]>::from(*s))
                .collect(),
            values: defines_tuple
                .values
                .iter()
                .map(|s| Box::<[u8]>::from(*s))
                .collect(),
        });
    }

    opts.drop = slice_to_owned(args.options(b"--drop"));
    opts.feature_flags = slice_to_owned(args.options(b"--feature"));

    // Node added a `--loader` flag (that's kinda like `--register`). It's
    // completely different from ours.
    let loader_tuple = if cmd != CommandTag::RunAsNodeCommand {
        LoaderColonList::resolve(args.options(b"--loader"))?
    } else {
        ColonListType {
            keys: Vec::new(),
            values: Vec::new(),
        }
    };

    if !loader_tuple.keys.is_empty() {
        opts.loaders = Some(api::LoaderMap {
            extensions: loader_tuple
                .keys
                .iter()
                .map(|s| Box::<[u8]>::from(*s))
                .collect(),
            loaders: loader_tuple.values,
        });
    }

    opts.tsconfig_override = args.option(b"--tsconfig-override").map(|ts| {
        let mut spill = Vec::new();
        Box::from(resolve_path::join_abs_string_spill::<platform::Auto>(
            ctx.args.absolute_working_dir.as_deref().unwrap(),
            &mut spill,
            &[ts],
        ))
    });

    opts.main_fields = slice_to_owned(args.options(b"--main-fields"));
    // we never actually supported inject.
    // opts.inject = args.options(b"--inject");
    opts.env_files = slice_to_owned(args.options(b"--env-file"));
    opts.extension_order = slice_to_owned(args.options(b"--extension-order"));

    if args.flag(b"--no-env-file") {
        opts.disable_default_env_files = true;
    }

    if args.flag(b"--preserve-symlinks") {
        opts.preserve_symlinks = Some(true);
    }
    if args.flag(b"--preserve-symlinks-main") {
        ctx.runtime_options.preserve_symlinks_main = true;
    }

    ctx.passthrough = slice_to_owned(args.remaining());

    if matches!(cmd, CommandTag::AutoCommand | CommandTag::RunCommand) {
        if !args.options(b"--conditions").is_empty() {
            opts.conditions = slice_to_owned(args.options(b"--conditions"));
        }
    }

    // runtime commands
    if matches!(
        cmd,
        CommandTag::AutoCommand | CommandTag::RunCommand | CommandTag::RunAsNodeCommand
    ) {
        {
            let preloads = args.options(b"--preload");
            let preloads2 = args.options(b"--require");
            let preloads3 = args.options(b"--import");
            let preload4 = env_var::BUN_INSPECT_PRELOAD.get();

            let total_preloads = ctx.preloads.len()
                + preloads.len()
                + preloads2.len()
                + preloads3.len()
                + (if preload4.is_some() { 1usize } else { 0usize });
            if total_preloads > 0 {
                let mut all: Vec<Box<[u8]>> = Vec::with_capacity(total_preloads);
                if !ctx.preloads.is_empty() {
                    all.append(&mut ctx.preloads);
                }
                for p in preloads {
                    all.push(Box::<[u8]>::from(*p));
                }
                for p in preloads2 {
                    all.push(Box::<[u8]>::from(*p));
                }
                for p in preloads3 {
                    all.push(Box::<[u8]>::from(*p));
                }
                if let Some(p) = preload4 {
                    all.push(Box::<[u8]>::from(p));
                }
                ctx.preloads = all;
            }
        }

        if args.flag(b"--hot") {
            ctx.debug.hot_reload = HotReload::Hot;
            if args.flag(b"--no-clear-screen") {
                let _ = bun_dotenv::HAS_NO_CLEAR_SCREEN_CLI_FLAG.set(true);
            }
        } else if args.flag(b"--watch") {
            ctx.debug.hot_reload = HotReload::Watch;

            // Windows applies this to the watcher child process.
            // The parent process is unable to re-launch itself
            #[cfg(not(windows))]
            {
                bun_core::set_auto_reload_on_crash(true);
            }

            if args.flag(b"--no-clear-screen") {
                let _ = bun_dotenv::HAS_NO_CLEAR_SCREEN_CLI_FLAG.set(true);
            }
        }

        if let Some(kill_signal) = args.option(b"--watch-kill-signal") {
            // Node reads --watch-kill-signal only in watch mode; elsewhere it is
            // accepted and ignored. Matching is case-insensitive (node uppercases).
            if ctx.debug.hot_reload == HotReload::Watch {
                let upper = kill_signal.to_ascii_uppercase();
                match bun_core::SignalCode::from_name(&upper)
                    .filter(|s| s.platform_number().is_some())
                {
                    Some(sig) => ctx.debug.watch_kill_signal = sig,
                    None => {
                        Output::print_errorln(format_args!(
                            "TypeError [ERR_UNKNOWN_SIGNAL]: Unknown signal: {}",
                            bstr::BStr::new(kill_signal)
                        ));
                        Output::flush();
                        Global::exit(1);
                    }
                }
            }
        }

        if let Some(origin) = args.option(b"--origin") {
            opts.origin = Some(origin.into());
        }

        if args.flag(b"--sql-preconnect") {
            ctx.runtime_options.sql_preconnect = true;
        }

        if args.flag(b"--no-addons") {
            // used for disabling process.dlopen and bun:ffi cc(), and
            // for disabling export condition "node-addons"
            opts.allow_addons = Some(false);
        }

        if args.flag(b"--no-ffi-cc") {
            opts.allow_ffi_cc = Some(false);
        }

        if let Some(unhandled_rejections) = args.option(b"--unhandled-rejections") {
            opts.unhandled_rejections = match api::UnhandledRejections::MAP
                .get(unhandled_rejections)
            {
                Some(v) => Some(*v),
                None => {
                    Output::err_generic(
                        "Invalid value for --unhandled-rejections: \"{}\". Must be one of \"strict\", \"throw\", \"warn\", \"none\", \"warn-with-error-code\"\n",
                        format_args!("{}", BStr::new(unhandled_rejections)),
                    );
                    Global::exit(1);
                }
            };
        }

        if let Some(port_str) = args.option(b"--port") {
            if cmd == CommandTag::RunAsNodeCommand {
                // TODO: prevent `node --port <script>` from working
                ctx.runtime_options.eval.script = port_str.into();
                ctx.runtime_options.eval.eval_and_print = true;
            } else {
                opts.port = match strings::parse_int::<u16>(port_str, 10) {
                    Ok(v) => Some(v),
                    Err(_) => {
                        Output::err_fmt(bun_core::fmt::out_of_range(
                            port_str,
                            bun_core::fmt::OutOfRangeOptions {
                                field_name: b"--port",
                                min: 0,
                                max: u16::MAX as i64,
                                msg: b"",
                            },
                        ));
                        bun_core::note!("To evaluate TypeScript here, use 'bun --print'");
                        Global::exit(1);
                    }
                };
            }
        }

        if let Some(size_str) = args.option(b"--max-http-header-size") {
            let size = match strings::parse_int::<usize>(size_str, 10) {
                Ok(v) => v,
                Err(_) => {
                    Output::err_generic(
                        "Invalid value for --max-http-header-size: \"{}\". Must be a positive integer\n",
                        format_args!("{}", BStr::new(size_str)),
                    );
                    Global::exit(1);
                }
            };
            bun_http::set_max_http_header_size(if size == 0 { 1024 * 1024 * 1024 } else { size });
        }

        if args.flag(b"--insecure-http-parser") {
            bun_http::set_insecure_http_parser(true);
        }

        if let Some(user_agent) = args.option(b"--user-agent") {
            // argv slices returned by `clap::Args::option` borrow
            // process-lifetime `argv` storage.
            let _ = bun_http::OVERRIDDEN_DEFAULT_USER_AGENT.set(user_agent);
        }

        ctx.debug.offline_mode_setting = Some(if args.flag(b"--prefer-offline") {
            bun_options_types::offline_mode::OfflineMode::Offline
        } else if args.flag(b"--prefer-latest") {
            bun_options_types::offline_mode::OfflineMode::Latest
        } else {
            bun_options_types::offline_mode::OfflineMode::Online
        });

        if args.flag(b"--no-install") {
            ctx.debug.global_cache = options::GlobalCache::disable;
        } else if args.flag(b"-i") && cmd != CommandTag::RunAsNodeCommand {
            // Under node emulation `-i` is node's --interactive alias, not
            // --install=fallback (auto-install is meaningless there).
            ctx.debug.global_cache = options::GlobalCache::fallback;
        } else if let Some(enum_value) = args.option(b"--install") {
            // -i=auto --install=force, --install=disable
            if let Some(result) = options::GlobalCache::MAP.get(enum_value) {
                ctx.debug.global_cache = *result;
            // -i, --install
            } else if enum_value.is_empty() {
                ctx.debug.global_cache = options::GlobalCache::force;
            } else {
                Output::err_generic(
                    "Invalid value for --install: \"{}\". Must be either \"auto\", \"fallback\", \"force\", or \"disable\"\n",
                    format_args!("{}", BStr::new(enum_value)),
                );
                Global::exit(1);
            }
        }

        if let Some(script) = args.option(b"--print") {
            ctx.runtime_options.eval.script = script.into();
            ctx.runtime_options.eval.eval_and_print = true;
        } else if let Some(script) = args.option(b"--eval") {
            ctx.runtime_options.eval.script = script.into();
        }
        ctx.runtime_options.if_present = args.flag(b"--if-present");
        ctx.runtime_options.smol = args.flag(b"--smol");
        // node's `-i` is an alias for --interactive; elsewhere `-i` is --install=fallback.
        ctx.runtime_options.interactive = args.flag(b"--interactive")
            || (cmd == CommandTag::RunAsNodeCommand && args.flag(b"-i"));
        ctx.runtime_options.preconnect = slice_to_owned(args.options(b"--fetch-preconnect"));
        ctx.runtime_options.experimental_http2_fetch = args.flag(b"--experimental-http2-fetch");
        ctx.runtime_options.experimental_http3_fetch = args.flag(b"--experimental-http3-fetch");
        ctx.runtime_options.expose_gc = args.flag(b"--expose-gc");
        if args.flag(b"--expose-internals") {
            // Same gate the env var `BUN_FEATURE_FLAG_INTERNAL_FOR_TESTING`
            // sets (VirtualMachine::configure_from_env): allows resolving
            // `bun:internal-for-testing` / `internal/test/binding` in release
            // builds. Debug builds always allow them.
            bun_jsc::module_loader::IS_ALLOWED_TO_USE_INTERNAL_TESTING_APIS
                .store(true, core::sync::atomic::Ordering::Relaxed);
            bun_resolve_builtins::set_expose_internals_enabled(true);
        }

        if let Some(depth_str) = args.option(b"--console-depth") {
            let depth = match strings::parse_int::<u16>(depth_str, 10) {
                Ok(v) => v,
                Err(_) => {
                    Output::err_generic(
                        "Invalid value for --console-depth: \"{}\". Must be a positive integer\n",
                        format_args!("{}", BStr::new(depth_str)),
                    );
                    Global::exit(1);
                }
            };
            // Treat depth=0 as maxInt(u16) for infinite depth
            ctx.runtime_options.console_depth = Some(if depth == 0 { u16::MAX } else { depth });
        }

        if let Some(order) = args.option(b"--dns-result-order") {
            ctx.runtime_options.dns_result_order = order.into();
        }

        let has_cron_title = args.option(b"--cron-title");
        let has_cron_period = args.option(b"--cron-period");
        if let Some(t) = has_cron_title {
            ctx.runtime_options.cron_title = t.into();
        }
        if let Some(p) = has_cron_period {
            ctx.runtime_options.cron_period = p.into();
        }
        if has_cron_title.is_some() != has_cron_period.is_some() {
            Output::err_generic(
                "--cron-title and --cron-period must be provided together",
                (),
            );
            Global::exit(1);
        }
        if has_cron_title.is_some()
            && (ctx.runtime_options.cron_title.is_empty()
                || ctx.runtime_options.cron_period.is_empty())
        {
            Output::err_generic("--cron-title and --cron-period must not be empty", ());
            Global::exit(1);
        }

        if let Some(inspect_flag) = args.option(b"--inspect") {
            ctx.runtime_options.debugger = if inspect_flag.is_empty() {
                Debugger::Enable(Default::default())
            } else {
                Debugger::Enable(DebuggerEnable {
                    path_or_port: Box::<[u8]>::from(inspect_flag),
                    ..Default::default()
                })
            };
        } else if let Some(inspect_flag) = args.option(b"--inspect-wait") {
            ctx.runtime_options.debugger = if inspect_flag.is_empty() {
                Debugger::Enable(DebuggerEnable {
                    wait_for_connection: true,
                    ..Default::default()
                })
            } else {
                Debugger::Enable(DebuggerEnable {
                    path_or_port: Box::<[u8]>::from(inspect_flag),
                    wait_for_connection: true,
                    ..Default::default()
                })
            };
        } else if let Some(inspect_flag) = args.option(b"--inspect-brk") {
            ctx.runtime_options.debugger = if inspect_flag.is_empty() {
                Debugger::Enable(DebuggerEnable {
                    wait_for_connection: true,
                    set_breakpoint_on_first_line: true,
                    ..Default::default()
                })
            } else {
                Debugger::Enable(DebuggerEnable {
                    path_or_port: Box::<[u8]>::from(inspect_flag),
                    wait_for_connection: true,
                    set_breakpoint_on_first_line: true,
                    ..Default::default()
                })
            };
        }

        let cpu_prof_flag = args.flag(b"--cpu-prof");
        let cpu_prof_md_flag = args.flag(b"--cpu-prof-md");

        // --cpu-prof-md alone enables profiling with markdown format
        // --cpu-prof alone enables profiling with JSON format
        // Both flags together enable profiling with both formats
        if cpu_prof_flag || cpu_prof_md_flag {
            ctx.runtime_options.cpu_prof.enabled = true;
            if let Some(name) = args.option(b"--cpu-prof-name") {
                ctx.runtime_options.cpu_prof.name = replace_pid_placeholder(name);
            }
            if let Some(dir) = args.option(b"--cpu-prof-dir") {
                ctx.runtime_options.cpu_prof.dir = dir.into();
            }
            // md_format is true if --cpu-prof-md is passed (regardless of --cpu-prof)
            ctx.runtime_options.cpu_prof.md_format = cpu_prof_md_flag;
            // json_format is true if --cpu-prof is passed (regardless of --cpu-prof-md)
            ctx.runtime_options.cpu_prof.json_format = cpu_prof_flag;
            if let Some(interval_str) = args.option(b"--cpu-prof-interval") {
                ctx.runtime_options.cpu_prof.interval =
                    strings::parse_int::<u32>(interval_str, 10).unwrap_or(1000);
            }
        } else {
            // Node parity: profiler-scoped options without a profiler flag are a
            // usage error printed as "<argv0>: <flag> must be used with --cpu-prof"
            // and exit code 9. The default interval value is a noop, like node.
            let mut bad_flags: [Option<&str>; 3] = [None, None, None];
            if args.option(b"--cpu-prof-name").is_some() {
                bad_flags[0] = Some("--cpu-prof-name");
            }
            if args.option(b"--cpu-prof-dir").is_some() {
                bad_flags[1] = Some("--cpu-prof-dir");
            }
            if let Some(interval_str) = args.option(b"--cpu-prof-interval") {
                if strings::parse_int::<u32>(interval_str, 10).unwrap_or(0) != 1000 {
                    bad_flags[2] = Some("--cpu-prof-interval");
                }
            }
            if bad_flags.iter().any(Option::is_some) {
                let argv0 = bun_core::argv().get(0).unwrap_or(bun_core::zstr!("bun"));
                for flag in bad_flags.into_iter().flatten() {
                    bun_core::pretty_errorln!(
                        "{}: {} must be used with --cpu-prof",
                        BStr::new(argv0.as_bytes()),
                        flag
                    );
                }
                Output::flush();
                Global::exit(9);
            }
        }

        let heap_prof_v8 = args.flag(b"--heap-prof");
        let heap_prof_md = args.flag(b"--heap-prof-md");

        if heap_prof_v8 && heap_prof_md {
            // Both flags specified - warn and use markdown format
            bun_core::warn!(
                "Both --heap-prof and --heap-prof-md specified; using --heap-prof-md (markdown format)",
            );
            ctx.runtime_options.heap_prof.enabled = true;
            ctx.runtime_options.heap_prof.text_format = true;
            if let Some(name) = args.option(b"--heap-prof-name") {
                ctx.runtime_options.heap_prof.name = name.into();
            }
            if let Some(dir) = args.option(b"--heap-prof-dir") {
                ctx.runtime_options.heap_prof.dir = dir.into();
            }
        } else if heap_prof_v8 || heap_prof_md {
            // --heap-prof-interval is accepted for node CLI parity but unused:
            // JSC has no allocation-site sampler to configure.
            ctx.runtime_options.heap_prof.enabled = true;
            ctx.runtime_options.heap_prof.text_format = heap_prof_md;
            if let Some(name) = args.option(b"--heap-prof-name") {
                ctx.runtime_options.heap_prof.name = name.into();
            }
            if let Some(dir) = args.option(b"--heap-prof-dir") {
                ctx.runtime_options.heap_prof.dir = dir.into();
            }
        } else {
            // Node parity: heap-profiler-scoped options without a profiler flag
            // exit 9, like the --cpu-prof block above. The default interval
            // value (512 KiB) is a noop, like node.
            let mut bad_flags: [Option<&str>; 3] = [None, None, None];
            if args.option(b"--heap-prof-name").is_some() {
                bad_flags[0] = Some("--heap-prof-name");
            }
            if args.option(b"--heap-prof-dir").is_some() {
                bad_flags[1] = Some("--heap-prof-dir");
            }
            if let Some(interval_str) = args.option(b"--heap-prof-interval") {
                if strings::parse_int::<u32>(interval_str, 10).unwrap_or(0) != 512 * 1024 {
                    bad_flags[2] = Some("--heap-prof-interval");
                }
            }
            if bad_flags.iter().any(Option::is_some) {
                let argv0 = bun_core::argv().get(0).unwrap_or(bun_core::zstr!("bun"));
                for flag in bad_flags.into_iter().flatten() {
                    bun_core::pretty_errorln!(
                        "{}: {} must be used with --heap-prof",
                        BStr::new(argv0.as_bytes()),
                        flag
                    );
                }
                Output::flush();
                Global::exit(9);
            }
        }

        if args.flag(b"--experimental-stream-iter") {
            bun_resolve_builtins::set_stream_iter_enabled(true);
        }
        if args.flag(b"--no-deprecation") {
            Bun__Node__ProcessNoDeprecation.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if args.flag(b"--throw-deprecation") {
            Bun__Node__ProcessThrowDeprecation.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if args.flag(b"--no-warnings") {
            Bun__Node__ProcessNoWarnings.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if args.flag(b"--trace-warnings") {
            Bun__Node__ProcessTraceWarnings.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if args.flag(b"--trace-deprecation") {
            Bun__Node__ProcessTraceDeprecation.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if args.flag(b"--pending-deprecation")
            || env_var::NODE_PENDING_DEPRECATION.get() == Some(b"1" as &[u8])
        {
            Bun__Node__ProcessPendingDeprecation.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        if let Some(path) = args.option(b"--redirect-warnings") {
            let _ = cli::Bun__Node__RedirectWarnings.set(path.into());
        }
        {
            let disabled = args.options(b"--disable-warning");
            if !disabled.is_empty() {
                let _ = cli::Bun__Node__DisabledWarnings
                    .set(disabled.iter().map(|e| Box::from(*e)).collect());
            }
        }
        if let Some(title) = args.option(b"--title") {
            // Static is `Mutex<Option<Box<[u8]>>>` so `process.title = "..."`
            // can drop the previous value; box the argv-borrowed slice up
            // front.
            *cli::Bun__Node__ProcessTitle.lock() = Some(title.into());
        }
        if args.flag(b"--zero-fill-buffers") {
            Bun__Node__ZeroFillBuffers.store(true, core::sync::atomic::Ordering::Relaxed);
        }
        let use_system_ca = args.flag(b"--use-system-ca");
        let use_openssl_ca = args.flag(b"--use-openssl-ca");
        let use_bundled_ca = args.flag(b"--use-bundled-ca");

        // Disallow any combination > 1
        if (use_system_ca as u8) + (use_openssl_ca as u8) + (use_bundled_ca as u8) > 1 {
            bun_core::pretty_errorln!(
                "<r><red>error<r>: choose exactly one of --use-system-ca, --use-openssl-ca, or --use-bundled-ca"
            );
            Global::exit(1);
        }

        // CLI overrides env var (NODE_USE_SYSTEM_CA)
        let store: Option<BunCAStore> = if use_bundled_ca {
            Some(BunCAStore::Bundled)
        } else if use_openssl_ca {
            Some(BunCAStore::Openssl)
        } else if use_system_ca || env_var::NODE_USE_SYSTEM_CA.get().unwrap_or(false) {
            Some(BunCAStore::System)
        } else {
            // No CA flag — leave the FFI default (Bundled) in place. Avoids a
            // `transmute<u8, BunCAStore>` round-trip through the atomic, which
            // would be UB on an out-of-range discriminant.
            None
        };
        if let Some(store) = store {
            Bun__Node__CAStore.store(store as u8, core::sync::atomic::Ordering::Relaxed);
            // Back-compat boolean used by native code until fully migrated
            Bun__Node__UseSystemCA.store(
                store == BunCAStore::System,
                core::sync::atomic::Ordering::Relaxed,
            );
        } else {
            // `Bun__Node__UseSystemCA` is written unconditionally,
            // even when no CA flag/env was supplied (default bundled ⇒ false).
            Bun__Node__UseSystemCA.store(false, core::sync::atomic::Ordering::Relaxed);
        }

        if args.flag(b"--tls-min-v1.3") && args.flag(b"--tls-max-v1.2") {
            bun_core::pretty_errorln!(
                "<r><red>error<r>: --tls-min-v1.3 sets default TLS minimum to TLSv1.3 and is not compatible with --tls-max-v1.2, which sets default TLS maximum to TLSv1.2; use one or the other, not both"
            );
            Global::exit(1);
        }
    }

    if let (Some(port), true) = (opts.port, opts.origin.is_none()) {
        let mut v: Vec<u8> = Vec::new();
        use std::io::Write;
        write!(&mut v, "http://localhost:{}/", port).expect("write to Vec");
        opts.origin = Some(v.into_boxed_slice());
    }

    let output_dir: Option<&[u8]> = None;
    let output_file: Option<&[u8]> = None;

    ctx.bundler_options.ignore_dce_annotations = args.flag(b"--ignore-dce-annotations");

if opts.entry_points.is_empty() {
           let mut entry_points: &[Box<[u8]>] = &ctx.positionals;

           if cmd == CommandTag::RunCommand
               && !entry_points.is_empty()
               && (&*entry_points[0] == b"run" || &*entry_points[0] == b"r")
           {
               entry_points = &entry_points[1..];
           }

           opts.entry_points = entry_points.to_vec();
       }

    let jsx_factory = args.option(b"--jsx-factory");
    let jsx_fragment = args.option(b"--jsx-fragment");
    let jsx_import_source = args.option(b"--jsx-import-source");
    let jsx_runtime = args.option(b"--jsx-runtime");
    let jsx_side_effects = args.flag(b"--jsx-side-effects");

    if matches!(cmd, CommandTag::AutoCommand | CommandTag::RunCommand) {
        // "run.silent" in bunfig.toml
        if args.flag(b"--silent") {
            ctx.debug.silent = true;
        }

        if let Some(elide_lines) = args.option(b"--elide-lines") {
            if !elide_lines.is_empty() {
                ctx.bundler_options.elide_lines = match strings::parse_int::<usize>(elide_lines, 10)
                {
                    Ok(v) => Some(v),
                    Err(_) => {
                        bun_core::pretty_errorln!(
                            "<r><red>error<r>: Invalid elide-lines: \"{}\"",
                            BStr::new(elide_lines)
                        );
                        Global::exit(1);
                    }
                };
            }
        }
    }

    if matches!(cmd, CommandTag::RunCommand | CommandTag::AutoCommand) {
        // "run.bun" in bunfig.toml
        if args.flag(b"--bun") {
            ctx.debug.run_in_bun = true;
        }
    }

    if jsx_factory.is_some()
        || jsx_fragment.is_some()
        || jsx_import_source.is_some()
        || jsx_runtime.is_some()
    {
        let default_factory: &[u8] = b"";
        let default_fragment: &[u8] = b"";
        let default_import_source: &[u8] = b"";
        if opts.jsx.is_none() {
            opts.jsx = Some(api::Jsx {
                factory: jsx_factory.unwrap_or(default_factory).into(),
                fragment: jsx_fragment.unwrap_or(default_fragment).into(),
                import_source: jsx_import_source.unwrap_or(default_import_source).into(),
                runtime: if let Some(runtime) = jsx_runtime {
                    resolve_jsx_runtime(runtime)
                } else {
                    api::JsxRuntime::Automatic
                },
                development: false,
                side_effects: jsx_side_effects,
            });
        } else {
            let prev = opts.jsx.take().unwrap();
            opts.jsx = Some(api::Jsx {
                factory: jsx_factory.map(Box::<[u8]>::from).unwrap_or(prev.factory),
                fragment: jsx_fragment.map(Box::<[u8]>::from).unwrap_or(prev.fragment),
                import_source: jsx_import_source
                    .map(Box::<[u8]>::from)
                    .unwrap_or(prev.import_source),
                runtime: if let Some(runtime) = jsx_runtime {
                    resolve_jsx_runtime(runtime)
                } else {
                    prev.runtime
                },
                development: false,
                side_effects: jsx_side_effects,
            });
        }
    }

    if let Some(log_level) = opts.log_level {
        bun_ast::DEFAULT_LOG_LEVEL.store(match log_level {
            api::MessageLevel::Debug => bun_ast::Level::Debug,
            api::MessageLevel::Err => bun_ast::Level::Err,
            api::MessageLevel::Warn => bun_ast::Level::Warn,
            _ => bun_ast::Level::Err,
        });
        // SAFETY: `ctx.log` is the CLI log, owned by the caller and not yet
        // shared with another thread.
        unsafe {
            (*ctx.log).level = bun_ast::DEFAULT_LOG_LEVEL.load();
        }
    }

    if args.flag(b"--no-macros") {
        ctx.debug.macros = MacroOptions::Disable;
    }

    opts.output_dir = output_dir.map(Box::<[u8]>::from);
    if let Some(of) = output_file {
        ctx.debug.output_file = of.into();
    }

    if matches!(cmd, CommandTag::RunCommand | CommandTag::AutoCommand) {
        if let Some(shell) = args.option(b"--shell") {
            if shell == b"bun" {
                ctx.debug.use_system_shell = false;
            } else if shell == b"system" {
                ctx.debug.use_system_shell = true;
            } else {
                Output::err_generic(
                    "Expected --shell to be one of 'bun' or 'system'. Received: \"{}\"",
                    format_args!("{}", BStr::new(shell)),
                );
                Global::exit(1);
            }
        }
    }

    if bun_core::env::SHOW_CRASH_TRACE {
        // argv slices are process-lifetime.
        bun_core::debug_flags::set_resolve_breakpoints(
            args.options(b"--breakpoint-resolve").to_vec(),
        );
    }

    Ok(opts)
}

