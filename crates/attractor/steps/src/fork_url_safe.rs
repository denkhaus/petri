//! Fork (denkhaus/work#1312 → denkhaus/work#1300, fix A): URL-safe
//! question ids.
//!
//! A question id travels a URL path segment twice: the questions API
//! lists a pending question by id, and the answer is posted to
//! `/runs/{id}/questions/{qid}/answer`. The historical composites — the
//! human gate's `<node>#<firing>` and the agent question's
//! `<node>#<firing>/agent/<session>/<call>/<index>` — carry `#` and `/`,
//! and the reverse proxy in front of the hosted app rejects the
//! percent-encoded forms of exactly those characters with a bare 400
//! (denkhaus/work#1300), making every call-scoped question unanswerable
//! through the proxy.
//!
//! This module owns the fork's replacement: ids are built from components
//! joined by `~` (URL-unreserved in practice — node names, session ids and
//! tool-call ids do not carry it), and any byte of a component outside
//! `[A-Za-z0-9._-]` is escaped as `~XX`, so the join is injective: two
//! distinct component tuples can never flatten to one id, which the
//! pending-answer routing (a map keyed by the id) relies on. The common
//! shapes stay readable — `gate~12`,
//! `planner~3~agent~ses_…~call_…~0` — and carry no character any proxy
//! rewrites or rejects.

/// A URL-safe question id: `parts` joined by `~`, each part escaped so it
/// cannot contain the join separator or any URL-unsafe byte.
pub fn id(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| escape(part))
        .collect::<Vec<_>>()
        .join("~")
}

/// Escape one component: URL-unreserved bytes pass through, everything
/// else (including `~` itself and the `%` escape character) becomes
/// `~XX` over the byte's UTF-8 representation.
fn escape(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    for &byte in part.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => {
                out.push(byte as char);
            }
            _ => {
                out.push('~');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0xF));
            }
        }
    }
    out
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every byte of an id this module builds is URL-unreserved: the id
    /// survives a path segment untouched, encoded or not.
    fn url_unreserved(byte: u8) -> bool {
        matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' | b'~')
    }

    #[test]
    fn keeps_the_common_shapes_readable() {
        let gate = id(&["gate", "12"]);
        assert_eq!(gate, "gate~12");
        let agent = id(&["planner", "3", "agent", "ses_7afa", "call_e2b3", "0"]);
        assert_eq!(agent, "planner~3~agent~ses_7afa~call_e2b3~0");
    }

    #[test]
    fn escapes_the_characters_the_proxy_rejects() {
        // The historical composites must never reappear in an id.
        assert_eq!(id(&["gate#3", "/agent/x"]), "gate~233~~2Fagent~2Fx");
        assert_eq!(id(&["a%b"]), "a~25b");
        assert_eq!(id(&["a~b"]), "a~7Eb");
    }

    #[test]
    fn escapes_non_ascii_bytes() {
        assert_eq!(id(&["türe"]), "t~C3~BCre");
    }

    #[test]
    fn every_output_byte_is_url_unreserved() {
        for probe in [
            id(&["gate", "12"]),
            id(&["planner", "3", "agent", "?", "call_x#y/z", "0"]),
            id(&["a b", "%", "~", "?", "#", "/"]),
            id(&["non-ascii-ü", "ø"]),
        ] {
            assert!(
                probe.as_bytes().iter().all(|&byte| url_unreserved(byte)),
                "id carries a URL-unsafe byte: {probe}"
            );
        }
    }

    #[test]
    fn distinct_component_tuples_never_flatten_to_one_id() {
        // A component containing the separator is escaped, so no split of
        // a different tuple can produce the same joined string.
        assert_eq!(id(&["a~b", "c"]), "a~7Eb~c");
        assert_ne!(id(&["a~b", "c"]), id(&["a", "b~c"]));
        assert_ne!(id(&["a~b", "c"]), id(&["a~b~c"]));
        assert_ne!(id(&["a#1", "2"]), id(&["a", "1~2"]));
        assert_ne!(id(&["x", "1", "agent"]), id(&["x~1", "agent"]));
    }
}
