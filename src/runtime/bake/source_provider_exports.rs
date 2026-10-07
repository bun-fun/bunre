//! Rust side of `BakeSourceProvider.h` / `DevServerSourceProvider.h`: the
//! FFI bindings and `SourceProvider` impls for bake's two C++ source
//! providers, plus the host exports that register them with the VM's
//! `SavedSourceMap` so stack remapping can resolve dev-server /
//! bake-production output. `bun_sourcemap` sees these only as erased
//! `AnySourceProvider` handles.
//!
//! `#[unsafe(no_mangle)] extern "C"` thunks are emitted by
//! `src/codegen/generate-host-exports.ts` from the `// HOST_EXPORT(Sym, c)`
//! markers; the bodies take safe `&mut VirtualMachine` / `&BunString` borrows.

use core::ffi::c_void;

use bun_core::String as BunString;
use bun_jsc::virtual_machine::VirtualMachine;
use bun_sourcemap::parsed_source_map::AnySourceProvider;
use bun_sourcemap::{SourceContentPtr, SourceProvider};

bun_opaque::opaque_ffi! {
    /// Opaque handle to the C++ `Bake::DevServerSourceProvider`
    /// (`DevServerSourceProvider.cpp`).
    pub(crate) struct DevServerSourceProvider;
}

#[repr(C)]
struct DevServerSourceMapData {
    ptr: *const u8,
    length: usize,
}

unsafe extern "C" {
    // The C++ accessors are read-only (`provider->source()` /
    // `provider->sourceMapJSON()`). Taking `*const` avoids casting away const
    // from the `&self` borrows below; any interior mutation lives behind the
    // FFI boundary in C++-owned storage that Rust has no provenance over
    // (these types are opaque ZST markers).
    fn DevServerSourceProvider__getSourceSlice(
        this: &DevServerSourceProvider,
    ) -> bun_core::StringView<'_>;
    fn DevServerSourceProvider__getSourceMapJSON(
        this: *const DevServerSourceProvider,
    ) -> DevServerSourceMapData;
}

impl SourceProvider for DevServerSourceProvider {
    const HAS_SOURCE_MAP_JSON: bool = true;

    fn get_source_slice(&self) -> bun_core::StringView<'_> {
        // SAFETY: opaque FFI handle.
        unsafe { DevServerSourceProvider__getSourceSlice(self) }
    }

    fn to_source_content_ptr(&self) -> SourceContentPtr {
        SourceContentPtr::from_source_provider::<Self>(self)
    }

    fn get_source_map_json(&self) -> Option<&[u8]> {
        // SAFETY: opaque FFI handle; address-only pass-through, callee does
        // not write Rust-visible memory.
        let d = unsafe { DevServerSourceProvider__getSourceMapJSON(self) };
        if d.length == 0 {
            return None;
        }
        // SAFETY: ptr/length come from C++ and are valid for the call duration
        Some(unsafe { core::slice::from_raw_parts(d.ptr, d.length) })
    }

    fn warn_invalid_source_map_json(&self, source_filename: &[u8], err: bun_sourcemap::Error) {
        bun_core::warn!(
            "Could not decode sourcemap in dev server runtime: {} - {}",
            ::bstr::BStr::new(source_filename),
            ::bstr::BStr::new(err.name()),
        );
    }
}

// HOST_EXPORT(Bun__addDevServerSourceProvider, c)
pub fn add_dev_server_source_provider(
    vm: &mut VirtualMachine,
    opaque_source_provider: *mut c_void,
    specifier: &BunString,
) {
    let slice = specifier.to_utf8();
    vm.source_mappings.put_source_provider(
        AnySourceProvider::new(
            opaque_source_provider
                .cast::<DevServerSourceProvider>()
                .cast_const(),
        ),
        slice.slice(),
    );
}

// HOST_EXPORT(Bun__removeDevServerSourceProvider, c)
pub fn remove_dev_server_source_provider(
    vm: &mut VirtualMachine,
    opaque_source_provider: *mut c_void,
    specifier: &BunString,
) {
    let slice = specifier.to_utf8();
    vm.source_mappings
        .remove_source_provider(opaque_source_provider, slice.slice());
}
