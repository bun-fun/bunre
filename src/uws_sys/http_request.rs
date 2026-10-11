//! Decoded HTTP request handle shared by the HTTP/2 and (removed) HTTP/3
//! transports.
//!
//! This was `h3::Request`. The type itself is transport-agnostic — the C++
//! side (`uWS::Http3Request`, now built on `us_http_header_t` rather than
//! `us_quic_header_t`) holds a decoded pseudo-header list either way, and
//! HTTP/2 reaches it through the same `uws_h3_req_*` exports. bunre has no
//! HTTP/3 stack, so the module was renamed to say what it actually is; the
//! `uws_h3_req_*` export names are kept because the C++ symbols still are.

use core::ptr;

bun_opaque::opaque_ffi! { pub struct Request; }

mod c {
    use super::*;

    // Opaque handle is `#[repr(C)]` with `UnsafeCell<[u8; 0]>`, so `&mut T` is
    // ABI-identical to a non-null pointer. Out-params are non-null and valid
    // for write and the shims only store a pointer into request-owned storage,
    // so the url/method shims are `safe fn`. The header lookup takes a
    // caller-supplied (ptr,len) name and stays `unsafe`.
    unsafe extern "C" {
        pub(super) safe fn uws_h3_req_set_yield(req: &mut Request, y: bool);
        pub(super) safe fn uws_h3_req_get_url(req: &mut Request, out: &mut *const u8) -> usize;
        pub(super) safe fn uws_h3_req_get_method(req: &mut Request, out: &mut *const u8) -> usize;
        pub(super) fn uws_h3_req_get_header(
            req: *mut Request,
            name: *const u8,
            len: usize,
            out: *mut *const u8,
        ) -> usize;
    }
}
impl Request {
    pub fn set_yield(&mut self, y: bool) {
        c::uws_h3_req_set_yield(self, y)
    }
    pub fn url(&mut self) -> &[u8] {
        let mut p: *const u8 = ptr::null();
        let n = c::uws_h3_req_get_url(self, &mut p);
        // SAFETY: uws returns a pointer+len pair valid for the lifetime of the request
        unsafe { bun_core::ffi::slice(p, n) }
    }
    pub fn method(&mut self) -> &[u8] {
        let mut p: *const u8 = ptr::null();
        let n = c::uws_h3_req_get_method(self, &mut p);
        // SAFETY: uws returns a pointer+len pair valid for the lifetime of the request
        unsafe { bun_core::ffi::slice(p, n) }
    }
    pub fn header(&mut self, name: &[u8]) -> Option<&[u8]> {
        let mut p: *const u8 = ptr::null();
        // SAFETY: self is a live FFI handle; name ptr/len valid for read; out-ptr is a valid local
        let n = unsafe { c::uws_h3_req_get_header(self, name.as_ptr(), name.len(), &raw mut p) };
        if n == 0 {
            None
        } else {
            // SAFETY: uws returns a pointer+len pair valid for the lifetime of the request
            Some(unsafe { bun_core::ffi::slice(p, n) })
        }
    }
}
