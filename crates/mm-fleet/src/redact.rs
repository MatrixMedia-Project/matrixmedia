//! Text that came back from a provider, made safe to store, return or log.
//!
//! A provider's error body is untrusted text, and some providers echo the request that
//! failed. The one request body that carries a secret is a test boot's cloud-init: it holds
//! the boot token (64 lowercase hex characters) the probe reports with. A provider that
//! echoes it in an error would otherwise carry the token into a request result, a status
//! row served to the dashboard, or a log line. Every place that turns provider text into
//! something the fleet keeps goes through [`provider_text`].

/// The longest provider text kept.
pub const MAX_PROVIDER_TEXT_CHARS: usize = 400;
/// The shortest run of lowercase hex characters treated as a secret: the length of a boot
/// token (and of its SHA-256 in hex).
const SECRET_HEX_RUN: usize = 64;
const REDACTED: &str = "[redacted]";

/// `text` with every run of lowercase hex characters at least 64 long replaced by
/// `[redacted]`, then cut to [`MAX_PROVIDER_TEXT_CHARS`] characters on a character boundary.
///
/// The redaction comes first. Cutting first could split a token and leave a fragment of it
/// that is too short to recognise. A whole maximal run is replaced, not a 64-character
/// window of it, so hex digits glued onto a token do not keep part of it. A legitimate 64-hex
/// value in provider text (a digest, say) reads `[redacted]` too, which costs only a less
/// specific error message.
pub fn provider_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_PROVIDER_TEXT_CHARS * 2));
    let mut run = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() || ('a'..='f').contains(&c) {
            run.push(c);
            continue;
        }
        flush_run(&mut run, &mut out);
        out.push(c);
    }
    flush_run(&mut run, &mut out);
    out.chars().take(MAX_PROVIDER_TEXT_CHARS).collect()
}

fn flush_run(run: &mut String, out: &mut String) {
    if run.len() >= SECRET_HEX_RUN {
        out.push_str(REDACTED);
    } else {
        out.push_str(run);
    }
    run.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn a_64_hex_run_is_replaced() {
        assert_eq!(
            provider_text(&format!("MM_REPORT_TOKEN={TOKEN}\nnext")),
            "MM_REPORT_TOKEN=[redacted]\nnext"
        );
        assert_eq!(provider_text(TOKEN), "[redacted]");
    }

    #[test]
    fn every_run_is_replaced_and_shorter_hex_is_kept() {
        let id = "0123456789abcdef"; // a 16-character id
        assert_eq!(
            provider_text(&format!("{TOKEN} and {TOKEN}, server {id}")),
            format!("[redacted] and [redacted], server {id}")
        );
        let almost = &TOKEN[..63];
        assert_eq!(
            provider_text(almost),
            almost,
            "63 characters is not a token"
        );
    }

    #[test]
    fn hex_digits_glued_onto_a_token_do_not_keep_part_of_it() {
        assert_eq!(provider_text(&format!("{TOKEN}cafe")), "[redacted]");
        assert_eq!(provider_text(&format!("beef{TOKEN}")), "[redacted]");
        // A letter outside a-f ends the run.
        assert_eq!(provider_text(&format!("x{TOKEN}z")), "x[redacted]z");
    }

    #[test]
    fn upper_case_hex_is_not_a_token() {
        let upper = TOKEN.to_uppercase();
        assert_eq!(provider_text(&upper), upper);
    }

    #[test]
    fn the_text_is_cut_to_400_characters_after_the_redaction() {
        // The token sits across the 400-character mark: cut first and 63 of its characters
        // would survive as an unrecognisable fragment.
        let text = format!("{}{TOKEN}tail", "x".repeat(380));
        let out = provider_text(&text);
        assert_eq!(out, format!("{}[redacted]tail", "x".repeat(380)));
        assert!(!out.contains(&TOKEN[..20]));
        let long = "y".repeat(1000);
        assert_eq!(
            provider_text(&long).chars().count(),
            MAX_PROVIDER_TEXT_CHARS
        );
    }

    #[test]
    fn the_cut_falls_on_a_character_boundary() {
        // Each "é" is two bytes: a byte cut at 400 would split one.
        let text = "é".repeat(500);
        let out = provider_text(&text);
        assert_eq!(out.chars().count(), MAX_PROVIDER_TEXT_CHARS);
        assert!(out.chars().all(|c| c == 'é'));
    }

    #[test]
    fn an_empty_text_stays_empty() {
        assert_eq!(provider_text(""), "");
    }
}
