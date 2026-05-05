//! NMEA 0183 checksum computation.
//!
//! NMEA 0183 sentences begin with `$`, end with `*XX\r\n` where `XX`
//! is two hex digits. The checksum is the XOR of all bytes between
//! the `$` and the `*`, exclusive. This module exposes the primitive;
//! all sentence formatters in this crate use it.

/// Compute the NMEA 0183 checksum byte for a sentence body.
///
/// `body` must NOT include the leading `$` or the trailing `*`.
#[must_use]
pub fn checksum(body: &str) -> u8 {
    let mut x: u8 = 0;
    for b in body.bytes() {
        x ^= b;
    }
    x
}

/// Format a complete sentence: `$<body>*XX\r\n`.
///
/// `body` is the talker + sentence type + comma-separated fields,
/// without the leading `$` or trailing `*XX\r\n`.
#[must_use]
pub fn format_sentence(body: &str) -> String {
    let cs = checksum(body);
    format!("${body}*{cs:02X}\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_matches_known_example() {
        // Classic NMEA documentation example:
        //   $GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47
        let body = "GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";
        assert_eq!(checksum(body), 0x47);
    }

    #[test]
    fn empty_body_has_zero_checksum() {
        assert_eq!(checksum(""), 0);
    }

    #[test]
    fn format_sentence_round_trips_to_canonical() {
        let s = format_sentence("GPGLL,4916.45,N,12311.12,W,225444,A,A");
        assert!(s.starts_with("$GPGLL,"));
        assert!(s.ends_with("\r\n"));
        // The body between $ and * should match what we passed in.
        let body = &s[1..s.len() - 5]; // strip $ ... *XX\r\n
        assert_eq!(body, "GPGLL,4916.45,N,12311.12,W,225444,A,A");
    }
}
