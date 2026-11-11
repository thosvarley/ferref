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

use ureq::Agent;
use ureq::http::{Response, Uri};

use crate::models::{Author, Entry};

const CROSSREF_BASE: &str = "https://api.crossref.org/works";
const UNPAYWALL_BASE: &str = "https://api.unpaywall.org/v2";
// The tail of the 404 message `fetch_guarded` produces for JSON services.
const NO_RECORD_404: &str = "has no record for this DOI (404)";
const OPENLIBRARY_BASE: &str = "https://openlibrary.org";

// Plain and fixed for both APIs. Crossref doesn't need identifying info;
// Unpaywall gets the contact email as a query parameter instead, per its
// polite-pool policy -- never in the User-Agent, never hardcoded.
const USER_AGENT: &str = "ferref/0.1";

// `pub(crate)`, not private: `fetch_pdf_for_entry`'s 60s deadline (main.rs)
// needs this as the ceiling for `min(JSON_TIMEOUT, remaining)` when it calls
// the OA-lookup functions below with less than a full JSON_TIMEOUT left.
pub(crate) const JSON_TIMEOUT: Duration = Duration::from_secs(30);
/// Default per-request timeout for `download_pdf`, used by callers (like
/// `add --url`) that download exactly one PDF. `fetch`'s candidate loop
/// instead caps each attempt at whatever's left of its own overall
/// deadline, which is always well under this.
pub const DEFAULT_PDF_TIMEOUT: Duration = Duration::from_secs(120);
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
/// caller's job (see `derive_cite_key` in main.rs, used by `add --doi`).
pub fn fetch_metadata(doi: &str) -> Result<Entry, String> {
    validate_doi(doi)?;
    let url = format!("{CROSSREF_BASE}/{}", percent_encode(doi));
    let body = get_text(&url, "Crossref", JSON_TIMEOUT)?;
    parse_crossref(&body)
}

/// True for exactly the error `fetch_metadata` returns when Crossref has no
/// record for the DOI at all (a real "not indexed here" answer, e.g. every
/// DataCite DOI -- Zenodo, Figshare, arXiv's 10.48550/... -- since none of
/// those are Crossref members) -- as opposed to a network failure, rate
/// limit, or any other error, which callers must still treat as fatal.
/// String-matched, not a typed error, since `get_text`'s callers all share
/// one `Result<_, String>` shape; kept as one function so the exact wording
/// only has to be right in one place.
pub fn is_no_record_404(err: &str) -> bool {
    err == format!("Crossref {NO_RECORD_404}")
}

/// Looks up everything Unpaywall knows about `doi`: every PDF URL it lists
/// (not just `best_oa_location`) and, if present, a PMC id to try
/// separately. An empty `pdf_urls` is a normal answer, not an error --
/// plenty of genuinely open papers are linked only as landing pages.
/// `timeout` lets `fetch`'s 60s overall deadline (main.rs) cap this call
/// too, rather than always waiting the full `JSON_TIMEOUT`.
pub fn fetch_oa_pdf_url(doi: &str, email: &str, timeout: Duration) -> Result<OaStatus, String> {
    validate_doi(doi)?;
    let url = format!(
        "{UNPAYWALL_BASE}/{}?email={}",
        percent_encode(doi),
        percent_encode(email)
    );
    let body = get_text(&url, "Unpaywall", timeout)?;
    parse_unpaywall(&body)
}

/// arXiv-registered DOI -> PDF URL, pure string parsing, no network call.
/// `None` means `doi` isn't an arXiv DOI (the normal case, not an error).
/// Matches DOI prefix `10.48550/arXiv.` case-insensitively on the `arXiv.`
/// label; everything after it is the arXiv id verbatim, including old-style
/// ids that contain their own `/` (e.g. `hep-th/9901001`) -- that internal
/// slash is not the DOI's own registrar/suffix separator, so it must not be
/// split on.
pub fn arxiv_pdf_url(doi: &str) -> Option<String> {
    const PREFIX: &str = "10.48550/arxiv.";
    let lower = doi.to_ascii_lowercase();
    if !lower.starts_with(PREFIX) {
        return None;
    }
    // PREFIX is ASCII, so its byte length matches the original (mixed-case)
    // doi's byte length for the same span -- slicing doi (not lower) here
    // preserves the id's real casing.
    let id = &doi[PREFIX.len()..];
    if id.is_empty() {
        return None;
    }
    Some(format!("https://arxiv.org/pdf/{id}.pdf"))
}

/// OSF-hosted-preprint DOI -> PDF URL, pure string parsing. `None` means
/// `doi` doesn't contain an `/osf.io/` segment -- matched as a
/// case-insensitive substring, not a fixed prefix list, since it has to
/// cover every OSF-hosted preprint server's own registrar prefix (OSF
/// Preprints, PsyArXiv, SocArXiv, EdArXiv, ...). Strips a trailing
/// `_v<digits>` version suffix by hand to get the bare guid, e.g.
/// `abc12_v1` -> `abc12`.
pub fn osf_pdf_url(doi: &str) -> Option<String> {
    let lower = doi.to_ascii_lowercase();
    let marker = "/osf.io/";
    let idx = lower.find(marker)?;
    let start = idx + marker.len();
    let rest = &doi[start..];
    let end = rest.find('/').unwrap_or(rest.len());
    let mut guid = &rest[..end];

    if let Some(pos) = guid.rfind("_v") {
        let suffix = &guid[pos + 2..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
            guid = &guid[..pos];
        }
    }

    if guid.is_empty() {
        return None;
    }
    Some(format!("https://osf.io/{guid}/download"))
}

/// preprints.org DOI -> PDF URL, pure string parsing. `None` means `doi`
/// isn't shaped `10.20944/preprints<manuscript>.v<version>`. Splits on the
/// last `.v` followed by trailing digits, since `<manuscript>` itself can
/// contain dots (e.g. `202001.0001`).
pub fn preprints_org_pdf_url(doi: &str) -> Option<String> {
    const PREFIX: &str = "10.20944/preprints";
    let lower = doi.to_ascii_lowercase();
    if !lower.starts_with(PREFIX) {
        return None;
    }
    // PREFIX and ".v" are both ASCII, so byte offsets found in `lower` land
    // on the same byte in `doi` -- slicing `doi` (not `lower`) here preserves
    // the manuscript id's real casing, the same trick `arxiv_pdf_url` uses.
    let lower_rest = &lower[PREFIX.len()..];
    let pos = lower_rest.rfind(".v")?;
    let rest = &doi[PREFIX.len()..];
    let manuscript = &rest[..pos];
    let version = &rest[pos + 2..];
    if manuscript.is_empty() || version.is_empty() || !version.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(format!(
        "https://www.preprints.org/manuscript/{manuscript}/v{version}/download"
    ))
}

const PMC_S3_BASE: &str = "https://pmc-oa-opendata.s3.amazonaws.com";

/// PMC id (e.g. "PMC9131462") -> its highest-version PDF URL, via the public
/// `pmc-oa-opendata` S3 bucket -- PMC's own article/PDF pages serve a
/// reCAPTCHA to scripts, and Europe PMC returns 403, but this bucket has no
/// bot check. `Ok(None)` means the article isn't in the open-access subset,
/// a normal answer, not an error. Rejects a malformed `pmcid` before making
/// any request. `timeout` lets `fetch`'s 60s overall deadline (main.rs) cap
/// this call too, rather than always waiting the full `JSON_TIMEOUT`.
pub fn pmc_pdf_url(pmcid: &str, timeout: Duration) -> Result<Option<String>, String> {
    let digits = pmcid
        .strip_prefix("PMC")
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()));
    let Some(digits) = digits else {
        return Err(format!(
            "'{pmcid}' is not a valid PMC id (expected \"PMC\" followed by digits)"
        ));
    };

    // The trailing dot after the id is mandatory: without it, a listing for
    // PMC1000034 also matches PMC10000341.*, PMC10000342.*, and so on, and a
    // different paper's PDF would download without complaint. See
    // parse_pmc_versions for the rest of the defence.
    let url = format!("{PMC_S3_BASE}/?list-type=2&prefix=PMC{digits}.&delimiter=/");
    let xml = get_text(&url, "PMC", timeout)?;
    let Some(version) = parse_pmc_versions(&xml, pmcid)? else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{PMC_S3_BASE}/{pmcid}.{version}/{pmcid}.{version}.pdf"
    )))
}

/// Scans an S3 `ListObjectsV2` response's `<Prefix>` values -- both the
/// query's own top-level echo and each `<CommonPrefixes><Prefix>` -- for
/// ones shaped exactly `<pmcid>.<digits>/`, and returns the numeric maximum.
/// Two traps this guards against, both seen in real responses: S3 sorts
/// keys as text, so "PMC1.10/" lists before "PMC1.2/" (taking the max
/// numerically, not the last entry, fixes that), and the response echoes the
/// query's own `<Prefix>PMC<id>.</Prefix>` with no version and no trailing
/// slash, which must be skipped rather than parsed as version "" or crashed
/// on. A prefix belonging to a different id (e.g. `PMC10000341.1/` when
/// asking about `PMC1000034`) is ignored too, on the strength of the
/// trailing dot in `pmcid`'s own prefix not matching.
///
/// A genuine "nothing in the open-access subset" answer is a real
/// `ListBucketResult` with zero matching prefixes -- `Ok(None)`. A response
/// that isn't a `ListBucketResult` at all (an error page, a truncated body,
/// garbage) is `Err`, not silently the same "nothing here": without this
/// check, unparseable junk and a real empty listing were indistinguishable,
/// and both counted toward `fetch`'s clean "no PDF anywhere" exit 0.
fn parse_pmc_versions(xml: &str, pmcid: &str) -> Result<Option<u32>, String> {
    if !xml.contains("<ListBucketResult") {
        return Err("PMC's S3 listing did not return a ListBucketResult".to_string());
    }

    let marker = format!("{pmcid}.");
    let mut best: Option<u32> = None;
    let mut rest = xml;
    while let Some(start) = rest.find("<Prefix>") {
        rest = &rest[start + "<Prefix>".len()..];
        let Some(end) = rest.find("</Prefix>") else {
            break;
        };
        let value = &rest[..end];
        rest = &rest[end + "</Prefix>".len()..];

        let Some(after_marker) = value.strip_prefix(&marker) else {
            continue;
        };
        let Some(version_str) = after_marker.strip_suffix('/') else {
            continue;
        };
        if version_str.is_empty() || !version_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(v) = version_str.parse::<u32>() {
            best = Some(best.map_or(v, |b| b.max(v)));
        }
    }
    Ok(best)
}

const BIORXIV_API_BASE: &str = "https://api.biorxiv.org/details";

/// bioRxiv/medRxiv DOI -> PDF URL. Unlike the other three sources, a
/// `10.1101/` DOI (shared by both servers -- the DOI alone can't tell which)
/// doesn't encode the version needed to build the PDF URL, so this makes a
/// JSON API call. Tries `api.biorxiv.org/details/biorxiv/<doi>` first; if
/// that reports no match, retries against `.../details/medrxiv/<doi>`.
/// `Ok(None)` means neither host has this DOI -- the normal "not from this
/// source" case, not an error. Only a genuine network/parse failure comes
/// back as `Err`. `timeout` (applied to each of the up-to-two host calls)
/// lets `fetch`'s 60s overall deadline (main.rs) cap this too, rather than
/// always waiting the full `JSON_TIMEOUT` per host.
pub fn biorxiv_pdf_url(doi: &str, timeout: Duration) -> Result<Option<String>, String> {
    validate_doi(doi)?;
    if !doi.starts_with("10.1101/") {
        return Ok(None);
    }

    for host in ["biorxiv", "medrxiv"] {
        let url = format!("{BIORXIV_API_BASE}/{host}/{}", percent_encode(doi));
        let body = get_text(&url, "bioRxiv", timeout)?;
        if let Some(version) = parse_biorxiv_details(&body)? {
            return Ok(Some(format!(
                "https://www.{host}.org/content/{doi}v{version}.full.pdf"
            )));
        }
    }
    Ok(None)
}

/// Picks the highest `version` out of a bioRxiv/medRxiv `/details` response's
/// `collection` array.
///
/// Two shapes count as the normal "not on this host" case, `Ok(None)`, never
/// a panic or an error: no `collection` key at all (bioRxiv's actual "no
/// posts found" response), or an empty `collection` array. But a response
/// that fails to parse as JSON, or where `collection` exists with the wrong
/// type, isn't a real "not found" answer -- it's `Err`, so it can't be
/// silently indistinguishable from one and count toward `fetch`'s clean "no
/// PDF anywhere" exit 0. Parsing is split from I/O here exactly like
/// `parse_crossref`/`parse_unpaywall`, so this is testable without the
/// network.
fn parse_biorxiv_details(json: &str) -> Result<Option<u32>, String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid bioRxiv JSON: {e}"))?;
    match v.get("collection") {
        None => Ok(None),
        Some(serde_json::Value::Array(collection)) => Ok(collection
            .iter()
            .filter_map(|item| {
                item.get("version")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u32>().ok())
            })
            .max()),
        Some(_) => Err("bioRxiv response's \"collection\" field is not an array".to_string()),
    }
}

/// Downloads the bytes at `url`, which must have come from a trusted call
/// site (Unpaywall JSON) and already passed the scheme check the caller is
/// expected to have done -- this function re-checks it anyway, since a URL
/// from a third-party API is hostile input regardless of who calls this.
/// `timeout` is the caller's to set: `add --url` uses `DEFAULT_PDF_TIMEOUT`,
/// while `fetch`'s candidate loop caps it at what's left of its own overall
/// deadline, so one slow/blocked candidate out of several can't block for
/// the full default on each.
///
/// Checks the `%PDF` magic bytes on just the first four bytes of the body,
/// before reading the rest -- Unpaywall's `url_for_pdf` not infrequently
/// lands on an HTML interstitial instead of the actual paper, and there's
/// no reason to read a whole (possibly large) one just to throw it away.
pub fn download_pdf(url: &str, timeout: Duration) -> Result<Vec<u8>, String> {
    let (bytes, _final_url) =
        fetch_guarded_with(url, timeout, MAX_PDF_BYTES, "PDF download", read_pdf_body)?;
    Ok(bytes)
}

/// Accepts a DOI pasted as a URL (`https://doi.org/10.1038/x`, a publisher
/// link like `.../doi/full/10.1002/x`) or with a `doi:` prefix, and returns
/// the bare `10.` form. Anything else comes back trimmed but otherwise
/// unchanged, so `validate_doi` still gets to reject it.
pub fn normalize_doi(input: &str) -> String {
    let s = input.trim();
    if s.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("doi:")) {
        return s[4..].trim().to_string();
    }
    if !s.contains("://") {
        return s.to_string();
    }
    let Some(start) = s.find("/10.") else {
        return s.to_string();
    };
    let doi = &s[start + 1..];
    let doi = doi.split(['?', '#']).next().unwrap_or(doi);
    doi.replace("%2F", "/").replace("%2f", "/")
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
        return Err(format!(
            "'{doi}' is not a DOI (they all start with \"10.\")"
        ));
    }
    Ok(())
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
            "citation_publication_date" | "citation_date" | "citation_year"
                if meta.year.is_none() =>
            {
                meta.year = content
                    .trim()
                    .get(..4)
                    .and_then(|y| y.parse::<i32>().ok())
                    .filter(|y| (1000..=9999).contains(y));
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

// Shared by every small-document caller here (Crossref/Unpaywall/bioRxiv
// JSON, and the PMC S3 listing, which is XML but tiny -- the JSON
// timeout/size limits are still the right ones for it).
fn get_text(url: &str, service: &str, timeout: Duration) -> Result<String, String> {
    let (bytes, _final_url) = fetch_guarded(url, timeout, MAX_JSON_BYTES, service)?;
    String::from_utf8(bytes).map_err(|_| format!("{service} returned invalid UTF-8"))
}

// Whether `what` names a call fetching bytes meant for a human to read (a
// PDF, or a landing page scraped for citation_pdf_url) rather than a JSON
// API this program parses. Only those two get the "download it in a browser
// and use ferref attach" suggestion -- Crossref, Unpaywall, PMC, and
// bioRxiv have no PDF for a human to fetch, so that advice would be
// nonsensical there; they just get told the host blocks automated requests.
fn suggests_manual_download(what: &str) -> bool {
    matches!(what, "PDF download" | "landing page")
}

fn cf_mitigated_message(what: &str, host: &str) -> String {
    if suggests_manual_download(what) {
        format!(
            "{host} blocks automated downloads with a bot check (HTTP 403); \
             download it in a browser and use `ferref attach` instead"
        )
    } else {
        format!("{host} blocks automated requests with a bot check (HTTP 403)")
    }
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
    fetch_guarded_with(url, timeout, limit, what, read_body_capped)
}

// Same as `fetch_guarded`, but the final "read the body" step is pulled out
// as a parameter -- `download_pdf` uses this to peek the first four bytes
// for the `%PDF` magic number before reading the rest, so an HTML
// interstitial doesn't get read in full just to be thrown away.
fn fetch_guarded_with(
    url: &str,
    timeout: Duration,
    limit: u64,
    what: &str,
    read_body: impl Fn(Response<ureq::Body>, u64, &str) -> Result<Vec<u8>, String>,
) -> Result<(Vec<u8>, String), String> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(timeout))
        // A 4xx/5xx comes back as Ok(response) so the status can be checked
        // explicitly below -- a raw ureq status error isn't an actionable
        // message.
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
            // "no record for this DOI" is right for the JSON APIs (Crossref,
            // Unpaywall, PMC, bioRxiv), all keyed on a DOI/id -- but a PDF
            // download or a landing page 404s on its own URL, nothing to do
            // with a DOI, so that wording would be actively misleading.
            404 if suggests_manual_download(what) => return Err(format!("{what} not found (404)")),
            404 => return Err(format!("{what} {NO_RECORD_404}")),
            429 => return Err(format!("{what} rate limit exceeded (429); try again later")),
            // Cloudflare marks a challenge page it served instead of the
            // real response with this response header -- a plain "HTTP 403"
            // reads like a permissions problem ferref could fix by trying
            // again, when the real answer is "a human has to click through
            // this in a browser".
            403 if resp
                .headers()
                .get("cf-mitigated")
                .and_then(|v| v.to_str().ok())
                == Some("challenge") =>
            {
                let host = uri.host().unwrap_or(what);
                return Err(cf_mitigated_message(what, host));
            }
            _ if !status.is_success() => return Err(format!("{what} returned HTTP {status}")),
            _ => {}
        }

        let body = read_body(resp, limit, what)?;
        return Ok((body, current));
    }

    Err(format!(
        "{what}: too many redirects (limit {MAX_REDIRECTS})"
    ))
}

// The default body reader: read up to `limit` bytes, erroring past it.
fn read_body_capped(resp: Response<ureq::Body>, limit: u64, what: &str) -> Result<Vec<u8>, String> {
    resp.into_body()
        .with_config()
        .limit(limit + 1)
        .read_to_vec()
        .map_err(|e| format!("failed reading {what} response (over the {limit}-byte cap): {e}"))
}

// Peeks the first four bytes for the `%PDF` magic number before reading the
// rest, so a non-PDF response (an HTML interstitial, a login page) is
// rejected off the first few bytes instead of reading the whole thing --
// which, for a large interstitial, is most of what download_pdf's timeout
// and byte cap are trying to bound in the first place.
fn read_pdf_body(resp: Response<ureq::Body>, limit: u64, what: &str) -> Result<Vec<u8>, String> {
    use std::io::Read;

    let not_a_pdf = || {
        "downloaded content is not a PDF (missing %PDF magic bytes) -- \
         this is usually an HTML interstitial, not the paper"
            .to_string()
    };

    let mut reader = resp.into_body().into_reader();
    let mut magic = [0u8; 4];
    match reader.read_exact(&mut magic) {
        Ok(()) if magic.starts_with(b"%PDF") => {}
        // A body shorter than four bytes can't be a PDF either -- treated
        // the same as a mismatched one, not a distinct I/O error.
        Ok(()) => return Err(not_a_pdf()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(not_a_pdf()),
        Err(e) => return Err(format!("failed reading {what} response: {e}")),
    }

    let mut rest = Vec::new();
    reader
        .take(limit.saturating_sub(4) + 1)
        .read_to_end(&mut rest)
        .map_err(|e| format!("failed reading {what} response (over the {limit}-byte cap): {e}"))?;
    if (rest.len() as u64) > limit.saturating_sub(4) {
        return Err(format!(
            "failed reading {what} response (over the {limit}-byte cap)"
        ));
    }

    let mut bytes = magic.to_vec();
    bytes.append(&mut rest);
    Ok(bytes)
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
    let port = uri
        .port_u16()
        .unwrap_or(if scheme == "https" { 443 } else { 80 });

    // `Uri::host()` keeps an IPv6 literal's brackets (`"[::1]"`), but
    // `ToSocketAddrs` for `(&str, u16)` only recognises the bracket-free
    // form -- with brackets left on, it tries (and fails) a DNS lookup on
    // the literal string "[::1]" instead of parsing it as an address, so an
    // IPv6 loopback/link-local/etc. literal never reached `is_internal` at
    // all and was rejected at "could not resolve" instead, for the wrong
    // reason.
    let host_for_lookup = host.strip_prefix('[').unwrap_or(host);
    let host_for_lookup = host_for_lookup.strip_suffix(']').unwrap_or(host_for_lookup);

    // A host with no resolvable address is a hard error rather than a pass:
    // "can't tell" must not mean "allow".
    let addrs: Vec<_> = (host_for_lookup, port)
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
                || v4.is_multicast()
                // RFC 6598 CGNAT range, 100.64.0.0/10 -- a real internal
                // range on some cloud/Kubernetes node networks, missed by
                // the private/link-local/etc. checks above.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 0b0100_0000)
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
                || (first & 0xff00) == 0xff00 // multicast ff00::/8
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
        // RFC 3986 §5.3 (merge): a relative reference resolves against the
        // base URI's directory -- everything up to and including the last
        // '/' in its path -- not against the host root. "12345.pdf" on a
        // base of ".../articles/9" must become ".../articles/12345.pdf",
        // not ".../12345.pdf". A base with no path at all (bare host) has
        // Uri::path() return "/", so rfind normally finds at least that
        // one '/'; the "/" fallback only guards a Uri that somehow doesn't
        // (map_or(1, ...) here would slice out of bounds on a truly empty
        // base_path, which .unwrap_or("/") avoids).
        let base_path = base.path();
        let dir = base_path
            .rfind('/')
            .map(|i| &base_path[..=i])
            .unwrap_or("/");
        Ok(format!("{scheme}://{authority}{dir}{location}"))
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

    let doi = message
        .get("DOI")
        .and_then(|d| d.as_str())
        .map(str::to_string);

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

    let volume = message
        .get("volume")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    // "page", not "pages", in Crossref's schema.
    let pages = message
        .get("page")
        .and_then(|v| v.as_str())
        .map(str::to_string);

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
    // For articles Crossref's publisher is the corporate parent, which BibTeX
    // ignores for @article; only books and chapters keep it.
    if matches!(entry.entry_type.as_str(), "book" | "incollection") {
        entry.publisher = message
            .get("publisher")
            .and_then(|p| p.as_str())
            .map(str::to_string);
    }
    entry.volume = volume;
    entry.pages = pages;
    entry.year = year;
    entry.abstract_text = abstract_text;
    for author in authors {
        entry.add_author(author);
    }
    Ok(entry)
}

/// Normalises a user-typed ISBN-10 or ISBN-13 to a bare ISBN-13.
pub fn normalize_isbn(input: &str) -> Result<String, String> {
    let trimmed = input.trim();
    // to_ascii_lowercase keeps byte offsets, so the prefix length indexes both.
    let lower = trimmed.to_ascii_lowercase();
    let rest = ["isbn-13", "isbn-10", "isbn"]
        .iter()
        .find(|p| lower.starts_with(*p))
        .map_or(trimmed, |p| trimmed[p.len()..].trim_start());
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let chars: Vec<char> = rest
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();

    match chars.len() {
        10 => {
            let digits = chars[..9]
                .iter()
                .map(|c| c.to_digit(10))
                .collect::<Option<Vec<u32>>>();
            let check = match chars[9] {
                'X' | 'x' => Some(10),
                c => c.to_digit(10),
            };
            let (Some(mut all), Some(check)) = (digits, check) else {
                return Err(format!(
                    "'{input}' is not a valid ISBN-10 (expected 9 digits and a digit or X)"
                ));
            };
            all.push(check);
            let sum: u32 = all.iter().zip((1..=10).rev()).map(|(d, w)| d * w).sum();
            if !sum.is_multiple_of(11) {
                return Err(format!("'{input}' has a bad ISBN-10 check digit"));
            }
            let mut thirteen: Vec<u32> = vec![9, 7, 8];
            thirteen.extend(&all[..9]);
            thirteen.push(isbn13_check_digit(&thirteen));
            Ok(thirteen.iter().map(|d| d.to_string()).collect())
        }
        13 => {
            let Some(digits) = chars
                .iter()
                .map(|c| c.to_digit(10))
                .collect::<Option<Vec<u32>>>()
            else {
                return Err(format!("'{input}' is not a valid ISBN-13 (digits only)"));
            };
            if digits[..3] != [9, 7, 8] && digits[..3] != [9, 7, 9] {
                return Err(format!(
                    "'{input}' is not an ISBN-13 (must start 978 or 979)"
                ));
            }
            if isbn13_check_digit(&digits[..12]) != digits[12] {
                return Err(format!("'{input}' has a bad ISBN-13 check digit"));
            }
            Ok(digits.iter().map(|d| d.to_string()).collect())
        }
        n => Err(format!(
            "'{input}' is not an ISBN: expected 10 or 13 characters, got {n}"
        )),
    }
}

// EAN-13 check digit over the first twelve digits (weights 1, 3, 1, 3, ...).
fn isbn13_check_digit(first12: &[u32]) -> u32 {
    let sum: u32 = first12
        .iter()
        .zip([1, 3].iter().cycle())
        .map(|(d, w)| d * w)
        .sum();
    (10 - sum % 10) % 10
}

/// "First Last" -> Author. Trailing suffixes (Jr, Sr, II, III, IV) are
/// dropped first; a name that still has a comma is "Last, First" and goes to
/// `parse_author`; otherwise the last token is the surname and a single token
/// is all surname. Known limitation: "Ludwig van Beethoven" splits as first
/// "Ludwig van", last "Beethoven".
fn split_name(name: &str) -> Option<Author> {
    let mut tokens: Vec<&str> = name.split_whitespace().collect();
    while tokens.len() > 1
        && let Some(last) = tokens.last()
        && ["jr", "sr", "ii", "iii", "iv"]
            .contains(&last.trim_end_matches('.').to_ascii_lowercase().as_str())
    {
        tokens.pop();
    }
    let joined = tokens.join(" ");
    let joined = joined.trim_end_matches(',');
    if joined.contains(',') {
        return crate::cli::parse_author(joined).ok();
    }
    match joined.rsplit_once(' ') {
        Some((first, last)) => Some(Author::new(last.to_string(), Some(first.to_string()))),
        None => (!joined.is_empty()).then(|| Author::new(joined.to_string(), None)),
    }
}

// `/authors/OL123A` or `/works/OL123W`: the only shapes ever interpolated
// into a request path.
fn is_ol_key(key: &str, prefix: &str, suffix: char) -> bool {
    key.strip_prefix(prefix)
        .and_then(|k| k.strip_suffix(suffix))
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

// First four-digit run in free text like "Mar 15, 2011" that is a plausible year.
fn year_from_publish_date(text: &str) -> Option<i32> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|run| run.len() == 4)
        .filter_map(|run| run.parse::<i32>().ok())
        .find(|y| (1000..=9999).contains(y))
}

// Valid, de-duplicated author keys from an edition (`authors[].key`) or a
// work (`authors[].author.key`) record; malformed keys are skipped.
fn author_keys(v: &serde_json::Value) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for a in v
        .get("authors")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
    {
        let key = a
            .get("key")
            .or_else(|| a.get("author").and_then(|x| x.get("key")))
            .and_then(|k| k.as_str());
        if let Some(key) = key
            && is_ol_key(key, "/authors/OL", 'A')
            && !keys.iter().any(|k| k == key)
        {
            keys.push(key.to_string());
        }
    }
    keys
}

/// Parses an Open Library edition record into a book `Entry` (no authors
/// yet), its valid author keys, and its valid work key.
fn parse_openlibrary_edition(json: &str) -> Result<(Entry, Vec<String>, Option<String>), String> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("invalid Open Library JSON: {e}"))?;
    let text = |field: &str| {
        v.get(field)
            .and_then(|t| t.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
    };
    let base = text("title").ok_or_else(|| "Open Library record has no title".to_string())?;
    let title = match text("subtitle") {
        Some(sub) => format!("{base}: {sub}"),
        None => base.to_string(),
    };

    let mut entry = Entry::new("book".to_string(), String::new(), title);
    entry.year = text("publish_date").and_then(year_from_publish_date);
    entry.publisher = v
        .get("publishers")
        .and_then(|p| p.as_array())
        .and_then(|a| a.first())
        .and_then(|p| p.as_str())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);

    let work_key = v
        .get("works")
        .and_then(|w| w.as_array())
        .into_iter()
        .flatten()
        .filter_map(|w| w.get("key")?.as_str())
        .find(|k| is_ol_key(k, "/works/OL", 'W'))
        .map(str::to_string);
    Ok((entry, author_keys(&v), work_key))
}

// GET on openlibrary.org; a 404 reports `not_found` instead of the generic
// "no record for this DOI" text.
fn ol_get(path: &str, not_found: &str) -> Result<String, String> {
    get_text(
        &format!("{OPENLIBRARY_BASE}{path}"),
        "Open Library",
        JSON_TIMEOUT,
    )
    .map_err(|e| {
        if e.ends_with(NO_RECORD_404) {
            not_found.to_string()
        } else {
            e
        }
    })
}

/// Looks up a book by ISBN-13 on Open Library: the edition, then (if the
/// edition lists none) the work's authors, then each author's name.
pub fn fetch_isbn_metadata(isbn13: &str) -> Result<Entry, String> {
    let body = ol_get(
        &format!("/isbn/{isbn13}.json"),
        &format!("Open Library has no record for ISBN {isbn13}"),
    )?;
    let (mut entry, mut keys, work_key) = parse_openlibrary_edition(&body)?;

    if keys.is_empty()
        && let Some(work_key) = work_key
    {
        let work = ol_get(
            &format!("{work_key}.json"),
            &format!("Open Library has no work record for ISBN {isbn13}"),
        )?;
        let v: serde_json::Value =
            serde_json::from_str(&work).map_err(|e| format!("invalid Open Library JSON: {e}"))?;
        keys = author_keys(&v);
    }

    if keys.len() > 50 {
        return Err(format!(
            "Open Library lists {} authors for ISBN {isbn13}; more than ferref will \
             fetch one at a time -- add it with --type/--key/--title instead",
            keys.len()
        ));
    }
    for key in &keys {
        let body = ol_get(
            &format!("{key}.json"),
            &format!("Open Library has no record for author {key}"),
        )?;
        let v: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| format!("invalid Open Library author JSON: {e}"))?;
        let author = v
            .get("name")
            .and_then(|n| n.as_str())
            .and_then(split_name)
            .ok_or_else(|| format!("Open Library author {key} has no name"))?;
        entry.add_author(author);
    }
    Ok(entry)
}

/// What Unpaywall knows about a DOI. `is_oa` with an empty `pdf_urls` is
/// common -- plenty of genuinely open papers are only linked as landing
/// pages -- and the two cases deserve different messages, so they're kept
/// apart here.
pub struct OaStatus {
    pub is_oa: bool,
    pub pdf_urls: Vec<String>,
    pub pmcid: Option<String>,
}

/// Parses an Unpaywall response body into every PDF URL it lists, in order,
/// plus a PMC id if one of its locations names one.
///
/// `best_oa_location.url_for_pdf` comes first, then every other
/// `oa_locations[].url_for_pdf`, deduplicated. Trying only the first pick
/// was tried once (see DESIGN.md's history of this) and reverted, because
/// stopping at the first *failed download* turned a clean "no PDF" into a
/// "not a PDF" error -- but that was a bug in stopping at the first
/// failure, not in having more candidates. Phase 26's caller moves on to the
/// next one instead, so every location Unpaywall lists is worth carrying.
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

    let mut pdf_urls: Vec<String> = Vec::new();
    if let Some(url) = v
        .get("best_oa_location")
        .filter(|loc| !loc.is_null())
        .and_then(pdf_of)
    {
        pdf_urls.push(url);
    }
    if let Some(locations) = v.get("oa_locations").and_then(|l| l.as_array()) {
        for loc in locations {
            if let Some(url) = pdf_of(loc)
                && !pdf_urls.contains(&url)
            {
                pdf_urls.push(url);
            }
        }
    }

    // A PMC id, if any location names one via its OAI-PMH identifier
    // (`oai:pubmedcentral.nih.gov:9131462` -> "PMC9131462"). Anything else
    // in pmh_id is ignored.
    let pmcid = v
        .get("oa_locations")
        .and_then(|l| l.as_array())
        .and_then(|locs| {
            locs.iter().find_map(|loc| {
                loc.get("pmh_id")
                    .and_then(|p| p.as_str())
                    .and_then(pmcid_from_pmh_id)
            })
        });

    Ok(OaStatus {
        is_oa,
        pdf_urls,
        pmcid,
    })
}

fn pmcid_from_pmh_id(pmh_id: &str) -> Option<String> {
    let digits = pmh_id.strip_prefix("oai:pubmedcentral.nih.gov:")?;
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        Some(format!("PMC{digits}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // M5: `add --url` falling back to page metadata on a DataCite DOI (no
    // Crossref record) hinges on matching this exact string -- any other
    // Crossref error (network, rate limit, ...) must stay fatal.
    #[test]
    fn is_no_record_404_matches_only_the_no_record_message() {
        assert!(is_no_record_404(
            "Crossref has no record for this DOI (404)"
        ));
        assert!(!is_no_record_404(
            "Crossref rate limit exceeded (429); try again later"
        ));
        assert!(!is_no_record_404("failed to reach Crossref: network error"));
    }

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
    // https://unpaywall.org/products/api
    #[test]
    fn parses_unpaywall_pdf_urls() {
        let plos = "https://journals.plos.org/plosone/article/file?id=10.1371/journal.pone.0000308&type=printable";
        let with_pdf = format!(
            r#"{{"is_oa": true, "best_oa_location": {{"url_for_pdf": "{plos}", "host_type": "publisher"}}}}"#
        );
        let cases = [
            ("with pdf", with_pdf.as_str(), vec![plos.to_string()]),
            // No legal OA copy: best_oa_location is null. Ok(empty), not an error.
            (
                "no oa copy",
                r#"{"is_oa": false, "best_oa_location": null}"#,
                vec![],
            ),
            // url_for_pdf itself can be null even when best_oa_location isn't.
            (
                "null pdf url",
                r#"{"best_oa_location": {"url_for_pdf": null}}"#,
                vec![],
            ),
        ];
        for (label, json, expected) in cases {
            assert_eq!(parse_unpaywall(json).unwrap().pdf_urls, expected, "{label}");
        }
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
        assert_eq!(
            m.pdf_url.as_deref(),
            Some("https://example.org/pdf/106-620.pdf")
        );
    }

    // A page with none of the tags must come back empty rather than
    // half-populated with garbage -- that's what makes the caller's "this page
    // isn't supported" message correct.
    #[test]
    fn a_page_without_citation_tags_yields_nothing() {
        assert_eq!(
            meta_of("<html><body>no meta here</body></html>"),
            PageMetadata::default()
        );
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
    // hop to the wrong host. Relative forms merge per RFC 3986 §5.3 (against
    // the base's directory, not the host root); a root-only base merges to "/".
    #[test]
    fn resolve_location_cases() {
        let cases = [
            (
                "https://good.example/a/b",
                "//cdn.example/x",
                "https://cdn.example/x",
            ),
            (
                "https://good.example/a/b",
                "HTTPS://other.example/x",
                "HTTPS://other.example/x",
            ),
            (
                "https://good.example/a/b",
                "https://other.example/x",
                "https://other.example/x",
            ),
            (
                "https://good.example/a/b",
                "/rooted",
                "https://good.example/rooted",
            ),
            (
                "https://good.example/a/b",
                "relative",
                "https://good.example/a/relative",
            ),
            ("https://host.example", "rel", "https://host.example/rel"),
        ];
        for (base, loc, expected) in cases {
            let base: Uri = base.parse().unwrap();
            assert_eq!(resolve_location(&base, loc).unwrap(), expected, "{loc}");
        }
    }

    #[test]
    fn html_entities_in_content_are_unescaped() {
        assert_eq!(unescape_html("a &amp; b"), "a & b");
        assert_eq!(unescape_html("&lt;i&gt;x&lt;/i&gt;"), "<i>x</i>");
        assert_eq!(
            unescape_html("Don&#39;t &#x2014; stop"),
            "Don't \u{2014} stop"
        );
        // Unknown and malformed entities are left exactly as written.
        assert_eq!(unescape_html("100&nbsp;% &amp"), "100&nbsp;% &amp");
    }

    #[test]
    fn sanitize_filename_cases() {
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
        // Safe names pass through unchanged.
        assert_eq!(sanitize_filename("kucsko2013").unwrap(), "kucsko2013");
        assert_eq!(sanitize_filename("smith-2024.v2").unwrap(), "smith-2024.v2");
    }

    fn body_response(data: &str) -> Response<ureq::Body> {
        let body = ureq::Body::builder().data(data.as_bytes().to_vec());
        ureq::http::Response::builder()
            .status(200)
            .body(body)
            .unwrap()
    }

    // read_pdf_body's whole point: a non-PDF body is rejected off its first
    // four bytes -- no network here, so this only checks the classification.
    #[test]
    fn read_pdf_body_cases() {
        // (label, body, byte cap, Ok(bytes) | Err(message fragment))
        let cases: [(&str, &str, u64, Result<&str, &str>); 4] = [
            (
                "non-pdf body",
                "<html>not a pdf</html>",
                1000,
                Err("not a PDF"),
            ),
            (
                "shorter than the magic number",
                "%PD",
                1000,
                Err("not a PDF"),
            ),
            (
                "real pdf keeps the rest",
                "%PDF-1.4 rest of the file",
                1000,
                Ok("%PDF-1.4 rest of the file"),
            ),
            (
                "cap enforced after the magic",
                "%PDF0123456789",
                4,
                Err("byte cap"),
            ),
        ];
        for (label, body, cap, expect) in cases {
            let got = read_pdf_body(body_response(body), cap, "PDF download");
            match expect {
                Ok(bytes) => assert_eq!(got.unwrap(), bytes.as_bytes(), "{label}"),
                Err(frag) => assert!(got.unwrap_err().contains(frag), "{label}"),
            }
        }
    }

    #[test]
    fn percent_encode_leaves_doi_slash_literal_and_escapes_special_chars() {
        assert_eq!(percent_encode("10.1038/nature12373"), "10.1038/nature12373");
        assert_eq!(percent_encode("a@b.com"), "a%40b.com");
        assert_eq!(percent_encode("10.1/has space"), "10.1/has%20space");
    }

    // The "download it in a browser" advice only makes sense for the two
    // callers fetching something a human could open (a PDF, a landing
    // page); a Crossref/Unpaywall/PMC/bioRxiv 403 gets the plain version.
    #[test]
    fn cf_mitigated_message_only_suggests_manual_download_for_pdf_and_page_callers() {
        for what in ["PDF download", "landing page"] {
            let msg = cf_mitigated_message(what, "example.org");
            assert!(msg.contains("example.org"));
            assert!(msg.contains("ferref attach"), "{what}: {msg}");
        }
        for what in ["Crossref", "Unpaywall", "PMC", "bioRxiv"] {
            let msg = cf_mitigated_message(what, "example.org");
            assert!(msg.contains("example.org"));
            assert!(!msg.contains("ferref attach"), "{what}: {msg}");
        }
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
            // B8: RFC 6598 CGNAT range, 100.64.0.0/10.
            "100.64.0.1",
            "100.127.255.255",
            // B8: IPv4 multicast.
            "224.0.0.1",
            // B8: IPv6 multicast ff00::/8 -- missed in the first pass, which
            // only added the IPv4 multicast check.
            "ff02::1",   // all-nodes link-local multicast
            "ff02::fb",  // mDNS
            "ff05::1:3", // site-local multicast
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip} should be internal");
        }
        for ip in [
            "1.1.1.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            // B8: just outside the CGNAT range on either side.
            "100.63.255.255",
            "100.128.0.0",
        ] {
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
        assert!(validate_url("not a url").is_err());
        // download_pdf must validate before any network call.
        assert!(download_pdf("file:///etc/passwd", Duration::from_secs(1)).is_err());
    }

    // Regression: `Uri::host()` keeps an IPv6 literal's brackets
    // ("[::1]"), but `ToSocketAddrs` only parses the bracket-free form --
    // left un-stripped, "[::1]" failed as an unresolvable DNS name instead
    // of ever reaching is_internal, so the loopback check never actually
    // ran for an IPv6 literal. Both sides here resolve with no network
    // access (a literal IP address is parsed directly, never looked up),
    // so this is safe to assert on without touching the network.
    #[test]
    fn validate_url_checks_ipv6_literals_against_is_internal_not_just_dns() {
        let err = validate_url("https://[::1]/x").unwrap_err();
        assert!(
            err.contains("internal address"),
            "expected an is_internal rejection, got: {err}"
        );

        // A real public IPv6 literal (Cloudflare's 1.1.1.1) must pass.
        assert!(validate_url("https://[2606:4700:4700::1111]/x").is_ok());
    }

    #[test]
    fn normalize_doi_strips_url_and_prefix() {
        assert_eq!(
            normalize_doi("https://doi.org/10.1038/nature12373"),
            "10.1038/nature12373"
        );
        assert_eq!(
            normalize_doi(" http://dx.doi.org/10.1103/X.1?via=a#b "),
            "10.1103/X.1"
        );
        assert_eq!(
            normalize_doi("https://onlinelibrary.wiley.com/doi/full/10.1002/abc.123"),
            "10.1002/abc.123"
        );
        assert_eq!(
            normalize_doi("https://doi.org/10.1000%2Fxyz"),
            "10.1000/xyz"
        );
        assert_eq!(normalize_doi("DOI: 10.1000/xyz"), "10.1000/xyz");
        assert_eq!(normalize_doi("10.1000/xyz"), "10.1000/xyz");
        assert_eq!(
            normalize_doi("https://example.com/paper"),
            "https://example.com/paper"
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
        assert!(oa.pdf_urls.is_empty());

        let closed = parse_unpaywall(r#"{"is_oa": false, "best_oa_location": null}"#).unwrap();
        assert!(!closed.is_oa);
        assert!(closed.pdf_urls.is_empty());
    }

    // best_oa_location comes first, then every other oa_locations[]
    // url_for_pdf in order, deduplicated -- Phase 26 reverted the earlier
    // "only best_oa_location" rule (see parse_unpaywall's doc comment).
    #[test]
    fn every_oa_location_pdf_url_is_collected_in_order_and_deduped() {
        let json = r#"{
            "is_oa": true,
            "best_oa_location": {"url_for_pdf": "https://best.example/a.pdf"},
            "oa_locations": [
                {"url_for_pdf": "https://best.example/a.pdf"},
                {"url_for_pdf": null},
                {"url_for_pdf": "https://repo.example/paper.pdf"}
            ]
        }"#;
        assert_eq!(
            parse_unpaywall(json).unwrap().pdf_urls,
            vec![
                "https://best.example/a.pdf".to_string(),
                "https://repo.example/paper.pdf".to_string(),
            ]
        );
    }

    // Real APS paper (Phase 26 fixture, captured 2026-09-25): the best pick
    // is a publisher PDF that's blocked in practice, but the same response
    // also lists an arXiv copy as a second oa_locations entry, and a
    // repository entry that's landing-page-only (url_for_pdf null) and must
    // be skipped, not turned into a "not a PDF" candidate.
    const UNPAYWALL_APS: &str = r#"
    {
      "is_oa": true,
      "best_oa_location": {
        "pmh_id": null,
        "url_for_pdf": "http://link.aps.org/pdf/10.1103/PhysRevE.100.032305"
      },
      "oa_locations": [
        {
          "pmh_id": null,
          "url_for_pdf": "http://link.aps.org/pdf/10.1103/PhysRevE.100.032305"
        },
        {
          "pmh_id": "oai:arXiv.org:1902.11239",
          "url_for_pdf": "https://arxiv.org/pdf/1902.11239"
        },
        {
          "pmh_id": "oai:infoscience.epfl.ch:270413",
          "url_for_pdf": null
        }
      ]
    }
    "#;

    #[test]
    fn parses_aps_response_blocked_pick_then_arxiv_fallback() {
        let oa = parse_unpaywall(UNPAYWALL_APS).unwrap();
        assert_eq!(
            oa.pdf_urls,
            vec![
                "http://link.aps.org/pdf/10.1103/PhysRevE.100.032305".to_string(),
                "https://arxiv.org/pdf/1902.11239".to_string(),
            ]
        );
        // arXiv.org's own pmh_id is not a PMC one.
        assert_eq!(oa.pmcid, None);
    }

    // Real Royal Society paper (Phase 26 fixture, captured 2026-09-25): no
    // url_for_pdf anywhere, but a PMC pmh_id that fetch_pdf_for_entry can
    // still try.
    const UNPAYWALL_RSTA: &str = r#"
    {
      "is_oa": true,
      "best_oa_location": {
        "pmh_id": null,
        "url_for_pdf": null
      },
      "oa_locations": [
        {
          "pmh_id": null,
          "url_for_pdf": null
        },
        {
          "pmh_id": "oai:pubmedcentral.nih.gov:9131462",
          "url_for_pdf": null
        }
      ]
    }
    "#;

    #[test]
    fn parses_rsta_response_no_pdf_but_a_pmc_id() {
        let oa = parse_unpaywall(UNPAYWALL_RSTA).unwrap();
        assert!(oa.pdf_urls.is_empty());
        assert_eq!(oa.pmcid.as_deref(), Some("PMC9131462"));
    }

    #[test]
    fn pmh_id_shapes_that_are_not_a_pmc_record_are_ignored() {
        assert_eq!(pmcid_from_pmh_id("oai:arXiv.org:1902.11239"), None);
        assert_eq!(pmcid_from_pmh_id("oai:pubmedcentral.nih.gov:"), None);
        assert_eq!(pmcid_from_pmh_id("oai:pubmedcentral.nih.gov:abc"), None);
        assert_eq!(
            pmcid_from_pmh_id("oai:pubmedcentral.nih.gov:9131462"),
            Some("PMC9131462".to_string())
        );
    }

    // The two real S3 listings captured 2026-09-25.
    const S3_NO_VERSIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
      <Prefix>PMC6112690.</Prefix><KeyCount>0</KeyCount>
    </ListBucketResult>"#;

    const S3_ONE_VERSION: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
      <Prefix>PMC9131462.</Prefix><KeyCount>1</KeyCount>
      <CommonPrefixes><Prefix>PMC9131462.1/</Prefix></CommonPrefixes>
    </ListBucketResult>"#;

    const S3_TWO_VERSIONS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
      <Prefix>PMC10000341.</Prefix><KeyCount>2</KeyCount>
      <CommonPrefixes><Prefix>PMC10000341.1/</Prefix></CommonPrefixes>
      <CommonPrefixes><Prefix>PMC10000341.2/</Prefix></CommonPrefixes>
    </ListBucketResult>"#;

    #[test]
    fn parse_pmc_versions_cases() {
        // S3 sorts keys as text, so version 10 lists before version 2 -- the
        // numeric maximum must win. A neighbouring id's prefix (PMC10000341
        // contains PMC1000034 as a textual prefix) must be ignored, or
        // PMC1000034 would silently download a different paper.
        let ten_and_two = r#"<ListBucketResult><Prefix>PMC1.</Prefix>
            <CommonPrefixes><Prefix>PMC1.10/</Prefix></CommonPrefixes>
            <CommonPrefixes><Prefix>PMC1.2/</Prefix></CommonPrefixes></ListBucketResult>"#;
        let neighbour = r#"<ListBucketResult><Prefix>PMC1000034.</Prefix>
            <CommonPrefixes><Prefix>PMC10000341.1/</Prefix></CommonPrefixes></ListBucketResult>"#;
        let cases = [
            ("empty listing", S3_NO_VERSIONS, "PMC6112690", None),
            ("single version", S3_ONE_VERSION, "PMC9131462", Some(1)),
            (
                "two versions take the max",
                S3_TWO_VERSIONS,
                "PMC10000341",
                Some(2),
            ),
            ("numeric not textual max", ten_and_two, "PMC1", Some(10)),
            ("neighbouring id's prefix", neighbour, "PMC1000034", None),
        ];
        for (label, xml, id, expected) in cases {
            assert_eq!(parse_pmc_versions(xml, id).unwrap(), expected, "{label}");
        }
        // L3: garbage (not a ListBucketResult at all) must not be silently
        // indistinguishable from a real empty listing.
        for bad in ["<html>Service Unavailable</html>", "", "not xml at all"] {
            assert!(parse_pmc_versions(bad, "PMC1").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pmc_pdf_url_rejects_a_malformed_pmcid() {
        // Rejected by validation before any network call, so the timeout
        // value here is never actually used.
        assert!(pmc_pdf_url("9131462", JSON_TIMEOUT).is_err());
        assert!(pmc_pdf_url("PMC", JSON_TIMEOUT).is_err());
        assert!(pmc_pdf_url("PMCabc", JSON_TIMEOUT).is_err());
        assert!(pmc_pdf_url("PMC123abc", JSON_TIMEOUT).is_err());
    }

    #[test]
    fn preprint_server_pdf_urls() {
        type F = fn(&str) -> Option<String>;
        // (label, resolver, doi, expected). Case-insensitive on the labels
        // throughout (DOIs are case-insensitive); preprints.org once matched
        // its prefix case-sensitively and missed a capitalized DOI.
        let cases: [(&str, F, &str, Option<&str>); 13] = [
            (
                "arxiv new",
                arxiv_pdf_url,
                "10.48550/arXiv.2301.00001",
                Some("https://arxiv.org/pdf/2301.00001.pdf"),
            ),
            // Old-style id contains its own '/', which must survive intact.
            (
                "arxiv old",
                arxiv_pdf_url,
                "10.48550/arXiv.hep-th/9901001",
                Some("https://arxiv.org/pdf/hep-th/9901001.pdf"),
            ),
            (
                "arxiv case",
                arxiv_pdf_url,
                "10.48550/ARXIV.2301.00001",
                Some("https://arxiv.org/pdf/2301.00001.pdf"),
            ),
            (
                "arxiv non-arxiv",
                arxiv_pdf_url,
                "10.1038/nature12373",
                None,
            ),
            ("arxiv empty id", arxiv_pdf_url, "10.48550/arXiv.", None),
            (
                "osf",
                osf_pdf_url,
                "10.31219/osf.io/abc12",
                Some("https://osf.io/abc12/download"),
            ),
            // PsyArXiv: a different registrar prefix, same /osf.io/ segment.
            (
                "osf psyarxiv",
                osf_pdf_url,
                "10.31234/osf.io/xyz98",
                Some("https://osf.io/xyz98/download"),
            ),
            // Versioned: the guid must come out bare, not with "_v1" attached.
            (
                "osf versioned",
                osf_pdf_url,
                "10.31219/osf.io/abc12_v1",
                Some("https://osf.io/abc12/download"),
            ),
            ("osf non-osf", osf_pdf_url, "10.1038/nature12373", None),
            (
                "preprints",
                preprints_org_pdf_url,
                "10.20944/preprints202001.0001.v1",
                Some("https://www.preprints.org/manuscript/202001.0001/v1/download"),
            ),
            (
                "preprints case",
                preprints_org_pdf_url,
                "10.20944/Preprints202001.0001.V1",
                Some("https://www.preprints.org/manuscript/202001.0001/v1/download"),
            ),
            (
                "preprints non-preprints",
                preprints_org_pdf_url,
                "10.1038/nature12373",
                None,
            ),
            (
                "preprints no version",
                preprints_org_pdf_url,
                "10.20944/preprints202001.0001",
                None,
            ),
        ];
        for (label, f, doi, expected) in cases {
            assert_eq!(f(doi).as_deref(), expected, "{label}");
        }
    }

    #[test]
    fn parse_biorxiv_details_cases() {
        let cases = [
            // Real shape (trimmed): a multi-version posting, highest wins.
            (
                "highest version",
                r#"{"collection":[{"doi":"10.1101/x","version":"1"},{"doi":"10.1101/x","version":"2"}],"messages":[{"status":"ok","count":"2"}]}"#,
                Some(2),
            ),
            // bioRxiv's real "not found" shape: no `collection` key at all.
            (
                "no collection key",
                r#"{"messages":[{"status":"no posts found matching the DOI"}]}"#,
                None,
            ),
            // An empty collection array is the same "not found" case.
            (
                "empty collection",
                r#"{"collection":[],"messages":[{"status":"ok","count":"0"}]}"#,
                None,
            ),
            // A non-numeric version on one item is skipped, not an error.
            (
                "non-numeric version skipped",
                r#"{"collection":[{"version":"not-a-number"},{"version":"3"}]}"#,
                Some(3),
            ),
        ];
        for (label, json, expected) in cases {
            assert_eq!(parse_biorxiv_details(json).unwrap(), expected, "{label}");
        }
        // L3: malformed JSON or a wrongly-typed "collection" is Err, not a
        // silent "not found".
        for bad in [
            "not json",
            r#"{"collection":"oops"}"#,
            r#"{"collection":42}"#,
        ] {
            assert!(parse_biorxiv_details(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn normalize_isbn_cases() {
        let cases: &[(&str, &str, Result<&str, &str>)] = &[
            ("valid 13", "9780199603398", Ok("9780199603398")),
            ("hyphenated 13", "978-0-19-960339-8", Ok("9780199603398")),
            (
                "ISBN: prefix",
                "ISBN: 978-0-19-960339-8",
                Ok("9780199603398"),
            ),
            (
                "ISBN-13 prefix",
                "isbn-13: 9780199603398",
                Ok("9780199603398"),
            ),
            ("979 prefix", "9791032300961", Ok("9791032300961")),
            ("10 -> 13", "0199603391", Ok("9780199603398")),
            ("10 with X", "080442957X", Ok("9780804429573")),
            ("bad 10 check", "0199603392", Err("check digit")),
            ("bad 13 check", "9780199603399", Err("check digit")),
            ("wrong length", "12345", Err("10 or 13")),
            ("non-978/979 13", "9770199603390", Err("978 or 979")),
        ];
        for (label, input, expect) in cases {
            let got = normalize_isbn(input);
            match expect {
                Ok(want) => assert_eq!(got.as_deref(), Ok(*want), "{label}"),
                Err(frag) => {
                    let e = got.expect_err(label);
                    assert!(e.contains(frag), "{label}: {e}");
                }
            }
        }
    }

    #[test]
    fn split_name_cases() {
        let cases = [
            ("James Oxley", "Oxley", Some("James")),
            ("Jean-Pierre Serre", "Serre", Some("Jean-Pierre")),
            ("Plato", "Plato", None),
            ("Ludwig van Beethoven", "Beethoven", Some("Ludwig van")),
            ("John Fox Jr.", "Fox", Some("John")),
            ("Voddie T. Baucham, Jr.", "Baucham", Some("Voddie T.")),
            ("Oxley, James G.", "Oxley", Some("James G.")),
        ];
        for (name, last, first) in cases {
            let a = split_name(name).unwrap();
            assert_eq!(a.last_name, last, "{name}: last");
            assert_eq!(a.first_name.as_deref(), first, "{name}: first");
        }
    }

    #[test]
    fn parse_openlibrary_edition_cases() {
        type ExpectedEdition = (
            &'static str,
            Option<i32>,
            Option<&'static str>,
            Vec<&'static str>,
            Option<&'static str>,
        );
        // (label, json, expect: Ok((title, year, publisher, author keys, work key)))
        let cases: Vec<(&str, &str, Option<ExpectedEdition>)> = vec![
            (
                "subtitle, March 2011, authors",
                r#"{"title":"Matroid Theory","subtitle":"Second edition","publish_date":"March 2011",
                    "publishers":["Oxford University Press"],
                    "authors":[{"key":"/authors/OL5666395A"}],
                    "works":[{"key":"/works/OL20438223W"}]}"#,
                Some((
                    "Matroid Theory: Second edition",
                    Some(2011),
                    Some("Oxford University Press"),
                    vec!["/authors/OL5666395A"],
                    Some("/works/OL20438223W"),
                )),
            ),
            (
                "no authors, work key returned",
                r#"{"title":"Matroid Theory","publish_date":"2011",
                    "works":[{"key":"/works/OL20438223W"}]}"#,
                Some((
                    "Matroid Theory",
                    Some(2011),
                    None,
                    vec![],
                    Some("/works/OL20438223W"),
                )),
            ),
            (
                "malformed author key skipped",
                r#"{"title":"T","authors":[{"key":"/authors/../../x"},{"key":"/works/OL1W"},{"key":"/authors/OL7A"}]}"#,
                Some(("T", None, None, vec!["/authors/OL7A"], None)),
            ),
            (
                "work-shaped, duplicate, and padded values",
                r#"{"title":"T","publish_date":"0123 2011","publishers":["  "],
                    "authors":[{"author":{"key":"/authors/OL7A"}},{"key":"/authors/OL7A"}]}"#,
                Some(("T", Some(2011), None, vec!["/authors/OL7A"], None)),
            ),
            ("no title", r#"{"publish_date":"2011"}"#, None),
            ("empty title", r#"{"title":"  "}"#, None),
        ];
        for (label, json, expect) in cases {
            let got = parse_openlibrary_edition(json);
            let Some((title, year, publisher, keys, work)) = expect else {
                assert!(got.is_err(), "{label}: should be an error");
                continue;
            };
            let (entry, got_keys, got_work) = got.unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(entry.entry_type, "book", "{label}: type");
            assert_eq!(entry.title, title, "{label}: title");
            assert_eq!(entry.year, year, "{label}: year");
            assert_eq!(entry.publisher.as_deref(), publisher, "{label}: publisher");
            assert_eq!(got_keys, keys, "{label}: author keys");
            assert_eq!(got_work.as_deref(), work, "{label}: work key");
        }
    }

    #[test]
    fn crossref_publisher_only_for_books_and_chapters() {
        let cases = [
            ("book", Some("OUP")),
            ("book-chapter", Some("OUP")),
            ("journal-article", None),
        ];
        for (kind, expect) in cases {
            let json =
                format!(r#"{{"message":{{"type":"{kind}","title":["T"],"publisher":"OUP"}}}}"#);
            let entry = parse_crossref(&json).unwrap();
            assert_eq!(entry.publisher.as_deref(), expect, "{kind}");
        }
    }
}
