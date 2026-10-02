// Compatibility shim for oven-sh BoringSSL fork additions that are used by
// bun-usockets and the Rust layer but not yet exposed in the pinned fork's
// public headers.
//
// The oven-sh/boringssl fork adds X509_LAZY_CERT_SET (a lazily-parsed
// certificate set) and X509_V_FLAG_IGNORE_EXPIRED_TRUST_ANCHORS. The
// bun-usockets C/C++ code (openssl.c, root_certs.cpp) and the Rust layer
// (system_certs.rs) call these APIs, but the pinned vendor/boringssl archive
// predates the implementations.
//
// The definitions live in vendor/boringssl/crypto/x509/x509_lazy_cert_set.cc
// (added to the boringssl direct-build source list) and are resolved at link
// time; this header supplies the missing declarations.

#ifndef US_BORINGSSL_LAZY_CERT_SHIM_H
#define US_BORINGSSL_LAZY_CERT_SHIM_H

#include <openssl/base.h>
#include <openssl/x509.h>
#include <stddef.h>

// Opaque type: a set of DER-encoded certificates that BoringSSL parses
// lazily on first use during chain verification.
typedef struct X509_LAZY_CERT_SET X509_LAZY_CERT_SET;

// X509_V_FLAG_IGNORE_EXPIRED_TRUST_ANCHORS: when set, expired trust anchors
// are treated as absent rather than shadowing the currently-valid certificate
// for the same issuer.
#ifndef X509_V_FLAG_IGNORE_EXPIRED_TRUST_ANCHORS
#define X509_V_FLAG_IGNORE_EXPIRED_TRUST_ANCHORS 0x400000
#endif

// All functions are C linkage so the definitions in x509_lazy_cert_set.cc
// (compiled as C++) export the unmangled C symbol names expected by both the
// C callers (openssl.c) and the Rust externs (system_certs.rs). In C the
// declaration is C linkage anyway; the guard matters for the C++ TUs.
#if defined(__cplusplus)
extern "C" {
#endif
OPENSSL_EXPORT X509_LAZY_CERT_SET *X509_LAZY_CERT_SET_new(const CRYPTO_BUFFER *const *buffers,
                                                           size_t num_buffers);
OPENSSL_EXPORT X509_LAZY_CERT_SET *X509_LAZY_CERT_SET_new_static(const uint8_t *const *certs,
                                                                  const size_t *lens, size_t num_certs);
OPENSSL_EXPORT void X509_LAZY_CERT_SET_free(X509_LAZY_CERT_SET *set);
OPENSSL_EXPORT int X509_LAZY_CERT_SET_get0_subject(const X509_LAZY_CERT_SET *set, size_t index,
                                                   const uint8_t **out_subject, size_t *out_subject_len);
OPENSSL_EXPORT int X509_STORE_add_lazy_cert_set(X509_STORE *store, X509_LAZY_CERT_SET *set);

OPENSSL_EXPORT size_t X509_LAZY_CERT_SET_num(const X509_LAZY_CERT_SET *set);
OPENSSL_EXPORT const CRYPTO_BUFFER *X509_LAZY_CERT_SET_get0_der(const X509_LAZY_CERT_SET *set, size_t index);
OPENSSL_EXPORT int X509_LAZY_CERT_SET_can_index(const uint8_t *der, size_t len);
#if defined(__cplusplus)
}  // extern "C"
#endif

#endif // US_BORINGSSL_LAZY_CERT_SHIM_H
