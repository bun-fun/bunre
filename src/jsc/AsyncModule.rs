
use bun_alloc::Arena as ArenaAllocator;
use bun_bundler::transpiler::ParseResult;
use bun_core::String as BunString;
use bun_io::KeepAlive;
use bun_resolver::fs as Fs;

use crate::virtual_machine::VirtualMachine;
use crate::{
    self as jsc, ErrorableResolvedSource, JSGlobalObject, JSValue, JsError, JsResult,
    ResolvedSource, StrongOptional,
};

bun_core::declare_scope!(AsyncModule, hidden);

pub struct AsyncModule {
    // This is all the state used by the printer to print the module
    pub(crate) parse_result: ParseResult<'static>,
    pub(crate) promise: StrongOptional, // Strong.Optional, default .empty
    /// Packed `referrer ++ specifier ++ path.text`. Owns the bytes; stored as offsets so
    /// the struct stays movable (no self-referential borrows); reconstruct
    /// slices via `referrer()` / `specifier()` / `path_text()`.
    pub(crate) string_buf: Box<[u8]>,
    referrer_len: u32,
    specifier_len: u32,
    // `*JSGlobalObject` is a VM-lifetime backref (BACKREF/JSC_BORROW class in
    // LIFETIMES.tsv); [`crate::GlobalRef`] encapsulates the single audited
    // deref.
    pub global_this: crate::GlobalRef,
    pub(crate) arena: Box<ArenaAllocator>,
    /// See [`InitOpts::ast_alloc_state`].
    pub ast_alloc_state: Option<Box<bun_alloc::ast_alloc::AstAllocState>>,

    // This is the specific state for making it async
    pub(crate) poll_ref: KeepAlive,
}

pub type Map = Vec<AsyncModule>;

impl AsyncModule {
    #[inline]
    pub(crate) fn referrer(&self) -> &[u8] {
        &self.string_buf[..self.referrer_len as usize]
    }

    #[inline]
    pub(crate) fn specifier(&self) -> &[u8] {
        let off = self.referrer_len as usize;
        &self.string_buf[off..off + self.specifier_len as usize]
    }

    #[inline]
    pub(crate) fn path_text(&self) -> &[u8] {
        let off = self.referrer_len as usize + self.specifier_len as usize;
        &self.string_buf[off..]
    }

    /// Dispatch the (possibly errored) transpile
    /// result back into JSC via `Bun__onFulfillAsyncModule`. Called from
    /// `RuntimeTranspilerStore::run_from_js_thread` and `on_done` when a
    /// concurrent transpile job finishes.
    pub(crate) fn fulfill(
        global_this: &JSGlobalObject,
        promise: JSValue,
        result: Result<ResolvedSource, crate::CrateError>,
        specifier: &BunString,
        referrer: &BunString,
        log: &mut bun_ast::Log,
    ) -> JsResult<()> {
        jsc::mark_binding();
        let mut errorable = match result {
            Ok(resolved_source) => ErrorableResolvedSource::ok(resolved_source),
            Err(
                crate::CrateError::JSError | crate::CrateError::Bundler(bun_bundler::Error::Js(_)),
            ) => ErrorableResolvedSource::err(global_this.take_error(JsError::Thrown)),
            Err(e) => ErrorableResolvedSource::err(crate::virtual_machine::process_fetch_log(
                global_this,
                specifier,
                referrer,
                log,
                e,
            )),
        };
        bun_core::scoped_log!(AsyncModule, "fulfill: {}", specifier);

        jsc::from_js_host_call_generic(global_this, || {
            Bun__onFulfillAsyncModule(global_this, promise, &mut errorable, specifier, referrer)
        })
    }
}

// pub fn deinit → impl Drop. Body only freed owned fields (promise,
// parse_result, arena, string_buf), all of which now have Drop impls on their
// Rust types. No explicit Drop needed; relying on field Drop order.
// bun.default_allocator.free(this.stmt_blocks);
// bun.default_allocator.free(this.expr_blocks);

// safe: `JSGlobalObject` is an opaque `UnsafeCell`-backed ZST handle (`&` is
// ABI-identical to non-null `*const`); `res` stays owned by this frame — C++
// takes the fields it keeps by transfer (zeroing them) and the rest drops here.
unsafe extern "C" {
    #[allow(improper_ctypes)]
    safe fn Bun__onFulfillAsyncModule(
        global_object: &JSGlobalObject,
        promise_value: JSValue,
        res: &mut ErrorableResolvedSource,
        specifier: &BunString,
        referrer: &BunString,
    );
}




/// `RunTasksCallbacks` impl for the auto-install module queue. `onResolve` /
/// `onPackageManifestError` / `onPackageDownloadError` forward to the `Queue`
/// methods, `progress_bar` selected via const generic to match the
/// `enable_ansi_colors_stderr` branch.
impl AsyncModule {
    #[allow(
        clippy::boxed_local,
        reason = "reclaim point for the box `done()` handed to the task queue"
    )]
    pub fn on_done(mut this: Box<AsyncModule>) -> JsResult<()> {
        jsc::mark_binding();
        // Copy the `GlobalRef` out (it is `Copy`) so the borrow of `this` ends
        // before `&mut this` reborrows below; deref via the local for the rest
        // of the function. `GlobalRef::deref` encapsulates the JSC_BORROW
        // lifetime invariant, so no raw-pointer deref is open-coded here.
        let global_ref = this.global_this;
        let global_this: &JSGlobalObject = &global_ref;
        // SAFETY: `VirtualMachine::get()` is the live per-thread VM (one VM per
        // thread).
        let mut log = bun_ast::Log::init();
        this.poll_ref.unref(bun_io::posix_event_loop::get_vm_ctx(
            bun_io::AllocatorType::Js,
        ));
        let result = this.resume_loading_module(&mut log);
        let spec = BunString::borrow_utf8(this.specifier());
        let referrer = BunString::borrow_utf8(this.referrer());
        Self::fulfill(
            global_this,
            this.promise.get().unwrap(),
            result,
            &spec,
            &referrer,
            &mut log,
        )
    }

    pub(crate) fn resume_loading_module(
        &mut self,
        log: &mut bun_ast::Log,
    ) -> crate::CrateResult<ResolvedSource> {
        bun_core::scoped_log!(
            AsyncModule,
            "resumeLoadingModule: {}",
            bstr::BStr::new(self.specifier())
        );
        // Take `parse_result` by value via `mem::take`, then restore below, to
        // satisfy borrowck around `linker.link(&mut parse_result)` while
        // `self` is also borrowed.
        let arena = *self.parse_result.ast.parts.allocator();
        let mut parse_result =
            core::mem::replace(&mut self.parse_result, ParseResult::empty(arena));
        // SAFETY: `string_buf` is a `Box<[u8]>` whose backing allocation is
        // stable for the lifetime of `*self`; this fn never replaces it, so
        // slices into it remain valid across the `&mut self` reborrows below
        // (`self.parse_result = ...`). Detach the borrow so borrowck doesn't
        // tie `path`/`specifier` to `&self`.
        let specifier: &[u8] = unsafe { bun_ptr::detach_lifetime(self.specifier()) };
        // SAFETY: same `string_buf` stability invariant as `specifier` above —
        // the backing `Box<[u8]>` is never replaced in this fn.
        let path_text: &[u8] = unsafe { bun_ptr::detach_lifetime(self.path_text()) };
        let path = Fs::Path::init(path_text);
        let jsc_vm = VirtualMachine::get_mut_ptr();
        // SAFETY: `jsc_vm` is the live per-thread VM (one VM per thread)
        // (`transpiler.log`/`resolver.log`/`linker.log` are themselves raw
        // `*mut Log` aliased deliberately — see `Transpiler::set_log`).
        // `vm.log` is set unconditionally in `init` and never cleared, so the
        // `expect` is infallible.
        let old_log: core::ptr::NonNull<bun_ast::Log> =
            unsafe { (*jsc_vm).log }.expect("vm.log set in init");

        let log_nn = core::ptr::NonNull::new(log).expect("AsyncModule log is non-null");
        let log_ptr: *mut bun_ast::Log = log;
        // SAFETY: see above — single-thread VM; raw-ptr field stores.
        unsafe {
            (*jsc_vm).transpiler.linker.log = log_ptr;
            (*jsc_vm).transpiler.log = log_ptr;
            (*jsc_vm).transpiler.resolver.log = log_nn;
        }
        let _restore = scopeguard::guard((jsc_vm, old_log), |(jsc_vm, old_log)| {
            // SAFETY: same per-thread VM; restoring the original log pointers
            // stored above.
            unsafe {
                let old_log_ptr = old_log.as_ptr();
                (*jsc_vm).transpiler.linker.log = old_log_ptr;
                (*jsc_vm).transpiler.log = old_log_ptr;
                (*jsc_vm).transpiler.resolver.log = old_log;
            }
        });

        // We _must_ link because:
        // - node_modules bundle won't be properly
        // SAFETY: per-thread VM; `linker` is a value field of `transpiler`.
        unsafe {
            (*jsc_vm).transpiler.linker.link::<false, true>(
                &path,
                &mut parse_result,
                &(*jsc_vm).origin,
                bun_bundler::options::ImportPathFormat::AbsolutePath,
            )?;
        }
        self.parse_result = parse_result;
        // `print_with_source_map` consumes `ParseResult` by
        // value (it moves `ast` into `print_ast`). Hoist the post-print
        // read (`is_commonjs_module`) above the move so we
        // can `mem::take` instead of cloning.
        let is_commonjs_module = self.parse_result.ast.has_commonjs_export_names
            || self.parse_result.ast.exports_kind == bun_ast::ExportsKind::Cjs;
        let arena = *self.parse_result.ast.parts.allocator();
        let parse_result = core::mem::replace(&mut self.parse_result, ParseResult::empty(arena));

        // `VirtualMachine.source_code_printer` is a thread-local
        // `?*BufferPrinter` (see `SOURCE_CODE_PRINTER`). `BufferPrinter` is `!Clone`, so
        // swap the buffer out and write it back via the `_writeback`
        // guard — same observable effect (the thread-local's buffer is
        // reused). Matches RuntimeTranspilerStore.rs.
        let mut printer_ptr = crate::virtual_machine::SOURCE_CODE_PRINTER
            .get()
            .expect("source_code_printer not initialized");
        // SAFETY: thread-local owns the leaked Box; only this thread touches it.
        let mut printer = core::mem::replace(
            unsafe { printer_ptr.as_mut() },
            bun_js_printer::BufferPrinter::init(bun_js_printer::BufferWriter::init()),
        );
        printer.ctx.reset();
        // The writeback must fire at fn exit,
        // *after* the `printer.ctx.get_written()` reads below. Declare the
        // guard immediately after `printer` so it drops last (locals drop in
        // reverse declaration order) and the buffer is still populated when
        // read.
        let _writeback =
            scopeguard::guard((printer_ptr.as_ptr(), &raw mut printer), |(dst, src)| {
                // SAFETY: `dst` is the thread-local's leaked Box, `src` is the
                // stack `printer`; both outlive this guard (it drops before
                // `printer`). Move the buffer back into the thread-local slot.
                unsafe {
                    *dst = core::mem::replace(
                        &mut *src,
                        bun_js_printer::BufferPrinter::init(bun_js_printer::BufferWriter::init()),
                    )
                };
            });

        {
            // SAFETY: per-thread VM; `source_map_handler` stashes the
            // `*mut BufferPrinter` and only reborrows inside
            // `on_source_map_chunk` after the writer's last use retires.
            let mut mapper = unsafe { (*jsc_vm).source_map_handler(&raw mut printer) };
            // SAFETY: per-thread VM.
            let _ = unsafe {
                (*jsc_vm).transpiler.print_with_source_map(
                    // `self.arena` is the same per-call arena that built
                    // `parse_result.ast` (handed to the queue via
                    // `InitOpts::arena` after the original parse). The
                    // printer's rope-flattening scratch belongs in it, not
                    // in the per-VM `transpiler_arena`.
                    &self.arena,
                    parse_result,
                    &mut printer,
                    bun_js_printer::Format::EsmAscii,
                    mapper.get(),
                    None,
                )
            }?;
        }

        // `bun_core::env::DUMP_SOURCE` is debug, non-test builds only. The previous
        // `cfg(feature = "dump_source")` gate referenced a feature that doesn't
        // exist, which silently compiled this call out everywhere.
        if bun_core::env::DUMP_SOURCE {
            crate::runtime_transpiler_store::dump_source_string(
                // SAFETY: `jsc_vm` is the live per-thread `VirtualMachine` (BACKREF, non-null).
                unsafe { core::ptr::NonNull::new_unchecked(jsc_vm) },
                specifier,
                printer.ctx.get_written(),
            );
        }

        // No watcher registration here: `maybe_watch_file` already ran before
        // the enqueue, and the fd the parse opened may have been closed (and
        // the number recycled) by the transpile frame's fd guard.

        // SAFETY: per-thread VM.
        if unsafe { (*jsc_vm).is_watcher_enabled() } {
            // SAFETY: per-thread VM.
            let mut resolved_source = unsafe {
                (*jsc_vm).ref_counted_resolved_source(
                    printer.ctx.get_written(),
                    &BunString::from_bytes(specifier),
                    path.text,
                    None,
                )
            };

            resolved_source.is_commonjs_module = is_commonjs_module;

            return Ok(resolved_source);
        }

        Ok(ResolvedSource {
            source_code: BunString::clone_latin1(printer.ctx.get_written()),
            source_url: BunString::from_bytes(path.text),
            is_commonjs_module,
            ..Default::default()
        })
    }
}
