// Decoded-HTTP-request C ABI.
//
// This file used to be `libuwsockets_h3.cpp`: the whole HTTP/3 C ABI
// (app/listen-socket/response exports) on top of lsquic. bunre ships no QUIC
// stack, so only the request accessors survive — HTTP/2 needs the same
// decoded pseudo-header list (`uWS::Http3Request`, now built on
// `us_http_header_t`), and it reaches it through these same `uws_h3_req_*`
// link names. The Rust side lives in `src/uws_sys/http_request.rs`, which
// documents the same thing.

// clang-format off
#include "_libusockets.h"

#include <bun-uws/src/Http3Request.h>
#include <string_view>
#include <string.h>
// clang-format on

using uWS::Http3Request;

static inline std::string_view sv(const char* p, size_t n) { return p ? std::string_view { p, n } : std::string_view {}; }

extern "C" {

// Thin C-ABI wrappers around uWS methods — force ThinLTO to inline them into
// their (Rust) callers.
#pragma clang attribute push(__attribute__((always_inline)), apply_to = function)

typedef struct uws_h3_req_s uws_h3_req_t;

/* The FFI contract requires a non-null pointer; a default-
 * constructed string_view has data() == nullptr, so normalise empties. */
static inline size_t ffi_sv(std::string_view v, const char** dest)
{
    *dest = v.empty() ? "" : v.data();
    return v.length();
}

void uws_h3_req_set_yield(uws_h3_req_t* req, bool y) { ((Http3Request*)req)->setYield(y); }

size_t uws_h3_req_get_url(uws_h3_req_t* req, const char** dest)
{
    return ffi_sv(((Http3Request*)req)->getFullUrl(), dest);
}

size_t uws_h3_req_get_method(uws_h3_req_t* req, const char** dest)
{
    return ffi_sv(((Http3Request*)req)->getMethod(), dest);
}

size_t uws_h3_req_get_header(uws_h3_req_t* req, const char* lower, size_t lower_len, const char** dest)
{
    return ffi_sv(((Http3Request*)req)->getHeader(sv(lower, lower_len)), dest);
}

#pragma clang attribute pop

} // extern "C"