//! Bound encoder retention independently of the peer's advertised table size.

use http::HeaderMap;

pub(super) fn never_index(headers: &mut HeaderMap) {
    // h2 applies peer SETTINGS_HEADER_TABLE_SIZE to its encoder without a local
    // ceiling. Never-index literals keep arbitrary application fields out of
    // that table even after our pending-write reservations have been released.
    // Names, values and static Huffman compression remain unchanged.
    // https://httpwg.org/specs/rfc7541.html#never.indexed.literal
    for value in headers.values_mut() {
        value.set_sensitive(true);
    }
}
