// DOI lookup: Crossref for metadata, Unpaywall for an open-access PDF URL.
// See DESIGN.md Phase 8. This is the project's network trust boundary --
// remote JSON we don't control, and a URL from that JSON handed to a
// downloader -- so every function here treats its input as hostile:
// timeouts, response size caps, a URL scheme allowlist, and a magic-byte
// check before anything touches disk.
//
// Parsing is split from I/O on purpose (parse_crossref/parse_unpaywall take
// a &str, never make a request) so the JSON-shape logic is testable without
// the network. No test in this module may make a network request.

use std::time::Duration;

use std::net::{IpAddr, ToSocketAddrs};

use ureq::http::Uri;
use ureq::Agent;

use crate::models::{Author, Entry};

const CROSSREF_BASE: &str = "https://api.crossref.org/works";
const UNPAYWALL_BASE: &str = "https://api.unpaywall.org/v2";

// Plain and fixed for both APIs. Crossref doesn't need identifying info;
// Unpaywall gets the contact email as a query parameter instead, per its
// polite-pool policy -- never in the User-Agent, never hardcoded.
const USER_AGENT: &str = "ferref/0.1";

const JSON_TIMEOUT: Duration = Duration::from_secs(30);
const PDF_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_JSON_BYTES: u64 = 5 * 1024 * 1024;
const MAX_PDF_BYTES: u64 = 100 * 1024 * 1024;
// Landing pages are documents, not payloads; 5MB is already a very fat one.
const MAX_HTML_BYTES: u64 = 5 * 1024 * 1024;
// 10, not 5: an institutional SSO chain (publisher -> IdP -> proxy -> PDF) can
// legitimately be several hops, and 5 turned a paywalled Nature article into
// "too many redirects" rather than a useful answer. Every hop is revalidated
// (scheme + resolved IP), so the extra hops cost reach, not safety. Browsers
// allow ~20.
const MAX_REDIRECTS: usize = 10;

/// Fetches Crossref metadata for `doi` and maps it onto an `Entry`. The
/// returned entry has an empty `cite_key` -- deriving/choosing one is the
/// caller's job (see `cli::derive_cite_key` used by `add --doi`).
pub fn fetch_metadata(doi: &str) -> Result<Entry, String> {
    validate_doi(doi)?;
    let url = format!("{CROSSREF_BASE}/{}", percent_encode(doi));
    let body = get_json(&url, "Crossref")?;
    parse_crossref(&body)
}

/// Looks up an open-access PDF URL for `doi` via Unpaywall. `Ok(None)` means
/// Unpaywall has no legal OA copy on record -- a normal answer, not an error.
pub fn fetch_oa_pdf_url(doi: &str, email: &str) -> Result<OaStatus, String> {
    validate_doi(doi)?;
    let url = format!(
        "{UNPAYWALL_BASE}/{}?email={}",
        percent_encode(doi),
        percent_encode(email)
    );
    let body = get_json(&url, "Unpaywall")?;
    parse_unpaywall(&body)
}

/// Downloads the bytes at `url`, which must have come from a trusted call
/// site (Unpaywall JSON) and already passed the scheme check the caller is
/// expected to have done -- this function re-checks it anyway, since a URL
/// from a third-party API is hostile input regardless of who calls this.
/// Verifies the `%PDF` magic bytes before returning: Unpaywall's
/// `url_for_pdf` not infrequently lands on an HTML interstitial instead of
/// the actual paper.
pub fn download_pdf(url: &str) -> Result<Vec<u8>, String> {
    let (bytes, _final_url) = fetch_guarded(url, PDF_TIMEOUT, MAX_PDF_BYTES, "PDF download")?;

    if !has_pdf_magic(&bytes) {
        return Err(
            "downloaded content is not a PDF (missing %PDF magic bytes) -- \
             this is usually an HTML interstitial, not the paper"
                .to_string(),
        );
    }

    Ok(bytes)
}

// percent_encode deliberately leaves `/` literal, because a DOI's slashes are
// real path separators to Crossref. That makes `..` a path segment rather than
// text, so a DOI like "10.1/../../x" would walk Crossref's URL path. It stays
// on api.crossref.org, but a DOI has no business containing dot segments.
fn validate_doi(doi: &str) -> Result<(), String> {
    if doi.trim().is_empty() {
        return Err("DOI is empty".to_string());
    }
    if doi.split('/').any(|seg| seg == "." || seg == "..") {
        return Err(format!("refusing to look up DOI with path segments: {doi}"));
    }
    if !doi.starts_with("10.") {
        return Err(format!("'{doi}' is not a DOI (they all start with \"10.\")"));
    }
    Ok(())
}

fn has_pdf_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"%PDF")
}

/// Turns a cite_key into a safe filename component for `./pdfs/<key>.pdf`.
/// cite_key is user- and BibTeX-controlled and is about to be used as a
/// filesystem path, so anything outside `[A-Za-z0-9._-]` is replaced with
/// `_`, and a result that would resolve to nothing, `.`, or `..` is rejected
/// outright rather than silently writing somewhere unexpected.
pub fn sanitize_filename(cite_key: &str) -> Result<String, String> {
    let sanitized: String = cite_key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        return Err(format!(
            "cite_key '{cite_key}' cannot be turned into a safe filename"
        ));
    }
    Ok(sanitized)
}

// Shared GET+status-check+size-capped-read for both APIs. `http_status_as_error`
// is turned off so a 4xx/5xx comes back as Ok(response) instead of Err,
// letting the status be checked explicitly here -- ureq 3.4's own default is
// actually the opposite -- its `http_status_as_error` defaults to true -- but
// checking it
// ourselves either way is the point: a raw status dump is not an
// actionable error message.
/// Fetches a publisher landing page and reads its Highwire Press `citation_*`
/// meta tags. Same guarded fetch as everything else here, so redirects are
/// revalidated per hop and internal addresses are refused.
///
/// The request carries whatever network position the process has -- including
/// an `HTTPS_PROXY`, which `ureq` picks up from the environment. That is what
/// makes this useful on an institutional VPN and useless off it: nothing here
/// bypasses an access control, it just makes an ordinary request and reads what
/// comes back.
pub fn fetch_page_metadata(url: &str) -> Result<PageMetadata, String> {
    let (bytes, final_url) = fetch_guarded(url, JSON_TIMEOUT, MAX_HTML_BYTES, "landing page")?;
    // Lossy, not strict: a publisher page is a document to skim for six
    // attributes, not a protocol payload. Mis-declared encodings are common and
    // shouldn't cost the whole fetch.
    let html = String::from_utf8_lossy(&bytes);
    // The URL the page actually came from, not the one that was typed: a DOI
    // resolver, a www redirect, or an SSO proxy all land somewhere else, and a
    // relative citation_pdf_url has to resolve against where we ended up.
    let base = validate_url(&final_url)?;
    Ok(parse_citation_meta(&html, &base))
}

/// What a landing page told us about itself. Every field is optional: pages
/// vary, and a page that advertises only a DOI is still completely useful,
/// since the DOI is the good path.
#[derive(Debug, Default, PartialEq)]
pub struct PageMetadata {
    pub doi: Option<String>,
    pub pdf_url: Option<String>,
    pub title: Option<String>,
    pub authors: Vec<String>,
    pub journal: Option<String>,
    pub year: Option<i32>,
}

// Scans raw HTML for <meta name="citation_*" content="..."> without parsing the
// document. Crude on purpose, in the same spirit as strip_jats_tags: an HTML
// parser is a dependency bought to read six attributes off a well-established
// convention. The convention is Highwire Press's, which Google Scholar indexing
// depends on, so publishers emit it reliably.
//
// What this deliberately does NOT handle: tags inside comments or <script>
// strings, and any per-publisher DOM structure. A page that doesn't emit the
// tags is unsupported, not worked around.
fn parse_citation_meta(html: &str, base: &Uri) -> PageMetadata {
    let mut meta = PageMetadata::default();

    for attrs in meta_attributes(html) {
        let get = |want: &str| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(want))
                .map(|(_, v)| v.as_str())
        };
        let Some(name) = get("name").or_else(|| get("property")) else {
            continue;
        };
        let Some(content) = get("content") else {
            continue;
        };
        let content = unescape_html(content);
        if content.trim().is_empty() {
            continue;
        }

        // First tag of each kind wins: pages sometimes repeat a field, and the
        // first is the head-of-document one. citation_author is the exception --
        // it repeats *by design*, one per author, in order.
        match name.trim().to_ascii_lowercase().as_str() {
            "citation_doi" => set_once(&mut meta.doi, content),
            "citation_pdf_url" => set_once(&mut meta.pdf_url, content),
            "citation_title" => set_once(&mut meta.title, content),
            "citation_journal_title" => set_once(&mut meta.journal, content),
            "citation_author" => meta.authors.push(content),
            // Dates come as "2020", "2020/07/16", "2020-07-16". Only the year
            // is stored, so take the leading four digits and ignore the rest.
            "citation_publication_date" | "citation_date" | "citation_year" => {
                if meta.year.is_none() {
                    meta.year = content
                        .trim()
                        .get(..4)
                        .and_then(|y| y.parse::<i32>().ok())
                        .filter(|y| (1000..=9999).contains(y));
                }
            }
            _ => {}
        }
    }

    // citation_pdf_url is usually absolute but the spec doesn't require it.
    if let Some(pdf) = meta.pdf_url.take() {
        meta.pdf_url = resolve_location(base, &pdf).ok();
    }
    meta
}

fn set_once(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

// Returns each <meta> tag's attributes as (name, value) pairs.
//
// This tracks quotes, which the obvious version -- find `<meta`, slice to the
// next '>', then substring-search for `content=` -- does not, and all three of
// its failures were real:
//   * a decoy attribute whose *value* contained the text ` content=` was read
//     as the content attribute, letting a page choose which PDF got downloaded;
//   * a '>' inside a quoted value truncated the tag and silently dropped it;
//   * one unclosed `<meta` swallowed every following tag up to the next '>'
//     anywhere in the document.
//
// Byte indexing is safe here without char-boundary checks: the scanner only
// ever stops on ASCII delimiters, and every byte of a multi-byte UTF-8 sequence
// is >= 0x80, so it can never be mistaken for one.
fn meta_attributes(html: &str) -> Vec<Vec<(String, String)>> {
    let b = html.as_bytes();
    let mut tags = Vec::new();
    let mut i = 0;

    while i < b.len() {
        let Some(offset) = b[i..].iter().position(|&c| c == b'<') else {
            break;
        };
        let start = i + offset;
        // Resume inside the tag we just found, so a malformed one can't consume
        // the tags after it.
        i = start + 1;

        if b.len() - start < 5 || !b[start..start + 5].eq_ignore_ascii_case(b"<meta") {
            continue;
        }
        // "<metadata" must not match.
        if !matches!(b.get(start + 5), Some(c) if c.is_ascii_whitespace() || *c == b'/') {
            continue;
        }

        let mut p = start + 5;
        let mut attrs: Vec<(String, String)> = Vec::new();
        let mut closed = false;

        loop {
            while p < b.len() && b[p].is_ascii_whitespace() {
                p += 1;
            }
            match b.get(p) {
                None => break,
                Some(b'>') => {
                    closed = true;
                    p += 1;
                    break;
                }
                Some(b'/') => {
                    p += 1;
                    continue;
                }
                // A '<' cannot begin an attribute name, so the tag we're in was
                // never closed. Abandon it rather than reading the next tag as
                // this one's attributes -- the outer loop resumes at this '<'.
                Some(b'<') => break,
                _ => {}
            }

            let name_start = p;
            while p < b.len()
                && !b[p].is_ascii_whitespace()
                && b[p] != b'='
                && b[p] != b'>'
                && b[p] != b'/'
            {
                p += 1;
            }
            let name = &html[name_start..p];

            let before_eq = p;
            while p < b.len() && b[p].is_ascii_whitespace() {
                p += 1;
            }
            if b.get(p) != Some(&b'=') {
                // Valueless attribute; rewind so the next round sees what follows.
                attrs.push((name.to_string(), String::new()));
                p = before_eq;
                continue;
            }
            p += 1;
            while p < b.len() && b[p].is_ascii_whitespace() {
                p += 1;
            }

            let value = match b.get(p) {
                None => break,
                Some(&q @ (b'"' | b'\'')) => {
                    p += 1;
                    let value_start = p;
                    // Stop at '<' as well as the closing quote: no citation_*
                    // value contains one, so hitting it means the quote was
                    // never closed and we are about to eat the next tag.
                    while p < b.len() && b[p] != q && b[p] != b'<' {
                        p += 1;
                    }
                    if p >= b.len() || b[p] == b'<' {
                        break; // unterminated quote: abandon this tag
                    }
                    let v = &html[value_start..p];
                    p += 1;
                    v
                }
                Some(_) => {
                    let value_start = p;
                    while p < b.len() && !b[p].is_ascii_whitespace() && b[p] != b'>' {
                        p += 1;
                    }
                    &html[value_start..p]
                }
            };
            attrs.push((name.to_string(), value.to_string()));
        }

        if closed && !attrs.is_empty() {
            tags.push(attrs);
            i = p;
        }
    }
    tags
}

// The five predefined XML entities plus numeric escapes, which is what actually
// turns up in a content attribute. Anything else is left as written -- a stray
// "&copy;" in a title is cosmetic, and a full entity table is a dependency.
fn unescape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let replacement = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            _ => entity
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match replacement {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn get_json(url: &str, service: &str) -> Result<String, String> {
    let (bytes, _final_url) = fetch_guarded(url, JSON_TIMEOUT, MAX_JSON_BYTES, service)?;
    String::from_utf8(bytes).map_err(|_| format!("{service} returned invalid UTF-8"))
}

// The one place an HTTP request is made. Redirects are followed BY HAND, one
// hop at a time, revalidating the target every time.
//
// This is the fix for the phase's worst defect: checking only the URL we were
// handed is not enough, because ureq follows up to 10 redirects on its own and
// the URL comes from Unpaywall -- a third party. A redirect to
// http://127.0.0.1/ or http://169.254.169.254/ (cloud metadata) would
// otherwise be fetched, written into ./pdfs/, and attached to the library, and
// anything starting with %PDF would sail through the magic-byte check.
//
// Known limitation: the address check resolves the host, then ureq resolves it
// again when it connects, so a DNS entry that changes between the two (a
// rebinding attack) can still slip past. Closing that needs a resolver we
// control, i.e. a dependency; the check below stops the realistic case.
// Returns the bytes and the URL they actually came from -- which is not the URL
// passed in whenever a redirect was followed. Callers that resolve relative
// links against the page (parse_citation_meta) need the final one, or a
// publisher reached via a cross-host redirect resolves its citation_pdf_url
// against the wrong host.
fn fetch_guarded(
    url: &str,
    timeout: Duration,
    limit: u64,
    what: &str,
) -> Result<(Vec<u8>, String), String> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        // We follow redirects ourselves so each hop can be revalidated.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .build()
        .into();

    let mut current = url.to_string();

    for _ in 0..=MAX_REDIRECTS {
        let uri = validate_url(&current)?;

        let resp = agent
            .get(&current)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| format!("failed to reach {what}: {e}"))?;

        let status = resp.status();

        if status.is_redirection() {
            let location = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{what} sent a redirect with no Location header"))?
                .to_string();
            current = resolve_location(&uri, &location)?;
            continue;
        }

        match status.as_u16() {
            404 => return Err(format!("{what} has no record for this DOI (404)")),
            429 => return Err(format!("{what} rate limit exceeded (429); try again later")),
            _ if !status.is_success() => return Err(format!("{what} returned HTTP {status}")),
            _ => {}
        }

        let body = resp
            .into_body()
            .with_config()
            .limit(limit + 1)
            .read_to_vec()
            .map_err(|e| {
                format!("failed reading {what} response (over the {limit}-byte cap): {e}")
            })?;
        return Ok((body, current));
    }

    Err(format!("{what}: too many redirects (limit {MAX_REDIRECTS})"))
}

// Accepts only http(s) URLs whose host resolves entirely to public addresses.
fn validate_url(url: &str) -> Result<Uri, String> {
    let uri: Uri = url
        .parse()
        .map_err(|_| format!("refusing to fetch malformed URL: {url}"))?;

    let scheme = uri.scheme_str().unwrap_or("");
    if scheme != "http" && scheme != "https" {
        return Err(format!("refusing to fetch non-http(s) URL: {url}"));
    }

    let host = uri
        .host()
        .ok_or_else(|| format!("refusing to fetch URL with no host: {url}"))?;
    let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });

    // A host with no resolvable address is a hard error rather than a pass:
    // "can't tell" must not mean "allow".
    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("could not resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("could not resolve {host}"));
    }
    for addr in addrs {
        if is_internal(addr.ip()) {
            return Err(format!(
                "refusing to fetch {url}: {host} resolves to the internal address {}",
                addr.ip()
            ));
        }
    }

    Ok(uri)
}

// Loopback, private, link-local (which covers cloud metadata at
// 169.254.169.254), and the various reserved ranges.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || v4.octets()[0] >= 240
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80 // link local fe80::/10
        }
    }
}

// Location may be absolute or relative; resolve it against the hop we were on.
fn resolve_location(base: &Uri, location: &str) -> Result<String, String> {
    let scheme = base.scheme_str().unwrap_or("https");

    // Schemes are case-insensitive per RFC 3986, and matching only lowercase
    // turned "HTTPS://host/x" into a *relative* path glued onto the base host --
    // silently going somewhere other than where the server said. (Harmless for
    // safety, since the result is revalidated either way, but it breaks the
    // redirect.)
    let lower = location.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(location.to_string());
    }

    let authority = base
        .authority()
        .ok_or_else(|| "redirect target has no host".to_string())?;

    // Protocol-relative ("//host/path") is a different host, not a path on this
    // one. Institutional proxy and CDN rewrites use it.
    if let Some(rest) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{rest}"));
    }
    if location.starts_with('/') {
        Ok(format!("{scheme}://{authority}{location}"))
    } else {
        Ok(format!("{scheme}://{authority}/{location}"))
    }
}


// Minimal RFC 3986 percent-encoding for a DOI or email dropped into a URL
// path/query. `/` is left literal: a DOI's prefix/suffix separator, which
// both Crossref and Unpaywall expect unescaped in the path.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// Crossref vocabulary -> our BibTeX-ish entry_type. Anything unlisted maps
// to "misc" rather than being passed through raw -- Crossref's type list is
// larger than BibTeX's and growing.
fn map_entry_type(crossref_type: &str) -> String {
    match crossref_type {
        "journal-article" => "article",
        "proceedings-article" => "inproceedings",
        "book-chapter" => "incollection",
        "book" => "book",
        "posted-content" => "misc",
        _ => "misc",
    }
    .to_string()
}

// Crossref's `abstract` field, when present, is JATS XML (e.g.
// `<jats:p>...</jats:p>`), not plain prose. Rather than store raw markup
// mislabeled as text, tags are crudely stripped: everything between `<` and
// `>` is dropped. This is not a general XML/HTML parser -- it doesn't handle
// entities, CDATA, or malformed markup -- but it's enough for the handful of
// wrapper tags (`<jats:p>`, `<jats:italic>`, ...) Crossref abstracts use.
fn strip_jats_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut in_tag = false;
    for c in xml.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Parses a Crossref `/works/{doi}` JSON body into an `Entry`. Never panics
/// on a malformed/partial response -- every field is optional here even
/// where Crossref's schema says it shouldn't be, because this is remote
/// input we don't control.
fn parse_crossref(json: &str) -> Result<Entry, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid Crossref JSON: {e}"))?;
    let message = v
        .get("message")
        .ok_or_else(|| "Crossref response missing 'message'".to_string())?;

    let doi = message.get("DOI").and_then(|d| d.as_str()).map(str::to_string);

    // title/container-title are arrays; take the first element, tolerating
    // an empty array or a missing field entirely.
    let title = message
        .get("title")
        .and_then(|t| t.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| t.as_str())
        .unwrap_or("Untitled")
        .to_string();

    let journal = message
        .get("container-title")
        .and_then(|t| t.as_array())
        .and_then(|arr| arr.first())
        .and_then(|t| t.as_str())
        .map(str::to_string);

    let volume = message.get("volume").and_then(|v| v.as_str()).map(str::to_string);
    // "page", not "pages", in Crossref's schema.
    let pages = message.get("page").and_then(|v| v.as_str()).map(str::to_string);

    let entry_type = map_entry_type(message.get("type").and_then(|t| t.as_str()).unwrap_or(""));

    // year is issued.date-parts[0][0]; date-parts can be year-only
    // ([[2013]]) and its entries can in principle be null, so this never
    // indexes/unwraps blindly.
    let year = message
        .get("issued")
        .and_then(|i| i.get("date-parts"))
        .and_then(|dp| dp.as_array())
        .and_then(|outer| outer.first())
        .and_then(|inner| inner.as_array())
        .and_then(|inner| inner.first())
        .and_then(|y| y.as_i64())
        // i32, not `as i32`: a year outside i32 range is remote garbage,
        // and a truncating cast turns 99999999999999 into a plausible-looking
        // 276447231 instead of leaving the field unset.
        .and_then(|y| i32::try_from(y).ok());

    let authors = message
        .get("author")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| match a.get("family").and_then(|f| f.as_str()) {
                    Some(family) => {
                        let given = a.get("given").and_then(|g| g.as_str()).map(str::to_string);
                        Some(Author::new(family.to_string(), given))
                    }
                    // Organizational authors carry "name" instead of
                    // family/given. Skipped (not unwrapped) if neither is
                    // present.
                    None => a
                        .get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| Author::new(n.to_string(), None)),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let abstract_text = message
        .get("abstract")
        .and_then(|a| a.as_str())
        .map(strip_jats_tags);

    let mut entry = Entry::new(entry_type, String::new(), title);
    entry.doi = doi;
    entry.journal = journal;
    entry.volume = volume;
    entry.pages = pages;
    entry.year = year;
    entry.abstract_text = abstract_text;
    for author in authors {
        entry.add_author(author);
    }
    Ok(entry)
}

/// Parses an Unpaywall response body, returning the OA PDF URL if one
/// exists. `best_oa_location` (or its `url_for_pdf`) being `null` means no
/// legal OA copy exists -- `Ok(None)`, not an error.
/// What Unpaywall knows about a DOI. `is_oa` without a `pdf_url` is common --
/// plenty of genuinely open papers are only linked as landing pages -- and the
/// two cases deserve different messages, so they're kept apart here.
pub struct OaStatus {
    pub is_oa: bool,
    pub pdf_url: Option<String>,
}

fn parse_unpaywall(json: &str) -> Result<OaStatus, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid Unpaywall JSON: {e}"))?;

    let is_oa = v.get("is_oa").and_then(|b| b.as_bool()).unwrap_or(false);

    let pdf_of = |loc: &serde_json::Value| {
        loc.get("url_for_pdf")
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    };

    // Only best_oa_location, deliberately. Scanning the other oa_locations for
    // a url_for_pdf was tried and reverted: on live data the extra candidates
    // it turned up were landing pages, so it converted a clean "no PDF
    // available" into a "downloaded content is not a PDF" failure without
    // fetching anything new.
    let pdf_url = v
        .get("best_oa_location")
        .filter(|loc| !loc.is_null())
        .and_then(pdf_of);

    Ok(OaStatus { is_oa, pdf_url })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed from the real, live response body for
    // GET https://api.crossref.org/works/10.1038/nature12373 (captured
    // 2026-08-22) -- fields not exercised by parse_crossref are dropped, but
    // every field it reads is a verbatim value from that response.
    const NATURE12373: &str = r#"
    {
      "status": "ok",
      "message-type": "work",
      "message-version": "1.0.0",
      "message": {
        "DOI": "10.1038/nature12373",
        "type": "journal-article",
        "title": ["Nanometre-scale thermometry in a living cell"],
        "container-title": ["Nature"],
        "volume": "500",
        "page": "54-58",
        "issued": { "date-parts": [[2013, 7, 31]] },
        "author": [
          { "given": "G.", "family": "Kucsko", "sequence": "first" },
          { "given": "P. C.", "family": "Maurer", "sequence": "additional" },
          { "given": "N. Y.", "family": "Yao", "sequence": "additional" }
        ]
      }
    }
    "#;

    #[test]
    fn parses_full_crossref_response() {
        let entry = parse_crossref(NATURE12373).unwrap();
        assert_eq!(entry.doi.as_deref(), Some("10.1038/nature12373"));
        assert_eq!(entry.entry_type, "article");
        assert_eq!(entry.title, "Nanometre-scale thermometry in a living cell");
        assert_eq!(entry.journal.as_deref(), Some("Nature"));
        assert_eq!(entry.volume.as_deref(), Some("500"));
        assert_eq!(entry.pages.as_deref(), Some("54-58"));
        assert_eq!(entry.year, Some(2013));
        assert_eq!(entry.authors.len(), 3);
        assert_eq!(entry.authors[0].last_name, "Kucsko");
        assert_eq!(entry.authors[0].first_name.as_deref(), Some("G."));
        assert!(entry.abstract_text.is_none());
    }

    // Real shape from GET https://api.crossref.org/works/10.7717/peerj.4375
    // (captured 2026-08-22): the abstract is JATS XML.
    #[test]
    fn strips_jats_xml_abstract() {
        let json = r#"
        {
          "message": {
            "DOI": "10.7717/peerj.4375",
            "type": "journal-article",
            "title": ["The state of OA: a large-scale analysis"],
            "abstract": "<jats:p>Despite growing interest in Open Access <jats:italic>(OA)</jats:italic>, there is an unmet need.</jats:p>"
          }
        }
        "#;
        let entry = parse_crossref(json).unwrap();
        assert_eq!(
            entry.abstract_text.as_deref(),
            Some("Despite growing interest in Open Access (OA), there is an unmet need.")
        );
    }

    // date-parts can be year-only: [[2013]].
    #[test]
    fn handles_year_only_date_parts() {
        let json = r#"
        {
          "message": {
            "DOI": "10.9999/example",
            "type": "book",
            "title": ["A Book"],
            "issued": { "date-parts": [[2013]] }
          }
        }
        "#;
        let entry = parse_crossref(json).unwrap();
        assert_eq!(entry.year, Some(2013));
        assert_eq!(entry.entry_type, "book");
    }

    // A malformed/absent date must never panic.
    #[test]
    fn malformed_date_parts_do_not_panic() {
        for issued in [
            r#""issued": { "date-parts": [[]] },"#,
            r#""issued": { "date-parts": [] },"#,
            r#""issued": { "date-parts": [[null]] },"#,
            "",
        ] {
            let json = format!(
                r#"{{ "message": {{ "DOI": "10.1/x", "type": "misc", "title": ["T"], {issued} "container-title": [] }} }}"#
            );
            let entry = parse_crossref(&json).unwrap();
            assert_eq!(entry.year, None);
        }
    }

    // A missing title falls back to "Untitled" rather than panicking or
    // leaving an empty string.
    #[test]
    fn missing_title_falls_back() {
        let json = r#"{ "message": { "DOI": "10.1/x", "type": "journal-article" } }"#;
        let entry = parse_crossref(json).unwrap();
        assert_eq!(entry.title, "Untitled");
        assert!(entry.authors.is_empty());
    }

    // Organizational authors carry "name" instead of "family"/"given".
    #[test]
    fn organizational_author_uses_name_field() {
        let json = r#"
        {
          "message": {
            "DOI": "10.1/x",
            "type": "report",
            "title": ["A Report"],
            "author": [
              { "name": "World Health Organization", "sequence": "first" },
              { "given": "Jane", "family": "Smith", "sequence": "additional" }
            ]
          }
        }
        "#;
        let entry = parse_crossref(json).unwrap();
        assert_eq!(entry.entry_type, "misc"); // "report" isn't in the map
        assert_eq!(entry.authors.len(), 2);
        assert_eq!(entry.authors[0].last_name, "World Health Organization");
        assert_eq!(entry.authors[0].first_name, None);
        assert_eq!(entry.authors[1].last_name, "Smith");
    }

    #[test]
    fn crossref_type_mapping() {
        assert_eq!(map_entry_type("journal-article"), "article");
        assert_eq!(map_entry_type("proceedings-article"), "inproceedings");
        assert_eq!(map_entry_type("book-chapter"), "incollection");
        assert_eq!(map_entry_type("book"), "book");
        assert_eq!(map_entry_type("posted-content"), "misc");
        assert_eq!(map_entry_type("dataset"), "misc");
    }

    #[test]
    fn rejects_response_without_message() {
        assert!(parse_crossref(r#"{"status": "ok"}"#).is_err());
        assert!(parse_crossref("not json").is_err());
    }

    // Real shape (fields trimmed) from Unpaywall's documented API response,
    // https://unpaywall.org/products/api -- a PDF URL under best_oa_location.
    #[test]
    fn parses_unpaywall_response_with_pdf() {
        let json = r#"
        {
          "doi": "10.1371/journal.pone.0000308",
          "is_oa": true,
          "best_oa_location": {
            "url_for_pdf": "https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000308&type=printable",
            "host_type": "publisher",
            "license": "cc-by"
          }
        }
        "#;
        assert_eq!(
            parse_unpaywall(json).unwrap().pdf_url,
            Some(
                "https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000308&type=printable"
                    .to_string()
            )
        );
    }

    // No legal OA copy: best_oa_location is null. This is Ok(None), not an
    // error.
    #[test]
    fn parses_unpaywall_response_with_no_oa_copy() {
        let json = r#"{ "doi": "10.1/paywalled", "is_oa": false, "best_oa_location": null }"#;
        assert_eq!(parse_unpaywall(json).unwrap().pdf_url, None);
    }

    // url_for_pdf itself can be null even when best_oa_location isn't.
    #[test]
    fn parses_unpaywall_response_with_null_pdf_url() {
        let json = r#"
        { "best_oa_location": { "url_for_pdf": null, "host_type": "repository" } }
        "#;
        assert_eq!(parse_unpaywall(json).unwrap().pdf_url, None);
    }

    fn meta_of(html: &str) -> PageMetadata {
        let base: Uri = "https://example.org/articles/1".parse().unwrap();
        parse_citation_meta(html, &base)
    }

    // The tag shapes publishers actually emit: attribute order varies, quoting
    // varies, citation_author repeats, and content is HTML-escaped.
    #[test]
    fn citation_meta_survives_real_world_tag_shapes() {
        let html = r#"
            <html><head>
            <meta name="citation_title" content="Entropy &amp; Information">
            <meta content='Zhou, Yi' name='citation_author'>
            <meta name=citation_author content="Smith, John">
            <meta name="citation_journal_title" content="Physical Review">
            <meta name="citation_publication_date" content="1957/05/15">
            <meta name="citation_doi" content="10.1103/PhysRev.106.620">
            <meta property="citation_pdf_url" content="/pdf/106-620.pdf" />
            <meta name="viewport" content="width=device-width">
            <metadata name="citation_title" content="NOT A META TAG">
            </head></html>"#;

        let m = meta_of(html);
        assert_eq!(m.title.as_deref(), Some("Entropy & Information"));
        assert_eq!(m.authors, vec!["Zhou, Yi", "Smith, John"]);
        assert_eq!(m.journal.as_deref(), Some("Physical Review"));
        assert_eq!(m.year, Some(1957));
        assert_eq!(m.doi.as_deref(), Some("10.1103/PhysRev.106.620"));
        // Relative citation_pdf_url is resolved against the page.
        assert_eq!(m.pdf_url.as_deref(), Some("https://example.org/pdf/106-620.pdf"));
    }

    // A page with none of the tags must come back empty rather than
    // half-populated with garbage -- that's what makes the caller's "this page
    // isn't supported" message correct.
    #[test]
    fn a_page_without_citation_tags_yields_nothing() {
        assert_eq!(meta_of("<html><body>no meta here</body></html>"), PageMetadata::default());
        // `data-content` must not be read as `content`.
        let m = meta_of(r#"<meta name="citation_title" data-content="wrong" content="right">"#);
        assert_eq!(m.title.as_deref(), Some("right"));
    }

    // A landing page is untrusted input, and the scanner that reads it decides
    // which PDF gets downloaded. Each of these was a real defect in the version
    // that sliced to the next '>' and substring-searched for the attribute.
    #[test]
    fn the_meta_scanner_tracks_quotes() {
        // A decoy attribute whose VALUE contains " content=" must not be read
        // as the content attribute -- that let a page choose the download URL.
        let hijack = r#"<meta name="citation_pdf_url" data-note="see content=http://evil.example/x.pdf ok" content="https://good.example/real.pdf">"#;
        assert_eq!(
            meta_of(hijack).pdf_url.as_deref(),
            Some("https://good.example/real.pdf")
        );

        // '>' inside a quoted value is content, not the end of the tag.
        let gt = r#"<meta name="citation_title" content="A > B">"#;
        assert_eq!(meta_of(gt).title.as_deref(), Some("A > B"));

        // An unclosed <meta must not swallow the tags after it.
        let unclosed = "<meta name=\"citation_pdf_url\" content=\"decoy.pdf\"\n\
                        <meta name=\"citation_doi\" content=\"10.1/found\">";
        assert_eq!(meta_of(unclosed).doi.as_deref(), Some("10.1/found"));

        // An unterminated quote abandons its own tag and nothing else.
        let unterminated = "<meta name=\"citation_title\" content=\"never closed\n\
                            <meta name=\"citation_doi\" content=\"10.2/ok\">";
        assert_eq!(meta_of(unterminated).doi.as_deref(), Some("10.2/ok"));
    }

    // A redirect's Location may be protocol-relative or use a shouted scheme;
    // both are absolute, and treating either as a relative path sends the next
    // hop to the wrong host.
    #[test]
    fn resolve_location_treats_absolute_forms_as_absolute() {
        let base: Uri = "https://good.example/a/b".parse().unwrap();
        assert_eq!(
            resolve_location(&base, "//cdn.example/x").unwrap(),
            "https://cdn.example/x"
        );
        assert_eq!(
            resolve_location(&base, "HTTPS://other.example/x").unwrap(),
            "HTTPS://other.example/x"
        );
        assert_eq!(
            resolve_location(&base, "/rooted").unwrap(),
            "https://good.example/rooted"
        );
        assert_eq!(
            resolve_location(&base, "relative").unwrap(),
            "https://good.example/relative"
        );
    }

    #[test]
    fn html_entities_in_content_are_unescaped() {
        assert_eq!(unescape_html("a &amp; b"), "a & b");
        assert_eq!(unescape_html("&lt;i&gt;x&lt;/i&gt;"), "<i>x</i>");
        assert_eq!(unescape_html("Don&#39;t &#x2014; stop"), "Don't \u{2014} stop");
        // Unknown and malformed entities are left exactly as written.
        assert_eq!(unescape_html("100&nbsp;% &amp"), "100&nbsp;% &amp");
    }

    #[test]
    fn sanitize_filename_neutralizes_path_traversal() {
        for bad in ["../../etc/passwd", "a/b", "..", ".", ""] {
            let result = sanitize_filename(bad);
            if let Ok(name) = &result {
                // Even when accepted, the result must not contain a path
                // separator or resolve outside pdfs/.
                assert!(!name.contains('/'), "{bad:?} -> {name:?} contains '/'");
                let joined = std::path::Path::new("pdfs").join(name);
                assert!(
                    joined.starts_with("pdfs"),
                    "{bad:?} escaped the pdfs/ directory: {joined:?}"
                );
            }
        }
        // These specific inputs must be rejected outright, not merely
        // neutralized.
        assert!(sanitize_filename("..").is_err());
        assert!(sanitize_filename(".").is_err());
        assert!(sanitize_filename("").is_err());
    }

    #[test]
    fn sanitize_filename_keeps_safe_characters() {
        assert_eq!(sanitize_filename("kucsko2013").unwrap(), "kucsko2013");
        assert_eq!(sanitize_filename("smith-2024.v2").unwrap(), "smith-2024.v2");
    }

    // Rejected before any network call is attempted -- a non-http(s) scheme
    // (file://, javascript:, a bare path) from a third-party API is hostile
    // input and must never reach an HTTP client.
    #[test]
    fn download_pdf_rejects_non_http_schemes() {
        for bad in ["file:///etc/passwd", "ftp://example.com/x.pdf", "javascript:alert(1)"] {
            assert!(download_pdf(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }

    #[test]
    fn pdf_magic_bytes_check() {
        assert!(has_pdf_magic(b"%PDF-1.4\n..."));
        assert!(!has_pdf_magic(b"<html><body>not a pdf</body></html>"));
        assert!(!has_pdf_magic(b""));
    }

    #[test]
    fn percent_encode_leaves_doi_slash_literal_and_escapes_special_chars() {
        assert_eq!(percent_encode("10.1038/nature12373"), "10.1038/nature12373");
        assert_eq!(percent_encode("a@b.com"), "a%40b.com");
        assert_eq!(percent_encode("10.1/has space"), "10.1/has%20space");
    }

    // Regression: the scheme check used to run only on the URL we were handed,
    // while ureq followed up to 10 redirects on its own, so an Unpaywall URL
    // could redirect us onto loopback or the cloud metadata address.
    #[test]
    fn internal_addresses_are_recognised() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254", // AWS/GCP metadata
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1", // v4-mapped loopback
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip} should be internal");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700:4700::1111"] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip} should be public");
        }
    }

    // Literal IPs so this needs no DNS and therefore no network.
    #[test]
    fn validate_url_rejects_bad_schemes_and_internal_hosts() {
        assert!(validate_url("file:///etc/passwd").is_err());
        assert!(validate_url("ftp://example.com/x").is_err());
        assert!(validate_url("javascript:alert(1)").is_err());
        assert!(validate_url("http://127.0.0.1:8080/x").is_err());
        assert!(validate_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_url("https://[::1]/x").is_err());
        assert!(validate_url("not a url").is_err());
    }

    #[test]
    fn resolve_location_handles_absolute_and_relative() {
        let base: Uri = "https://host.example/a/b".parse().unwrap();
        assert_eq!(
            resolve_location(&base, "https://other.example/x").unwrap(),
            "https://other.example/x"
        );
        assert_eq!(
            resolve_location(&base, "/root").unwrap(),
            "https://host.example/root"
        );
        assert_eq!(
            resolve_location(&base, "rel").unwrap(),
            "https://host.example/rel"
        );
    }

    #[test]
    fn validate_doi_rejects_path_segments_and_non_dois() {
        assert!(validate_doi("10.1038/nature12373").is_ok());
        assert!(validate_doi("10.1/../../etc/passwd").is_err());
        assert!(validate_doi("10.1/./x").is_err());
        assert!(validate_doi("not-a-doi").is_err());
        assert!(validate_doi("").is_err());
    }

    // Regression: `as i32` turned a 14-digit year into a plausible 276447231.
    #[test]
    fn out_of_range_year_is_dropped_not_truncated() {
        let json = r#"{"message":{"type":"journal-article","title":["T"],
            "issued":{"date-parts":[[99999999999999]]}}}"#;
        assert_eq!(parse_crossref(json).unwrap().year, None);
    }

    // Real shape from 10.7717/peerj.4375: genuinely open access, but every
    // location is a landing page. Reporting that as "not open access" sends
    // the user looking for the wrong thing.
    #[test]
    fn open_access_without_a_pdf_link_is_distinguishable() {
        let json = r#"{
            "is_oa": true,
            "best_oa_location": {"url": "https://doi.org/10.7717/peerj.4375",
                                 "url_for_pdf": null, "host_type": "publisher"},
            "oa_locations": [
                {"url_for_pdf": null, "host_type": "publisher"},
                {"url_for_pdf": null, "host_type": "repository"}
            ]
        }"#;
        let oa = parse_unpaywall(json).unwrap();
        assert!(oa.is_oa);
        assert_eq!(oa.pdf_url, None);

        let closed = parse_unpaywall(r#"{"is_oa": false, "best_oa_location": null}"#).unwrap();
        assert!(!closed.is_oa);
        assert_eq!(closed.pdf_url, None);
    }

    // Only best_oa_location is consulted; other oa_locations are ignored on
    // purpose (see the comment in parse_unpaywall).
    #[test]
    fn other_oa_locations_are_not_consulted() {
        let json = r#"{
            "is_oa": true,
            "best_oa_location": {"url_for_pdf": null},
            "oa_locations": [{"url_for_pdf": "https://repo.example/paper.pdf"}]
        }"#;
        assert_eq!(parse_unpaywall(json).unwrap().pdf_url, None);

        let both = r#"{"is_oa": true,
            "best_oa_location": {"url_for_pdf": "https://best.example/a.pdf"}}"#;
        assert_eq!(
            parse_unpaywall(both).unwrap().pdf_url.as_deref(),
            Some("https://best.example/a.pdf")
        );
    }
}
