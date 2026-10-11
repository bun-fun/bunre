/* HTTP header list shared by the HTTP/2 request decoder.
 *
 * This used to be `us_quic_header_t` from usockets' QUIC layer (lsquic), which
 * forced the HTTP/2 path to include `quic.h` for a plain name/value pair.
 * bunre ships no QUIC stack, so the struct moved here with the spelling.
 */
#ifndef US_HTTP_HEADERS_H
#define US_HTTP_HEADERS_H

/* One name/value pair pointing into caller-owned storage. `qpack_index` was an
 * outgoing QPACK hint; the HTTP/2 decode path always leaves it -1. */
struct us_http_header_t {
    const char *name;
    unsigned int name_len;
    const char *value;
    unsigned int value_len;
    int qpack_index;
};

#endif /* US_HTTP_HEADERS_H */