//! Which `cdn_url` values the server will act on (D8). mm-switch fetches a creative's `cdn_url`, and
//! mm-core hands it to clients, so a value that is not one of this deployment's own two forms is
//! never followed. The worker's database role holds `UPDATE (cdn_url)` (it has to: `publish` writes
//! it), so the check lives on the reading side, in this one function.
//!
//! The two forms, with `{id}` the creative's own canonical (lower-case, hyphenated) UUID:
//!
//! - a converted rendition: `http://mm-web/_mm/ads/r/{id}/{job8}-{kind}-{sha16}.{webm|mp4}`, as
//!   `store::rel_path` in mm-media-worker names it (`{job8}` and `{sha16}` lower-case hex, `{kind}` a
//!   `RenditionKind` name);
//! - a legacy upload that predates conversion and still serves from its own file:
//!   `http://mm-web/_mm/ads/{id}.webm`.
//!
//! Nothing else matches: not another scheme, host, port or userinfo, no query string, no fragment,
//! no `..` or other path segment, not another creative's file.

use uuid::Uuid;

const BASE: &str = "http://mm-web/_mm/ads/";

/// Which of the two forms a creative's `cdn_url` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalMedia {
    /// A converted rendition under `r/{id}/`.
    Rendition,
    /// The `{id}.webm` a creative was uploaded as before conversion existed.
    Legacy,
}

/// `Some` when `cdn_url` is exactly one of the creative's own two local forms (see the module doc).
pub fn classify_cdn_url(creative_id: &str, cdn_url: &str) -> Option<LocalMedia> {
    if !is_canonical_uuid(creative_id) {
        return None;
    }
    let rest = cdn_url.strip_prefix(BASE)?;
    if let Some(stem) = rest.strip_suffix(".webm")
        && stem == creative_id
    {
        return Some(LocalMedia::Legacy);
    }
    let (id, name) = rest.strip_prefix("r/")?.split_once('/')?;
    (id == creative_id && is_rendition_name(name)).then_some(LocalMedia::Rendition)
}

fn is_canonical_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok_and(|u| u.hyphenated().to_string() == s)
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `^[0-9a-f]{8}-[a-z0-9_]+-[0-9a-f]{16}\.(webm|mp4)$`
fn is_rendition_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".webm").or_else(|| name.strip_suffix(".mp4")) else {
        return false;
    };
    // 8 + '-' + at least one kind character + '-' + 16
    if !stem.is_ascii() || stem.len() < 8 + 1 + 1 + 1 + 16 {
        return false;
    }
    let (job8, rest) = stem.split_at(8);
    let (kind, sha16) = rest[1..].split_at(rest.len() - 1 - 17);
    rest.starts_with('-')
        && is_lower_hex(job8)
        && !kind.is_empty()
        && kind.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
        && sha16.starts_with('-')
        && is_lower_hex(&sha16[1..])
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "391aca35-fad3-4d39-a8f0-9dfaa53f6380";
    const OTHER: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";
    const NAME: &str = "01234567-webrtc_vp8-0123456789abcdef.webm";

    fn rendition(id: &str, name: &str) -> String {
        format!("http://mm-web/_mm/ads/r/{id}/{name}")
    }

    fn classify(url: &str) -> Option<LocalMedia> {
        classify_cdn_url(ID, url)
    }

    #[test]
    fn the_two_local_forms_are_accepted() {
        assert_eq!(classify(&rendition(ID, NAME)), Some(LocalMedia::Rendition));
        assert_eq!(classify(&rendition(ID, "abcdef01-hls_h264_720-fedcba9876543210.mp4")), Some(LocalMedia::Rendition));
        assert_eq!(classify(&format!("http://mm-web/_mm/ads/{ID}.webm")), Some(LocalMedia::Legacy));
    }

    /// Every name `store::rel_path` can produce is accepted. (The path is rebuilt here, not
    /// imported: mm-ads cannot depend on the worker.)
    #[test]
    fn every_name_the_worker_publishes_is_accepted() {
        use crate::conversion::RenditionKind;
        let (job, sha) = ("0123456789abcdef0123456789abcdef", "f".repeat(64));
        for kind in RenditionKind::ALL {
            let name = format!("{}-{}-{}.{}", &job[..8], kind.as_str(), &sha[..16], kind.ext());
            assert_eq!(classify(&rendition(ID, &name)), Some(LocalMedia::Rendition), "{name}");
        }
    }

    #[test]
    fn another_scheme_host_port_or_userinfo_is_refused() {
        let path = format!("/_mm/ads/r/{ID}/{NAME}");
        for base in [
            "https://mm-web", "ftp://mm-web", "file://mm-web", "HTTP://mm-web", "http://MM-WEB", "http://mm-web:8080",
            "http://mm-web.evil.example", "http://evil.example", "http://127.0.0.1", "http://localhost", "http://mm-web@evil.example",
            "http://evil.example#@mm-web", "//mm-web", "mm-web", "",
        ] {
            assert_eq!(classify(&format!("{base}{path}")), None, "{base}");
        }
        assert_eq!(classify("https://cdn.example.org/ad.webm"), None);
        assert_eq!(classify(&format!("http://mm-web/_mm/clips/r/{ID}/{NAME}")), None, "another path prefix");
        assert_eq!(classify(&format!("http://mm-web//_mm/ads/r/{ID}/{NAME}")), None);
    }

    #[test]
    fn dot_dot_a_query_or_a_fragment_is_refused() {
        for url in [
            format!("http://mm-web/_mm/ads/r/{ID}/../{OTHER}/{NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}/../../x.webm"),
            format!("http://mm-web/_mm/ads/../ads/r/{ID}/{NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}/..%2f{NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}?x=1"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}?"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}#t=1"),
            format!("http://mm-web/_mm/ads/{ID}.webm?x=1"),
            format!("http://mm-web/_mm/ads/{ID}.webm#x"),
            format!("http://mm-web/_mm/ads/../{ID}.webm"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}/"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}/extra"),
            format!("http://mm-web/_mm/ads/r/{ID}/x/{NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}//{NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}/ {NAME}"),
            format!("http://mm-web/_mm/ads/r/{ID}/{NAME}\n"),
        ] {
            assert_eq!(classify(&url), None, "{url:?}");
        }
    }

    #[test]
    fn a_wrong_uuid_is_refused() {
        // Another creative's file.
        assert_eq!(classify(&rendition(OTHER, NAME)), None);
        assert_eq!(classify(&format!("http://mm-web/_mm/ads/{OTHER}.webm")), None);
        // Not a canonical UUID at all.
        for bad in ["", "r", "391aca35fad34d39a8f09dfaa53f6380", &ID.to_uppercase(), &ID[1..], &format!("{ID}0"), "../etc"] {
            assert_eq!(classify(&rendition(bad, NAME)), None, "{bad}");
            assert_eq!(classify(&format!("http://mm-web/_mm/ads/{bad}.webm")), None, "{bad}");
        }
        // The creative itself must be a canonical UUID too: a fixture id like "ad-1" never matches.
        assert_eq!(classify_cdn_url("ad-1", &rendition("ad-1", NAME)), None);
        assert_eq!(classify_cdn_url(&ID.to_uppercase(), &rendition(&ID.to_uppercase(), NAME)), None);
    }

    #[test]
    fn a_wrong_extension_or_name_is_refused() {
        for name in [
            "01234567-webrtc_vp8-0123456789abcdef.mkv",
            "01234567-webrtc_vp8-0123456789abcdef.WEBM",
            "01234567-webrtc_vp8-0123456789abcdef.webm.exe",
            "01234567-webrtc_vp8-0123456789abcdef",
            "01234567-webrtc_vp8-0123456789abcdef.",
            ".webm",
            "01234567-webrtc_vp8-0123456789abcde.webm",   // 15 hex
            "01234567-webrtc_vp8-0123456789abcdef0.webm", // 17 hex
            "01234567-webrtc_vp8-0123456789ABCDEF.webm",
            "0123456-webrtc_vp8-0123456789abcdef.webm", // 7 hex
            "012345678-webrtc_vp8-0123456789abcdef.webm", // 9 hex
            "0123456g-webrtc_vp8-0123456789abcdef.webm",
            "01234567--0123456789abcdef.webm", // no kind
            "01234567-WEBRTC-0123456789abcdef.webm",
            "01234567-web.rtc-0123456789abcdef.webm",
            "01234567-web-rtc-0123456789abcdef.webm", // a hyphen in the kind
            "01234567_webrtc_vp8_0123456789abcdef.webm",
            "webrtc_vp8.webm",
            "ünïcödé-webrtc_vp8-0123456789abcdef.webm",
        ] {
            assert_eq!(classify(&rendition(ID, name)), None, "{name}");
        }
        // The legacy form is `.webm` only.
        assert_eq!(classify(&format!("http://mm-web/_mm/ads/{ID}.mp4")), None);
        assert_eq!(classify(&format!("http://mm-web/_mm/ads/{ID}")), None);
        assert_eq!(classify(&format!("http://mm-web/_mm/ads/{ID}.webm.webm")), None);
    }
}
