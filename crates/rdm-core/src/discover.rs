//! Mirror discovery, turn a single user-provided URL (or a Metalink) into a set of mirror URLs that
//! all serve the SAME file, so the segmented engine can stripe across them. Deterministic, **no LLM**:
//! every backend is plain HTTP + parsing.
//!
//! Discovery only *proposes* candidates. The engine's [`crate::verify_sources`] gate still byte-checks
//! every discovered mirror against the anchor before it's striped, so a bogus or hostile discovered
//! mirror can't corrupt the download, discovery finds sources, verification disposes of bad ones.
//!
//! Backends here are the safest, standards-based tiers:
//!   - **Metalink files** (RFC 5854 `.meta4` v4 and the older v3 `.metalink`), a document listing
//!     mirrors + a whole-file checksum (+ size). Parsed leniently so both versions work.
//!   - **Metalink/HTTP** (RFC 6249), one GET whose `Link: <url>; rel=duplicate` headers enumerate
//!     mirrors (what Fedora's redirector and some CDNs serve).
//!
//! Riskier tiers are deferred: distro mirror-list adapters (Ubuntu/Debian/Arch),
//! repack **page-scrape** (opt-in, with SSRF / inert-probe guards), and BitTorrent. Each will be a new
//! function alongside these, feeding the same [`Discovered`] shape into the pool.

use reqwest::header::{CONTENT_LENGTH, LINK};
use reqwest::Client;

use crate::Err;

/// What a discovery backend found: mirror URLs (all serving one file) plus any integrity metadata.
#[derive(Debug, Default, Clone)]
pub struct Discovered {
    /// Mirror URLs to stripe across. The engine treats `sources[0]` as the anchor.
    pub sources: Vec<String>,
    /// Whole-file size, if the source advertised it.
    pub size: Option<u64>,
    /// Whole-file SHA-256 (lowercase hex), if the source carried it; lets the caller verify the
    /// finished download end-to-end.
    pub sha256: Option<String>,
    /// Human label of the backend that produced this (for logging).
    pub via: &'static str,
}

/// Try each deterministic discovery backend on `input`, returning the first that yields mirrors, or
/// `Ok(None)` if nothing applies (the caller then just downloads `input` directly). `input` may be a
/// Metalink file (URL or local path) or a plain download URL to probe for Metalink/HTTP headers.
pub async fn discover(client: &Client, input: &str) -> Result<Option<Discovered>, Err> {
    if is_metalink_ref(input) {
        if let Some(d) = metalink_file(client, input).await? {
            return Ok(Some(d));
        }
    }
    if is_http(input) {
        if let Some(d) = metalink_http(client, input).await? {
            return Ok(Some(d));
        }
    }
    Ok(None)
}

fn is_http(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// Does `input` name a Metalink document (by extension, ignoring any query/fragment)? Public so the
/// CLI can decide whether to auto-run discovery without duplicating the check.
pub fn is_metalink_ref(s: &str) -> bool {
    let path = s.split(['?', '#']).next().unwrap_or(s);
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".meta4") || lower.ends_with(".metalink")
}

/// Fetch (or read) a Metalink document and parse it.
async fn metalink_file(client: &Client, input: &str) -> Result<Option<Discovered>, Err> {
    let xml = if is_http(input) {
        client
            .get(input)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?
    } else {
        std::fs::read_to_string(input)?
    };
    Ok(parse_metalink(&xml))
}

/// Parse a Metalink document (v4 `.meta4` or v3 `.metalink`). Lenient by design: it collects every
/// `<url>` element's text as a mirror and picks up `<size>` and a SHA-256 `<hash>` wherever they
/// appear, so both schema versions (v4's `file/url` and v3's `file/resources/url` +
/// `verification/hash`) are handled by the same pass. Returns None if no mirror URLs are present.
pub fn parse_metalink(xml: &str) -> Option<Discovered> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let mut sources: Vec<String> = Vec::new();
    let mut size: Option<u64> = None;
    let mut sha256: Option<String> = None;
    for node in doc.descendants() {
        match node.tag_name().name() {
            "url" => {
                if let Some(t) = node.text().map(str::trim) {
                    if is_http(t) && !sources.iter().any(|u| u == t) {
                        sources.push(t.to_string());
                    }
                }
            }
            "size" if size.is_none() => {
                size = node.text().and_then(|t| t.trim().parse().ok());
            }
            "hash" if sha256.is_none() => {
                let ty = node
                    .attribute("type")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if ty.contains("sha-256") || ty.contains("sha256") {
                    sha256 = node
                        .text()
                        .map(|t| t.trim().to_ascii_lowercase())
                        .filter(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()));
                }
            }
            _ => {}
        }
    }
    if sources.is_empty() {
        return None;
    }
    Some(Discovered {
        sources,
        size,
        sha256,
        via: "metalink-file",
    })
}

/// Probe a plain URL for RFC 6249 Metalink/HTTP: a GET whose `Link: <mirror>; rel=duplicate` headers
/// enumerate equivalent mirrors. The original URL is kept as the anchor (sources[0]); mirrors are
/// appended. Returns None if the server advertises no duplicates. We issue a GET (some redirectors
/// only attach the headers on the resource response, not a HEAD) but never read the body; dropping
/// the response cancels the transfer.
async fn metalink_http(client: &Client, url: &str) -> Result<Option<Discovered>, Err> {
    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return Ok(None),
    };
    let mut sources = vec![url.to_string()];
    for hv in resp.headers().get_all(LINK) {
        if let Ok(s) = hv.to_str() {
            for l in parse_link_header(s) {
                if l.rel.eq_ignore_ascii_case("duplicate")
                    && is_http(&l.url)
                    && !sources.contains(&l.url)
                {
                    sources.push(l.url);
                }
            }
        }
    }
    if sources.len() <= 1 {
        return Ok(None); // no mirrors advertised, nothing gained over a plain download
    }
    let size = resp
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse().ok());
    Ok(Some(Discovered {
        sources,
        size,
        sha256: None,
        via: "metalink-http",
    }))
}

struct WebLink {
    url: String,
    rel: String,
}

/// Parse an RFC 8288 `Link` header value into its `<uri>; rel=…` entries (enough of the grammar for
/// RFC 6249 mirror lists): scan each `<…>` target and the `; key=value` params up to the next target.
fn parse_link_header(s: &str) -> Vec<WebLink> {
    let mut out = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i + 1;
        let mut j = start;
        while j < bytes.len() && bytes[j] != b'>' {
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let url = s[start..j].trim().to_string();
        // Params run from after '>' to the next '<' (start of the next link-value).
        let mut seg_end = j + 1;
        while seg_end < bytes.len() && bytes[seg_end] != b'<' {
            seg_end += 1;
        }
        let mut rel = String::new();
        for p in s[j + 1..seg_end].split(';') {
            let p = p.trim();
            if let Some(v) = p.strip_prefix("rel=") {
                rel = v.trim().trim_matches('"').to_string();
            }
        }
        out.push(WebLink { url, rel });
        i = seg_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metalink_v4() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <file name="ubuntu.iso">
    <size>3405469696</size>
    <hash type="sha-256">E907D92EEEC9DF64163A7E454CBC8D7755E8DDC7ED42F99DBC80C40F1A138433</hash>
    <url priority="1">https://mirror.init7.net/ubuntu/ubuntu.iso</url>
    <url priority="2">https://ftp.halifax.rwth-aachen.de/ubuntu/ubuntu.iso</url>
    <url priority="3">ftp://legacy.example.org/ubuntu.iso</url>
  </file>
</metalink>"#;
        let d = parse_metalink(xml).expect("should parse");
        assert_eq!(
            d.sources.len(),
            2,
            "only the two http(s) mirrors, ftp dropped"
        );
        assert_eq!(d.sources[0], "https://mirror.init7.net/ubuntu/ubuntu.iso");
        assert_eq!(d.size, Some(3405469696));
        assert_eq!(
            d.sha256.as_deref(),
            Some("e907d92eeec9df64163a7e454cbc8d7755e8ddc7ed42f99dbc80c40f1a138433"), // sha256 test vector
            "hash lowercased"
        );
    }

    #[test]
    fn parses_metalink_v3() {
        // Older Fedora-style v3 schema: resources/url + verification/hash.
        let xml = r#"<?xml version="1.0"?>
<metalink version="3.0" xmlns="http://www.metalinker.org/">
  <files>
    <file name="netinst.iso">
      <size>700000000</size>
      <verification>
        <hash type="sha256">aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899</hash>
      </verification>
      <resources>
        <url protocol="https" type="https" location="de">https://mirror.de/netinst.iso</url>
        <url protocol="https" type="https" location="fr">https://mirror.fr/netinst.iso</url>
      </resources>
    </file>
  </files>
</metalink>"#;
        let d = parse_metalink(xml).expect("should parse v3");
        assert_eq!(d.sources.len(), 2);
        assert_eq!(d.size, Some(700000000));
        assert_eq!(d.sha256.as_deref().unwrap().len(), 64);
    }

    #[test]
    fn rejects_metalink_without_urls() {
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x"><size>1</size></file></metalink>"#;
        assert!(parse_metalink(xml).is_none());
    }

    #[test]
    fn ignores_bogus_hash() {
        let xml = r#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="x">
          <hash type="sha-256">not-a-real-hash</hash>
          <url>https://m/x</url></file></metalink>"#;
        let d = parse_metalink(xml).unwrap();
        assert!(d.sha256.is_none(), "malformed hash is dropped, not trusted");
        assert_eq!(d.sources.len(), 1);
    }

    #[test]
    fn parses_rfc6249_link_header() {
        // As emitted by a Metalink/HTTP server: multiple comma-separated link-values.
        let h = r#"<https://m1.example/f.iso>; rel=duplicate; pri=1, <https://m2.example/f.iso>; rel="duplicate"; pri=2, <https://about.example/meta>; rel=describedby"#;
        let links = parse_link_header(h);
        let dups: Vec<&str> = links
            .iter()
            .filter(|l| l.rel.eq_ignore_ascii_case("duplicate"))
            .map(|l| l.url.as_str())
            .collect();
        assert_eq!(
            dups,
            vec!["https://m1.example/f.iso", "https://m2.example/f.iso"]
        );
    }
}
