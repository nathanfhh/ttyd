//! The embedded frontend bundle, extracted from `src/html.h` at build time.

use std::io::Read;
use std::sync::OnceLock;

include!(concat!(env!("OUT_DIR"), "/html_meta.rs"));

/// The gzip-compressed frontend, byte-identical to the blob the C build embeds.
pub const INDEX_HTML_GZIP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/index.html.gz"));

static PLAIN: OnceLock<Vec<u8>> = OnceLock::new();

/// The decompressed frontend, inflated once and cached, mirroring the C `html_cache`.
pub fn index_html_plain() -> &'static [u8] {
    PLAIN.get_or_init(|| {
        let mut out = Vec::with_capacity(INDEX_HTML_SIZE);
        let mut decoder = flate2::read::GzDecoder::new(INDEX_HTML_GZIP);
        decoder
            .read_to_end(&mut out)
            .expect("embedded index.html is not valid gzip");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_matches_declared_sizes() {
        assert_eq!(INDEX_HTML_GZIP.len(), INDEX_HTML_GZIP_LEN);
        assert_eq!(index_html_plain().len(), INDEX_HTML_SIZE);
    }

    #[test]
    fn decompressed_bundle_is_html() {
        let plain = index_html_plain();
        let head = String::from_utf8_lossy(&plain[..64.min(plain.len())]).to_lowercase();
        assert!(head.contains("<!doctype html"), "unexpected head: {head}");
    }
}
