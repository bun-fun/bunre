//! CLI entry point + command dispatch.
//!
//! `Command::which()` + `HelpCommand` + `print_version_and_exit` compile
//! against lower-tier crates. `Command::start()` (full dispatch) and
//! per-command exec bodies live in the sibling `*_command.rs` modules.

use bun_core::strings;
use bun_core::{self as bun, Global, Output};
use bun_core::{pretty, pretty_error, pretty_errorln};

// ─── compiling submodules ────────────────────────────────────────────────────
#[path = "colon_list_type.rs"]
pub mod colon_list_type;
#[path = "open.rs"]
pub mod open;

#[path = "Arguments.rs"]
pub mod arguments;
pub use arguments as Arguments;
#[path = "run_command.rs"]
pub mod run_command;


// ─── crate-local helper for param-table concatenation ────────────────────────
// `bun_clap::parse_param!` is a real proc-macro (const `Param<Help>` literal),
// and `bun_clap::concat_params!` is a const-fn slice concat,
// so combined tables (`AUTO_PARAMS`, `RUN_PARAMS`, …) are baked into rodata —
// no `LazyLock`, no init closure in `.text`, no startup heap allocation.
pub use ::bun_clap::concat_params;

// ─── process-lifetime globals ────────────────────────────────────────────────
#[allow(non_upper_case_globals)]
// Owned `Box<[u8]>` so
// `process.title = "..."` (set_title) drops the previous value instead of
// leaking. The mutex provides exclusion between `get_title`/`set_title`.
pub(crate) static Bun__Node__ProcessTitle: bun_threading::Guarded<Option<Box<[u8]>>> =
    bun_threading::Guarded::new(None);

#[allow(non_upper_case_globals)]
/// `--redirect-warnings=<path>` — process warnings are appended to this file
/// instead of stderr (Node's flag; NODE_REDIRECT_WARNINGS is handled by the
/// C++ consumer as the fallback). Set once during CLI parse.
pub(crate) static Bun__Node__RedirectWarnings: std::sync::OnceLock<Box<[u8]>> =
    std::sync::OnceLock::new();

#[allow(non_upper_case_globals)]
/// `--disable-warning=<code-or-type>` (repeatable) — warnings whose `code`
/// or `name` matches an entry are suppressed. Set once during CLI parse.
pub(crate) static Bun__Node__DisabledWarnings: std::sync::OnceLock<Vec<Box<[u8]>>> =
    std::sync::OnceLock::new();

/// Backing storage for [`cli_arena`]. Written exactly once in [`Cli::start`]
/// during single-threaded process startup (before `Command::start`, hence
/// before any `cli_arena()` / `cli_dupe` caller), then read freely — same
/// "init once in `start()`" shape as `cli::LOG_`.
///
/// `RacyCell<MaybeUninit<…>>`, **not** `std::sync::LazyLock`: `LazyLock`'s init
/// thunk and the `std::sync::Once` poison/slow path it forces are `#[cold]`, and
/// fat-LTO parks them tens of MB away from the startup symbol cluster.
/// `cli_arena()` is on the hot `bun <file>` / `bun run <script>` path (via
/// `cli_dupe` / `cli_dupe_z` / `runner_arena`), so a `LazyLock` there faults a
/// fresh cold page on every `bun` invocation; a plain cell is the correct shape.
static CLI_ARENA: bun_core::RacyCell<core::mem::MaybeUninit<bun_alloc::Arena>> =
    bun_core::RacyCell::new(core::mem::MaybeUninit::uninit());

/// Process-lifetime arena for one-shot CLI commands; allocations live until
/// exit.
///
/// **Main-thread only.** `MimallocArena`'s `Sync` impl is *contract-only*:
/// `mi_heap_*` allocation calls are thread-local, and
/// `MimallocArena::assert_owning_thread()` debug-panics on cross-thread alloc.
/// The heap is pinned to the thread that ran [`Cli::start`] (the CLI dispatch /
/// main thread, which is where the arena is constructed). Do not call from
/// worker/watcher threads.
#[inline]
fn cli_arena() -> &'static bun_alloc::Arena {
    // SAFETY: `CLI_ARENA` is written exactly once in `Cli::start` during
    // single-threaded startup, before `Command::start` runs and therefore
    // before any caller of `cli_arena()` / `cli_dupe` / `cli_dupe_z` exists.
    // Read-only for the rest of the process lifetime.
    unsafe { (*CLI_ARENA.get()).assume_init_ref() }
}

/// Dupe `s` into the process-lifetime CLI arena. Replaces ad-hoc
/// `s.to_vec().into_boxed_slice()` leaks at CLI sites. Main-thread only
/// (see [`cli_arena`]).
#[inline]
pub(crate) fn cli_dupe(s: &[u8]) -> &'static [u8] {
    cli_arena().alloc_slice_copy(s)
}

/// This is set `true` during `Command.which()` if argv0 is "node", in which the CLI is going
/// to pretend to be node.js by always choosing RunCommand with a relative filepath.
/// Node-compat mode flag (set when argv0 is "node").
pub static PRETEND_TO_BE_NODE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

bun_core::declare_scope!(CLI, hidden);

pub(crate) type LoaderColonList =
    colon_list_type::ColonListType<bun_options_types::schema::api::Loader>;
pub(crate) type DefineColonList = colon_list_type::ColonListType<&'static [u8]>;

impl colon_list_type::ColonListValue for bun_options_types::schema::api::Loader {
    const IS_LOADER: bool = true;
    fn resolve_value(input: &[u8]) -> crate::Result<Self> {
        arguments::loader_resolver(input)
    }
}
impl colon_list_type::ColonListValue for &'static [u8] {
    fn resolve_value(input: &[u8]) -> crate::Result<Self> {
        // SAFETY: argv slices are process-lifetime; see ColonListType::keys note.
        Ok(unsafe { bun_ptr::detach_lifetime(input) })
    }
}

// ─── Cli (entry point) ───────────────────────────────────────────────────────
pub mod cli {
    use super::*;

    pub use bun_options_types::compile_target::CompileTarget;

    // Process-global, init in start().
    pub(crate) static LOG_: bun_core::RacyCell<core::mem::MaybeUninit<bun_ast::Log>> =
        bun_core::RacyCell::new(core::mem::MaybeUninit::uninit());

    /// `#[inline(never)]`: this is the first Rust call after `main()` (see
    /// `src/runtime/bin_entry/mod.rs`) and the head of the `bun <file>` / `bun run`
    /// startup chain. It must stay a concrete symbol so lld's
    /// `--symbol-ordering-file` (`src/startup.order`) can cluster it — and the
    /// callees it walks (`Command::start` → `which` → `create_context_data` →
    /// `Arguments::parse` → …) — into one contiguous front-loaded `.text` run.
    /// Without that, fat-LTO + `codegen-units=1` lay these out in
    /// crate-alphabetical order, scattering the cold-start path across pages
    /// shared with bundler/install/css/panic-format bodies.
    #[inline(never)]
    pub fn start() {
        // `bun_crash_handler::cli_state::is_main_thread()` (used to print the
        // `panic(main thread): …` header) compares against a stored OS tid.
        bun_crash_handler::cli_state::set_main_thread_id(bun_threading::current_thread_id());
        bun_core::set_start_time(bun_core::time::nano_timestamp());
        // SAFETY: single-threaded process startup
        unsafe { (*LOG_.get()).write(bun_ast::Log::init()) };
        // Init the process-lifetime CLI arena here (not via `LazyLock` on first
        // use) — see `super::CLI_ARENA`. The write happens before any worker
        // thread is spawned and before `Command::start` (the first
        // `cli_arena()` caller), so a plain `RacyCell` is sound.
        // SAFETY: single-threaded process startup; `mimalloc` is already init.
        unsafe { (*super::CLI_ARENA.get()).write(bun_alloc::Arena::new()) };

        // (The panic hook is installed by `bun_crash_handler::init()` in `bin_entry::main`.)
        // SAFETY: just initialized above; single-threaded for the lifetime of `log`.
        let log = unsafe { (*LOG_.get()).assume_init_mut() };
        if let Err(err) = Command::start(log) {
            // Print accumulated diagnostics BEFORE the
            // generic `handle_root_error` "An internal error occurred (..)"
            // message. The bake production path returns `error.BuildFailed`
            // with the actual parse/link errors sitting in `ctx.log` (== this
            // `log`); without this print, users see only the opaque error name.
            let _ = log.print(std::ptr::from_mut::<bun_core::io::Writer>(
                bun_core::Output::error_writer(),
            ));
            bun_crash_handler::handle_root_error(err);
        }
    }
}
pub use cli as Cli;

// ─── HelpCommand ─────────────────────────────────────────────────────────────
pub mod help_command {
    use super::*;

    #[derive(Copy, Clone, PartialEq, Eq)]
    pub enum Reason {
        Explicit,
        InvalidCommand,
    }

    #[cold]
    pub(crate) fn exec() -> crate::Result<()> {
        exec_with_reason(Reason::Explicit)
    }

    /// Emits the `pretty!`/`pretty_error!` call directly instead of
    /// expanding to a bare literal — `pretty!` captures its template as
    /// `$fmt:expr`, which is opaque to the `pretty_fmt!` proc-macro, so a
    /// nested `cli_helptext_fmt!()` inside `concat!()` would never be flattened.
    /// Taking the printer macro (and per-reason prefix line) as parameters keeps
    /// a single source of truth for the help body across both
    /// `Reason::Explicit` (stdout) and `Reason::InvalidCommand` (stderr).
    /// The spacing between commands is intentional.
    macro_rules! print_cli_helptext {
        ($printer:ident, $prefix:literal $(, $extra:expr)*) => {
            $printer!(
                concat!($prefix, "\
<b>Usage:<r> <b>bun \\<command\\> <cyan>[...flags]<r> <b>[...args]<r>

<b>Commands:<r>
  <b><magenta>run<r>       <d>./my-script.ts<r>       Execute a file with Bun
            <d>lint<r>                 Run a package.json script

  <d>\\<command\\><r> <b><cyan>--help<r>               Print help text for command.
"),
                $($extra,)*
            )
        };
    }

    // Tag/Reason lack `ConstParamTy` in lower-tier crates, so `reason` is a
    // runtime arg.
    pub(crate) fn print_with_reason(reason: Reason, show_all_flags: bool) {
        match reason {
            Reason::Explicit => {
                print_cli_helptext!(
                    pretty,
                    "<r><b><magenta>Bun<r> is a fast JavaScript runtime. <d>({})<r>\n\n",
                    Global::package_json_version_with_revision
                );
                if show_all_flags {
                    pretty!("\n<b>Flags:<r>");
                    bun_clap::simple_help_bun_top_level(arguments::AUTO_PARAMS);
                    pretty!("\n\n(more flags in <b>bun test --help<r> and <b>bun build --help<r>)\n");
                }
                pretty!(
                    "\nLearn more about Bun:            <magenta>https://bun.com/docs<r>\n\
Join our Discord community:      <blue>https://bun.com/discord<r>\n"
                );
            }
            Reason::InvalidCommand => {
                print_cli_helptext!(
                    pretty_error,
                    "<r><red>Uh-oh<r> not sure what to do with that command.\n\n"
                );
            }
        }

        Output::flush();
    }

    #[cold]
    fn exec_with_reason(reason: Reason) -> ! {
        print_with_reason(reason, false);
        if reason == Reason::InvalidCommand {
            Global::exit(1);
        }
        Global::exit(0);
    }
}
pub use help_command as HelpCommand;

pub mod reserved_command {
    use super::*;

    #[cold]
    pub(crate) fn exec() -> crate::Result<()> {
        let mut command_name: &[u8] = b"";
        for (i, arg) in bun::argv().iter().enumerate() {
            if i == 0 {
                continue;
            }
            if arg.len() > 1 && arg[0] == b'-' {
                continue;
            }
            command_name = arg;
            break;
        }
        if command_name.is_empty() {
            command_name = bun::argv().get(1).map(|z| z.as_bytes()).unwrap_or(b"");
        }
        pretty_error!(
            "<r><red>Uh-oh<r>. <b><yellow>bun {0}<r> is a subcommand reserved for future use by Bun.\n\nIf you were trying to run a package.json script called {0}, use <b><magenta>bun run {0}<r>.\n",
            bstr::BStr::new(command_name)
        );
        Output::flush();
        Global::exit(1);
    }
}
pub use reserved_command as ReservedCommand;

// ─── Command (Tag + which() + dispatch skeleton) ─────────────────────────────
pub mod command {
    use super::*;

    pub use bun_options_types::command_tag::Tag;
    pub use bun_options_types::command_tag::{LOADS_CONFIG, USES_GLOBAL_OPTIONS};
    pub use bun_options_types::context::{Context, ContextData, HotReload, TestOptions};

    // Process-lifetime
    // storage, written exactly once in `create_context_data` during
    // single-threaded startup. The pointer to it is published via
    // `bun_options_types::context::set_global` (single source of truth).
    static CONTEXT_DATA: bun_core::RacyCell<core::mem::MaybeUninit<ContextData>> =
        bun_core::RacyCell::new(core::mem::MaybeUninit::uninit());

    /// Process-global CLI context handle.
    #[inline]
    pub fn get() -> Context<'static> {
        // SAFETY: only called after `create_context_data` published the ctx
        // during single-threaded startup; callers treat the result as read-mostly.
        unsafe { &mut *bun_options_types::context::global_ptr() }
    }

    // ──────────────────────────────────────────────────────────────────────────
    // Canonical home: src/runtime/cli/mod.rs, inside `pub mod command { ... }`
    // (crate path `bun_runtime::cli::command::{is_bun_x, is_node, which}`).
    //
    // The `is_node` branch of `which()` must clear
    // `bun_clap::streaming::WARN_ON_UNRECOGNIZED_FLAG` so node-mode argv parsing
    // stays silent on unknown flags.
    // ──────────────────
    fn is_node(argv0: &[u8]) -> bool {
        #[cfg(windows)]
        {
            return strings::ends_with(argv0, b"node.exe") || strings::ends_with(argv0, b"node");
        }
        #[cfg(not(windows))]
        {
            strings::ends_with(argv0, b"node")
        }
    }

    /// Cheap argv prescan for the dominant `bun <path>` / `bun .` shape.
    ///
    /// `which()` classifies any first positional that isn't one of the ~40
    /// subcommand keywords as [`Tag::AutoCommand`] — but it pays for the
    /// `RootCommandMatcher` packed-u96 keyword table (and its rodata) to find
    /// that out, and `start()` then walks the full per-tag dispatch `match`.
    /// For a first positional that *looks* like a path — `.`/`..`, a `./`,
    /// `../`, `/` (or, on Windows, `\`, `.\`, `..\`, `X:\`) prefix, or
    /// anything whose basename carries a `.` (a file extension) — none of
    /// which can ever spell a subcommand keyword, so it is unambiguously
    /// `AutoCommand` and we can jump straight to the run path. Anything
    /// ambiguous (bare name like `run`/`x`, a leading `-flag`, a `node`/`bunx`
    /// shim, no args at all) falls through to `which()` unchanged.
    #[inline]
    fn looks_like_run_entrypoint(arg: &[u8]) -> bool {
        // Empty or option-like: let `which()`'s leading-flag skip loop handle it.
        let Some(&first) = arg.first() else {
            return false;
        };
        if first == b'-' {
            return false;
        }
        if arg == b"." || arg == b".." {
            return true;
        }
        // Unix relative/absolute path prefixes.
        if first == b'/' || arg.starts_with(b"./") || arg.starts_with(b"../") {
            return true;
        }
        #[cfg(windows)]
        {
            if first == b'\\' || arg.starts_with(b".\\") || arg.starts_with(b"..\\") {
                return true;
            }
            // Drive-letter root: `C:\…` / `C:/…`.
            if arg.len() >= 3
                && first.is_ascii_alphabetic()
                && arg[1] == b':'
                && (arg[2] == b'\\' || arg[2] == b'/')
            {
                return true;
            }
        }
        // Has a `.` in the basename — `foo.js`, `dir/foo.ts`, `.dotfile`, …
        // (no subcommand keyword contains a `.`).
        let basename = match strings::last_index_of_any(arg, b"/\\") {
            Some(i) => &arg[i + 1..],
            None => arg,
        };
        strings::contains_char(basename, b'.')
    }

    /// `#[inline(never)]`: argv→`Tag` classification, called once from
    /// `Cli::start` on every `bun` invocation. Kept a concrete symbol so
    /// `src/startup.order` can place it next to `Cli::start` /
    /// `create_context_data` (and the `RootCommandMatcher` helpers it pulls
    /// in) in the front-loaded startup window, rather than letting fat-LTO
    /// inline-and-scatter it through cold code.
    #[inline(never)]
    fn which() -> Tag {
        let argv = bun::argv();
        let mut iter = argv.iter();
        let Some(argv0) = iter.next() else {
            return Tag::HelpCommand;
        };

        if is_node(argv0) {
            // Node-mode must not warn on flags Bun doesn't know.
            bun_clap::streaming::WARN_ON_UNRECOGNIZED_FLAG
                .store(false, core::sync::atomic::Ordering::Relaxed);
            // SAFETY: single-threaded startup
            PRETEND_TO_BE_NODE.store(true, core::sync::atomic::Ordering::Relaxed);
            return Tag::RunAsNodeCommand;
        }

        let Some(mut first_arg_name) = iter.next() else {
            return Tag::AutoCommand;
        };
        while !first_arg_name.is_empty()
            && first_arg_name[0] == b'-'
            && !(first_arg_name.len() > 1 && first_arg_name[1] == b'e')
        {
            // `--interactive` stays on AutoCommand: Arguments.rs parses it and the no-target check
            // routes to RunCommand::exec_node_repl.
            match iter.next() {
                Some(n) => first_arg_name = n,
                None => return Tag::AutoCommand,
            }
        }

        type RootCommandMatcher = strings::ExactSizeMatcher<12>;
        let x = RootCommandMatcher::r#match(first_arg_name);
        if x == RootCommandMatcher::case(b"run") {
            return Tag::RunCommand;
        }
        if x == RootCommandMatcher::case(b"help") {
            return Tag::HelpCommand;
        }
        // reserved
        if x == RootCommandMatcher::case(b"deploy")
            || x == RootCommandMatcher::case(b"cloud")
            || x == RootCommandMatcher::case(b"config")
            || x == RootCommandMatcher::case(b"use")
            || x == RootCommandMatcher::case(b"auth")
            || x == RootCommandMatcher::case(b"login")
            || x == RootCommandMatcher::case(b"logout")
        {
            return Tag::ReservedCommand;
        }
        if x == RootCommandMatcher::case(b"-e") {
            return Tag::AutoCommand;
        }
        Tag::AutoCommand
    }

    /// Initialize the process-global `CONTEXT_DATA` and publish it via
    /// `Context::set_global`. Shared by `create_context_data` and the
    /// standalone-graph fast path in `start()`.
    fn write_context_no_parse(log: &mut bun_ast::Log) -> &'static mut ContextData {
        // SAFETY: single-threaded CLI startup; first and only write to
        // `CONTEXT_DATA` for the process lifetime. `log` is the `&'static mut`
        // borrow of `Cli::LOG_` taken in `Cli::start()`, so storing its raw
        // address is sound for the process lifetime.
        //
        // One `ContextData::default()` is constructed and written in place,
        // then the two non-default fields are patched on the live storage —
        // avoids the second `Default` temporary (and its drop) that the
        // `..Default::default()` struct-update form would build on the stack.
        unsafe {
            let ctx = (*CONTEXT_DATA.get()).write(ContextData::default());
            ctx.log = std::ptr::from_mut::<bun_ast::Log>(log);
            ctx.start_time = bun_core::start_time();
            bun_options_types::context::set_global(ctx);
            ctx
        }
    }

    /// `ContextData.create` — populates the global ctx and runs `Arguments::parse`.
    ///
    /// `Tag` lacks `ConstParamTy` (lower-tier crate), so `cmd` is a runtime
    /// arg; the runtime `USES_GLOBAL_OPTIONS` set covers per-command gating.
    /// Returns `&'static mut` to the process-global `CONTEXT_DATA`. Sound
    /// because CLI dispatch is single-threaded and this is the sole live
    /// borrow at the time of return; callers thread it down via the `ctx`
    /// parameter rather than re-deriving.
    ///
    /// `#[inline(never)]`: this is the `init` step of the `bun run <script>`
    /// dispatch chain (`exec_auto_or_run → init → RunCommand::exec_with_cfg`)
    /// and must stay a concrete symbol so `src/startup.order` can place it —
    /// and the argv→Context parsing it pulls in — contiguously. `#[track_caller]`
    /// would otherwise make it an inlining candidate, scattering those callees.
    #[track_caller]
    #[inline(never)]
    pub(crate) fn create_context_data(
        cmd: Tag,
        log: &mut bun_ast::Log,
    ) -> crate::Result<&'static mut ContextData> {
        bun_crash_handler::cli_state::set_cmd_char(cmd.char());

        let ctx = write_context_no_parse(log);

        if USES_GLOBAL_OPTIONS[cmd] {
            ctx.args = arguments::parse(cmd, ctx)?;
        }

        #[cfg(windows)]
        {
            if ctx.debug.hot_reload == HotReload::Watch {
                {
                    if !bun_sys::windows::is_watcher_child() {
                        bun_sys::windows::become_watcher_manager();
                    } else {
                        bun_core::set_auto_reload_on_crash(true);
                    }
                }
            }
        }

        Ok(ctx)
    }
    pub(crate) use create_context_data as init;

    /// Full subcommand dispatch.
    ///
    /// Kept deliberately tiny: every arm is a single call into a
    /// `#[cold] #[inline(never)]` helper so this compiles to a <1-page jump
    /// table. Previously this was a single 20 KB function and the
    /// `AutoCommand` arm body (the `bun --version` / `bun foo.js` hot path)
    /// sat at byte offset +0x142d behind ~5 KB of inlined standalone-graph
    /// setup and per-tag bodies — see perf sample 0x3d628fd. With the bodies
    /// out-lined, `start` is just `which()` + a `match` of tail calls.
    ///
    /// `#[inline(never)]`: the dispatch root must stay a concrete symbol so
    /// `src/startup.order`'s `--symbol-ordering-file` can anchor the `bun
    /// <file>` startup cluster on it (and keep `which` / `create_context_data`
    /// / `exec_auto_or_run` adjacent), instead of fat-LTO inlining it into
    /// `Cli::start` and re-scattering the per-tag tail calls.
    #[inline(never)]
    pub(crate) fn start(log: &mut bun_ast::Log) -> crate::Result<()> {
        // WebView host subprocess entry. Must be before StandaloneModuleGraph,
        // before JSC init, before anything that touches a JS engine. The child
        // runs CFRunLoopRun() as its real main loop — no Bun runtime past this.
        #[cfg(target_os = "macos")]
        {
            if let Some(fd_str) = bun_core::env_var::BUN_INTERNAL_WEBVIEW_HOST::get() {
                // Parse base-10 directly
                // from bytes; env var values are `&[u8]`, not assumed UTF-8.
                let fd: u32 = match bun_core::parse_int::<u32>(fd_str, 10).ok() {
                    Some(v) if v <= i32::MAX as u32 => v,
                    _ => Output::panic(format_args!(
                        "Invalid BUN_INTERNAL_WEBVIEW_HOST fd: {}",
                        bstr::BStr::new(fd_str),
                    )),
                };
                unsafe extern "C" {
                    // By-value `i32` only; noreturn entry point — no preconditions.
                    #[link_name = "Bun__WebView__hostMain"]
                    safe fn host_main(fd: i32) -> !;
                }
                host_main(fd as i32);
            }
        }

        // bun build --compile entry point
        if !bun_core::env_var::feature_flag::BUN_BE_BUN::get().unwrap_or(false) {
            if let Some(graph) = bun_standalone_graph::Graph::from_executable()? {
                // Never taken for a plain `bun` binary; ~2 KB of argv-splice
                // and ctx-setup code lives behind this cold call.
                return boot_standalone(graph, log);
            }
        }

        // Fast path: `bun -v` / `bun --version` / `bun --revision`, the
        // empty-eval forms `bun -e ''` / `bun -p ''` (and the `--eval=` /
        // `--print=` spellings), and the dominant `bun <path>` / `bun .` run
        // shape. Hoisted ABOVE `which()` and the per-tag `match` so these
        // common invocations never decode the subcommand-name classifier
        // (`which()` + its `RootCommandMatcher` name table / rodata) or walk
        // the per-tag dispatch `match`. `bun --version` also skips
        // `create_context_data` entirely (`arguments::parse` builds-and-drops
        // a full `api::TransformOptions` and forces two `LazyLock`s for what
        // is a no-op). Keeps `command::which`'s code/rodata and `arguments`'s
        // clap tables out of the `--version` / `bun <file>` working set.
        //
        // Correctness guards:
        //  * argv0 must be a plain `bun` invocation — a `node` / `bunx` shim
        //    must still fall through to `which()` so `node --version` reports
        //    Node's version, `bunx --version` is parsed by bunx, etc. Only
        //    the *predicates* are read here; `which()` performs the matching
        //    `PRETEND_TO_BE_NODE` / `IS_BUNX_EXE` side effects.
        //  * the standalone-graph probe above already ran, so a compiled
        //    executable's `--version` / `-e ''` is still passed through to
        //    user code (it returned via `boot_standalone`).
        //  * the version check is exact-argv-shape (`len == 2`) so it cannot
        //    intercept `bun <bin> --version`, where the flag belongs to
        //    `<bin>` (the bug the old argv-scan shim had — see the
        //    NOTE below). The empty-eval check is likewise exact-shape, falling
        //    through to `HelpCommand.exec`.
        {
            let argv = bun::argv();
            let argv0 = argv.get(0).map(bun_core::ZStr::as_bytes).unwrap_or(b"");
            if !is_node(argv0) {
                if argv.len() == 2 {
                    match argv.get(1).map(bun_core::ZStr::as_bytes) {
                        Some(b"-v" | b"--version") => print_version_and_exit(),
                        Some(b"--revision") => print_revision_and_exit(),
                        _ => {}
                    }
                }

                let empty_eval = match argv.len() {
                    2 => matches!(
                        argv.get(1).map(bun_core::ZStr::as_bytes),
                        Some(b"-e=" | b"-p=" | b"--eval=" | b"--print=")
                    ),
                    3 => {
                        argv.get(2).is_some_and(|a| a.as_bytes().is_empty())
                            && matches!(
                                argv.get(1).map(bun_core::ZStr::as_bytes),
                                Some(b"-e" | b"-p" | b"--eval" | b"--print")
                            )
                    }
                    _ => false,
                };
                if empty_eval {
                    Output::flush();
                    return HelpCommand::exec();
                }

                // `bun <path>` / `bun .` — the dominant run shape. argv[1] is
                // path-shaped (`looks_like_run_entrypoint`), which no
                // subcommand keyword can be, so `which()` would unambiguously
                // return `Tag::AutoCommand`; short-circuit straight to that
                // arm so a plain `bun <file>` never decodes the subcommand
                // classifier (`which()` + its `RootCommandMatcher` keyword
                // table / rodata) or walks the per-tag dispatch `match` below.
                // Dispatches to exactly the arm `which()` would have selected,
                // so config loading / arg parsing / passthrough are unchanged.
                if argv
                    .get(1)
                    .map(bun_core::ZStr::as_bytes)
                    .is_some_and(looks_like_run_entrypoint)
                {
                    return exec_auto_or_run(Tag::AutoCommand, log);
                }
            }
        }

        let tag = which();

        // NOTE: an earlier shim used to scan all of `argv` here for
        // `--version`/`--help`/`--revision` and short-circuit, because
        // `Arguments::parse` was gated. That shim is removed now that
        // `arguments::parse` (called via `init` → `create_context_data`) is
        // live and honours `stop_after_positional_at = 1` — the shim broke
        // `bun <bin> --version` by intercepting the flag meant for `<bin>`.

        match tag {
            Tag::AutoCommand | Tag::RunCommand => exec_auto_or_run(tag, log),
            Tag::HelpCommand => HelpCommand::exec(),
            Tag::ReservedCommand => ReservedCommand::exec(),
            Tag::RunAsNodeCommand => exec_run_as_node(log),
            _ => {
                tag_print_help(tag, true);
                Ok(())
            }
        }
    }

    // ─── out-lined `start` arm bodies ───────────────────────────────────────
    // Every per-tag body lives in its own `#[cold] #[inline(never)]` fn so
    // `start` itself stays a jump table. The `Auto/Run` arm is the hot path
    // (`bun foo.js`, `bun --version`), so it gets `#[inline(never)]` only —
    // no `#[cold]` — to avoid pessimising branch weights / section placement.

    type CmdResult = crate::Result<()>;

    /// `bun build --compile` standalone-executable boot. Never taken for a
    /// plain `bun` binary; out-lined so the ~2 KB of argv-splice / ctx-setup
    /// code is not decoded on the `bun --version` path.
    #[cold]
    #[inline(never)]
    fn boot_standalone(
        graph: *mut bun_standalone_graph::Graph,
        log: &mut bun_ast::Log,
    ) -> CmdResult {
        // SAFETY: `from_executable` returns a non-null `*mut Graph` whose
        // backing storage is process-static (owned by the executable image).
        let graph: &mut bun_standalone_graph::Graph = unsafe { &mut *graph };
        let offset_for_passthrough: usize;

        let ctx: &mut ContextData = 'brk: {
            // The entry point (`bin_entry::main`) defers argv
            // init to `bun_core::argv()`'s lazy `Once`, so force that init
            // now — otherwise `bun_options_argc()` reads 0 here and the
            // standalone executable silently drops `BUN_OPTIONS` flags.
            let original_argv_len = bun::argv().len();
            let bun_options_argc = bun::bun_options_argc();
            if !graph.compile_exec_argv.is_empty() || bun_options_argc > 0 {
                let mut argv_list: Vec<&'static bun_core::ZStr> = bun::argv().to_vec();
                if !graph.compile_exec_argv.is_empty() {
                    bun::append_options_env(graph.compile_exec_argv, &mut argv_list);
                }

                // Store the full argv including user arguments
                let full_argv: &'static [&'static bun_core::ZStr] = bun::intern_argv(argv_list);
                let num_exec_argv_options = full_argv.len().saturating_sub(original_argv_len);

                // Calculate offset: skip executable name + all exec argv options + BUN_OPTIONS args
                let num_parsed_options = num_exec_argv_options + bun_options_argc;
                offset_for_passthrough = if full_argv.len() > 1 {
                    1 + num_parsed_options
                } else {
                    0
                };

                // Temporarily set bun.argv to only include executable name + exec_argv options + BUN_OPTIONS args.
                // This prevents user arguments like --version/--help from being intercepted
                // by Bun's argument parser (they should be passed through to user code).
                // SAFETY: single-threaded startup; `full_argv` is process-static.
                unsafe {
                    bun::set_argv(&full_argv[..(1 + num_parsed_options).min(full_argv.len())]);
                }

                // Handle actual options to parse.
                let result = init(Tag::AutoCommand, log)?;

                // Restore full argv so passthrough calculation works correctly
                // SAFETY: single-threaded startup.
                unsafe { bun::set_argv(full_argv) };

                break 'brk result;
            }

            // If no compile_exec_argv, skip executable name if present
            offset_for_passthrough = 1.min(bun::argv().len());

            break 'brk write_context_no_parse(log);
        };

        ctx.args.target = Some(bun_options_types::schema::api::Target::Bun);
        use bun_options_types::global_cache::GlobalCache;
        if ctx.debug.global_cache == GlobalCache::auto {
            ctx.debug.global_cache = GlobalCache::disable;
        }

        ctx.passthrough = bun::argv()
            .iter()
            .skip(offset_for_passthrough)
            .map(|a| a.to_vec().into_boxed_slice())
            .collect();

        let entry_name = graph.entry_point().name.to_vec().into_boxed_slice();
        super::run_command::RunCommand::boot_standalone(ctx, entry_name, graph)?;
        Ok(())
    }

    /// `bun [run] <script>` / `bun --version` / bare `bun`. The dominant tag
    /// pair — kept out-of-line so `start` is a jump table, but *not* `#[cold]`.
    #[inline(never)]
    fn exec_auto_or_run(tag: Tag, log: &mut bun_ast::Log) -> CmdResult {
        // The AutoCommand arm swallows
        // `error.MissingEntryPoint` from `Command.init` and prints help;
        // every other tag (including RunCommand) propagates the error.
        // Note: nothing currently produces `MissingEntryPoint`; bare
        // `bun` help is served by the empty-positionals fallthrough. This arm
        // exists in case a producer is ever added (Arguments.rs).
        let ctx = match init(tag, log) {
            Ok(ctx) => ctx,
            Err(e) if tag == Tag::AutoCommand && matches!(e, crate::Error::MissingEntryPoint) => {
                return HelpCommand::exec();
            }
            Err(e) => return Err(e),
        };
        ctx.args.target = Some(bun_options_types::schema::api::Target::Bun);

        // `--filter` / `--workspaces` / `--parallel` / `--sequential` enumerate
        // workspace packages through the package manager, which bunre does not
        // ship. Reject them instead of silently running in the cwd package.
        if ctx.parallel || ctx.sequential || !ctx.filters.is_empty() || ctx.workspaces {
            pretty_errorln!(
                "<r><red>error<r>: --filter / --workspaces / --parallel / --sequential need the package manager, which bunre does not include"
            );
            Global::exit(1);
        }

        // Node: `-i foo.js` runs the script; `-i -e code` evals then enters the
        // REPL (via process._eval). `-i -p` is not yet threaded through the
        // bootstrap (Node prints AND enters the REPL), so `-p` currently
        // bypasses the REPL. RunCommand's positionals carry a leading "run".
        if ctx.runtime_options.interactive && !ctx.runtime_options.eval.eval_and_print {
            let no_target = match tag {
                Tag::AutoCommand => ctx.positionals.is_empty(),
                Tag::RunCommand => match ctx.positionals.as_slice() {
                    [] => true,
                    [r] => r.as_ref() == b"run",
                    _ => false,
                },
                _ => false,
            };
            if no_target {
                return run_command::RunCommand::exec_node_repl(ctx);
            }
        }

        if tag == Tag::AutoCommand && !ctx.runtime_options.eval.script.is_empty() {
            return run_command::RunCommand::exec_eval(ctx);
        }

        if !ctx.positionals.is_empty() {
            let cfg = run_command::ExecCfg {
                bin_dirs_only: tag == Tag::AutoCommand,
                log_errors: tag != Tag::AutoCommand || !ctx.runtime_options.if_present,
                allow_fast_run_for_extensions: tag == Tag::AutoCommand,
            };
            if run_command::RunCommand::exec_with_cfg(ctx, cfg)? {
                return Ok(());
            }
            if tag == Tag::RunCommand {
                Global::exit(1);
            }
            return Ok(());
        }

        if tag == Tag::AutoCommand {
            Output::flush();
            return HelpCommand::exec();
        }
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn exec_run_as_node(log: &mut bun_ast::Log) -> CmdResult {
        let ctx = init(Tag::RunAsNodeCommand, log)?;
        run_command::RunCommand::exec_as_if_node(ctx)
    }

    pub(crate) fn tag_print_help(cmd: Tag, show_all_flags: bool) {
        match cmd {
            Tag::AutoCommand | Tag::HelpCommand => {
                HelpCommand::print_with_reason(HelpCommand::Reason::Explicit, show_all_flags);
            }
            Tag::RunCommand | Tag::RunAsNodeCommand => {
                run_command::RunCommand::print_help(None);
            }
            Tag::ReservedCommand => {
                pretty!(
                    "\
<b>Usage<r>: <b><green>bun \\<command\\><r>
  This command is reserved for future use by Bun.
"
                );
                Output::flush();
            }
            _ => HelpCommand::print_with_reason(HelpCommand::Reason::Explicit, false),
        }
    }
}
pub use command as Command;

// NOT `#[cold]` — `bun --version` is the most-benchmarked startup path, and
// `#[cold]` relocates the body to `.text.unlikely` ~40 MB past the
// startup.order cluster. The symbol is listed in src/startup.order instead.
pub(crate) fn print_version_and_exit() -> ! {
    // The version string is plain ASCII (no `<tag>` markup), so bypass
    // `Output::pretty(format_args!(..))` — that path renders the `Arguments`
    // into a heap `String`, then runs the runtime `<tag>` rewriter into a
    // second `Vec<u8>`, all to print a ~10-byte constant. Write the bytes
    // straight to the buffered stdout writer instead. One `write_all` (the
    // `\n` is baked into the constant) → one syscall.
    let w = Output::writer();
    let _ = w.write_all(Global::package_json_version_nl.as_bytes());
    Output::flush();
    Global::exit(0);
}

#[cold]
pub(crate) fn print_revision_and_exit() -> ! {
    // See `print_version_and_exit` — plain bytes, no `<tag>` rewrite needed.
    let w = Output::writer();
    let _ = w.write_all(Global::package_json_version_with_revision.as_bytes());
    let _ = w.write_all(b"\n");
    Output::flush();
    Global::exit(0);
}
