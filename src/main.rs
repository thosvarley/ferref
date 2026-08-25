mod bibtex;
mod cli;
mod config;
mod db;
mod doi;
mod models;
mod text;
mod tui;

use std::path::{Path, PathBuf};

use clap::Parser;

use cli::{Cli, Command};
use models::{Author, Entry};

fn main() {
    let cli = Cli::parse();

    let root = match config::library_root() {
        Ok(r) => r,
        Err(e) => die(&e),
    };
    if let Err(e) = std::fs::create_dir_all(&root) {
        die(&format!("failed to create '{}': {e}", root.display()));
    }

    let conn = match db::init_db(&root.join("ferref.db")) {
        Ok(c) => c,
        Err(e) => die(&format!("failed to open database: {e}")),
    };

    match cli.command {
        Command::Add {
            entry_type,
            cite_key,
            title,
            authors,
            year,
            journal,
            volume,
            pages,
            doi,
            from_url,
            url,
            abstract_text,
            json,
        } => {
            // --from-url is the third way in, alongside --doi and the manual
            // flags. It reads a landing page's citation_* meta tags; if the
            // page names a DOI it hands off to the Crossref path below, since
            // publisher pages abbreviate and Crossref is authoritative.
            let mut pending_pdf: Option<String> = None;
            let mut doi = doi;
            let mut page: Option<doi::PageMetadata> = None;

            if let Some(page_url) = &from_url {
                let found = match doi::fetch_page_metadata(page_url) {
                    Ok(m) => m,
                    Err(e) => die(&format!("failed to read '{page_url}': {e}")),
                };
                if found.doi.is_none() && found.title.is_none() {
                    die(&format!(
                        "'{page_url}' has no citation_doi or citation_title meta tag -- \
                         ferref reads the Highwire Press tags publishers emit for Google \
                         Scholar, and this page doesn't carry them. Add it by hand, or \
                         with --doi if you know it."
                    ));
                }
                pending_pdf = found.pdf_url.clone();
                doi = found.doi.clone();
                page = Some(found);
            }

            // --doi fetches metadata from Crossref instead of taking it from
            // flags -- see Command::Add's docs. Everything else (--type,
            // --title, --author, --year, ...) is ignored in this mode; only
            // --key (to override the derived cite_key) still applies.
            let mut entry = if let Some(doi_value) = doi {
                let mut entry = match doi::fetch_metadata(&doi_value) {
                    Ok(e) => e,
                    Err(e) => die(&format!(
                        "failed to fetch metadata for DOI '{doi_value}': {e}"
                    )),
                };
                entry.doi = Some(doi_value);
                // Keep the page we were pointed at. Crossref supplies the
                // metadata but not this, and dropping it only on the DOI path
                // meant the common --from-url case lost the URL the user typed
                // while the rarer fallback kept it.
                if entry.url.is_none() {
                    entry.url = from_url.clone();
                }
                entry.cite_key = match cite_key {
                    Some(key) => key,
                    None => match derive_cite_key(&conn, &entry) {
                        Ok(key) => key,
                        Err(e) => die(&e),
                    },
                };
                entry
            } else if let Some(found) = page {
                // A page with no DOI: fall back to what it told us directly.
                // Weaker than Crossref, but it's the difference between working
                // on a preprint server and refusing to.
                let mut entry = Entry::new(
                    "article".to_string(),
                    String::new(),
                    found.title.clone().unwrap_or_default(),
                );
                for raw in &found.authors {
                    // citation_author is "Last, First" by convention; parse_author
                    // treats a comma-less name as all surname, which degrades
                    // sensibly for the publishers that ignore that.
                    match cli::parse_author(raw) {
                        Ok(author) => entry.add_author(author),
                        Err(_) => continue,
                    }
                }
                entry.year = found.year;
                entry.journal = found.journal.clone();
                entry.url = from_url.clone();
                entry.cite_key = match cite_key {
                    Some(key) => key,
                    None => match derive_cite_key(&conn, &entry) {
                        Ok(key) => key,
                        Err(e) => die(&e),
                    },
                };
                entry
            } else {
                // clap's required_unless_present_any guarantees these are Some
                // when neither --doi nor --from-url was passed.
                let mut entry = Entry::new(
                    entry_type.expect("--type required by clap without --doi"),
                    cite_key.expect("--key required by clap without --doi"),
                    title.expect("--title required by clap without --doi"),
                );
                for raw in authors {
                    match cli::parse_author(&raw) {
                        Ok(author) => entry.add_author(author),
                        Err(e) => die(&e),
                    }
                }
                entry.year = year;
                entry.journal = journal;
                entry.volume = volume;
                entry.pages = pages;
                entry.url = url;
                entry.abstract_text = abstract_text;
                entry
            };

            match db::insert_entry(&conn, &entry) {
                Ok(id) => entry.id = Some(id),
                Err(e) => die(&db_error("add entry", e)),
            }

            // The entry is committed before the PDF is attempted: a download
            // that fails (off the VPN, say) must not cost you the metadata you
            // just fetched. Same partial-failure rule as `attach --extract`.
            let pdf = pending_pdf.map(|url| add_pdf_from_page(&conn, &entry.cite_key, &url));

            output_entry(&entry, json);
            if !json {
                match &pdf {
                    Some(Ok(path)) => emit(&format!("Attached '{path}'")),
                    Some(Err(e)) => eprintln!("Warning: {e}"),
                    None => {}
                }
            }
            if matches!(pdf, Some(Err(_))) {
                std::process::exit(1);
            }
        }

        Command::List {
            tag,
            collection,
            recursive,
            full_text,
            json,
        } => match db::list_entries(
            &conn,
            &db::Filter {
                tag,
                collection_id: resolve_collection_filter(&conn, collection),
                recursive,
                ..Default::default()
            },
            full_text,
        ) {
            Ok(entries) => {
                if json {
                    emit_json(&entries);
                } else {
                    for entry in &entries {
                        emit(&format_list_line(entry));
                    }
                }
            }
            Err(e) => die(&format!("failed to list entries: {e}")),
        },

        Command::Show { cite_key, json } => match db::get_entry(&conn, &cite_key) {
            Ok(Some(entry)) => output_entry(&entry, json),
            Ok(None) => die(&format!("no entry found with cite_key '{cite_key}'")),
            Err(e) => die(&format!("failed to fetch entry: {e}")),
        },

        Command::Edit {
            cite_key,
            title,
            year,
            journal,
            volume,
            pages,
            doi,
            url,
            abstract_text,
            authors,
            json,
        } => {
            let mut entry = match db::get_entry(&conn, &cite_key) {
                Ok(Some(e)) => e,
                Ok(None) => die(&format!("no entry found with cite_key '{cite_key}'")),
                Err(e) => die(&format!("failed to fetch entry: {e}")),
            };

            if let Some(title) = title {
                entry.title = title;
            }
            if year.is_some() {
                entry.year = year;
            }
            if journal.is_some() {
                entry.journal = journal;
            }
            if volume.is_some() {
                entry.volume = volume;
            }
            if pages.is_some() {
                entry.pages = pages;
            }
            if doi.is_some() {
                entry.doi = doi;
            }
            if url.is_some() {
                entry.url = url;
            }
            if abstract_text.is_some() {
                entry.abstract_text = abstract_text;
            }
            if !authors.is_empty() {
                // Parse every author before assigning any, so one bad argument
                // can't half-replace a good author list.
                let parsed: Result<Vec<_>, _> =
                    authors.iter().map(|a| cli::parse_author(a)).collect();
                match parsed {
                    Ok(list) => entry.authors = list,
                    Err(e) => die(&e),
                }
            }

            if let Err(e) = db::update_entry(&conn, &entry) {
                die(&db_error("update entry", e));
            }

            // Refetch so the printed entry reflects what update_entry actually
            // wrote (it stamps date_modified itself).
            match db::get_entry(&conn, &cite_key) {
                Ok(Some(updated)) => output_entry(&updated, json),
                Ok(None) => die("entry vanished after update"),
                Err(e) => die(&format!("failed to fetch updated entry: {e}")),
            }
        }

        Command::Rm { cite_key, json } => {
            match db::get_entry(&conn, &cite_key) {
                Ok(Some(_)) => {}
                Ok(None) => die(&format!("no entry found with cite_key '{cite_key}'")),
                Err(e) => die(&format!("failed to fetch entry: {e}")),
            }
            if let Err(e) = db::delete_entry(&conn, &cite_key) {
                die(&format!("failed to delete entry: {e}"));
            }
            if json {
                let out = serde_json::json!({ "removed": cite_key });
                emit_json(&out);
            } else {
                emit(&format!("Removed '{cite_key}'"));
            }
        }

        Command::Merge { keep, drop, json } => {
            if keep == drop {
                die("keep and drop must be different entries");
            }
            let keep_id = match db::get_entry(&conn, &keep) {
                Ok(Some(e)) => e.id.unwrap(),
                Ok(None) => die(&format!("no entry found with cite_key '{keep}'")),
                Err(e) => die(&format!("failed to fetch entry: {e}")),
            };
            let drop_id = match db::get_entry(&conn, &drop) {
                Ok(Some(e)) => e.id.unwrap(),
                Ok(None) => die(&format!("no entry found with cite_key '{drop}'")),
                Err(e) => die(&format!("failed to fetch entry: {e}")),
            };
            if let Err(e) = db::merge_entries(&conn, keep_id, drop_id) {
                die(&format!("failed to merge entries: {e}"));
            }
            if json {
                let out = serde_json::json!({ "kept": keep, "dropped": drop });
                emit_json(&out);
            } else {
                emit(&format!("Merged '{drop}' into '{keep}', deleting '{drop}'"));
            }
        }

        Command::Tag {
            cite_key,
            tag,
            json,
        } => {
            let normalized = match db::normalize_tag(&tag) {
                Ok(t) => t,
                Err(e) => die(&format!("invalid tag: {e}")),
            };
            match db::add_tag(&conn, &cite_key, &normalized) {
                Ok(changed) => {
                    if json {
                        let out = serde_json::json!({
                            "cite_key": cite_key,
                            "tag": normalized,
                            "changed": changed,
                        });
                        emit_json(&out);
                    } else if changed {
                        emit(&format!("Tagged '{cite_key}' with '{normalized}'"));
                    } else {
                        emit(&format!("'{cite_key}' already tagged '{normalized}'"));
                    }
                }
                Err(e) => die(&entry_error(&cite_key, "tag entry", e)),
            }
        }

        Command::Untag {
            cite_key,
            tag,
            json,
        } => {
            let normalized = match db::normalize_tag(&tag) {
                Ok(t) => t,
                Err(e) => die(&format!("invalid tag: {e}")),
            };
            match db::remove_tag(&conn, &cite_key, &normalized) {
                Ok(changed) => {
                    if json {
                        let out = serde_json::json!({
                            "cite_key": cite_key,
                            "tag": normalized,
                            "changed": changed,
                        });
                        emit_json(&out);
                    } else if changed {
                        emit(&format!("Untagged '{cite_key}' from '{normalized}'"));
                    } else {
                        emit(&format!("'{cite_key}' was not tagged '{normalized}'"));
                    }
                }
                Err(e) => die(&entry_error(&cite_key, "untag entry", e)),
            }
        }

        Command::Search {
            author,
            title,
            year,
            from,
            to,
            tag,
            collection,
            recursive,
            full_text,
            text,
            json,
        } => {
            // An empty --text matches every attachment at the SQL level
            // (like_pattern("") is "%%"), but find_snippets on an empty
            // query always returns zero matches -- so every entry would be
            // silently dropped by text_search_results's "0 rust matches"
            // skip, printing nothing with no indication why. Reject it here
            // instead of leaving that skip to paper over the mismatch.
            if text.as_deref() == Some("") {
                die("--text cannot be empty");
            }

            // --year is shorthand for a single-year range, so the filter only
            // has to understand min/max.
            let filter = db::Filter {
                author,
                title,
                year_min: year.or(from),
                year_max: year.or(to),
                tag,
                collection_id: resolve_collection_filter(&conn, collection),
                recursive,
                text: text.clone(),
            };

            match db::list_entries(&conn, &filter, full_text) {
                Ok(entries) => {
                    if let Some(query) = &text {
                        // Deliberately NOT with_full_text=true on the query
                        // above: that would bulk-load every matching entry's
                        // full text into one Vec<Entry> at once -- on a large
                        // library that's hundreds of MB resident before a
                        // single snippet is cut, the same unbounded-memory
                        // shape --full-text needed a --json guard for.
                        // text_search_results instead loads one entry's
                        // attachments at a time, so peak memory is bounded by
                        // one attachment's text, not the whole matching set.
                        let results = text_search_results(&conn, &entries, query);
                        if json {
                            emit_json(&results);
                        } else {
                            for result in &results {
                                emit(&format_text_search_result(result));
                            }
                        }
                    } else if json {
                        emit_json(&entries);
                    } else {
                        for entry in &entries {
                            emit(&format_list_line(entry));
                        }
                    }
                }
                Err(e) => die(&format!("failed to search entries: {e}")),
            }
        }

        Command::Attach { cite_key, path, extract, json } => {
            cmd_attach(&conn, cite_key, path, extract, json)
        }

        Command::Extract { cite_key, json } => cmd_extract(&conn, cite_key, json),

        Command::Open { cite_key, json } => {
            let entry = match db::get_entry(&conn, &cite_key) {
                Ok(Some(e)) => e,
                Ok(None) => die(&format!("no entry found with cite_key '{cite_key}'")),
                Err(e) => die(&format!("failed to fetch entry: {e}")),
            };
            if entry.attachments.is_empty() {
                die(&format!("'{cite_key}' has no attachments"));
            }

            // Every attachment, not just the first: an entry usually has one,
            // and when it has two they're the paper and its supplement.
            for attachment in &entry.attachments {
                if let Err(e) = open_path(&attachment.path) {
                    die(&e);
                }
            }

            if json {
                let out = serde_json::json!({
                    "cite_key": cite_key,
                    "opened": entry.attachments.iter().map(|a| &a.path).collect::<Vec<_>>(),
                });
                emit_json(&out);
            } else {
                for attachment in &entry.attachments {
                    emit(&format!("Opened '{}'", attachment.path));
                }
            }
        }

        Command::Import { path, json } => cmd_import(&conn, path, json),

        Command::Export { out, biblatex } => {
            let entries = match db::list_entries(&conn, &db::Filter::default(), false) {
                Ok(e) => e,
                Err(e) => die(&format!("failed to list entries: {e}")),
            };
            let bibtex_str = bibtex::export(&entries, biblatex);

            match out {
                Some(path) => {
                    if let Err(e) = std::fs::write(&path, &bibtex_str) {
                        die(&format!("failed to write '{}': {e}", path.display()));
                    }
                }
                None => emit(bibtex_str.trim_end()),
            }
        }

        Command::Fetch { cite_key, email, json } => cmd_fetch(&conn, cite_key, email, json),

        Command::Collection { command } => dispatch_collection(&conn, command),

        Command::Doctor { json } => cmd_doctor(&conn, json),

        Command::Tui => {
            if let Err(e) = tui::run(&conn) {
                die(&e);
            }
        }
    }
}

fn cmd_attach(
    conn: &rusqlite::Connection,
    cite_key: String,
    path: PathBuf,
    extract: bool,
    json: bool,
) {
        let source = match cli::resolve_attachment_path(&path) {
            Ok(p) => p,
            Err(e) => die(&e),
        };
        let (resolved, copied) =
            match copy_into_library(conn, &cite_key, Path::new(&source)) {
                Ok(pair) => pair,
                Err(e) => die(&e),
            };
        let (attachment_id, changed) = match db::attach(conn, &cite_key, &resolved) {
            Ok(pair) => pair,
            Err(e) => {
                // Same rule as `fetch`: don't leave a copy in pdfs/ that
                // nothing in the library points at. Only remove what this
                // run wrote.
                if copied {
                    let _ = std::fs::remove_file(&resolved);
                }
                die(&entry_error(&cite_key, "attach file", e))
            }
        };

        // Extraction is independent of the attach: the path resolved (it
        // exists), which is all `attach` promises. The attach row is
        // committed above regardless of whether pdftotext can read it.
        let extraction: Option<Result<usize, String>> = if extract {
            Some(
                text::extract_text(Path::new(&resolved))
                    .and_then(|extracted| save_extracted(conn, attachment_id, &extracted)),
            )
        } else {
            None
        };

        if json {
            let mut out = serde_json::json!({
                "cite_key": cite_key,
                "path": resolved,
                "source": source,
                "changed": changed,
            });
            if let Some(result) = &extraction {
                out["extracted"] = serde_json::json!(result.is_ok());
                match result {
                    Ok(chars) => out["chars"] = serde_json::json!(chars),
                    Err(e) => out["extract_error"] = serde_json::json!(e),
                }
            }
            emit_json(&out);
        } else if changed {
            emit(&format!("Attached '{resolved}' to '{cite_key}'"));
        } else {
            emit(&format!("'{cite_key}' already has '{resolved}'"));
        }

        if let Some(Err(e)) = &extraction {
            eprintln!("Warning: extraction failed for '{resolved}': {e}");
            std::process::exit(1);
        } else if let Some(Ok(chars)) = &extraction {
            if !json {
                emit(&format!("Extracted {chars} characters from '{resolved}'"));
            }
        }
    }

fn cmd_extract(conn: &rusqlite::Connection, cite_key: String, json: bool) {
        let attachments = match db::attachments_for_cite_key(conn, &cite_key) {
            Ok(a) => a,
            Err(e) => die(&entry_error(&cite_key, "extract text", e)),
        };
        if attachments.is_empty() {
            die(&format!("'{cite_key}' has no attachments"));
        }

        // Per-attachment failures don't abort the rest -- collect every
        // result and report them all.
        let results: Vec<(String, Result<usize, String>)> = attachments
            .into_iter()
            .map(|(id, path)| {
                let result = text::extract_text(Path::new(&path))
                    .and_then(|extracted| save_extracted(conn, id, &extracted));
                (path, result)
            })
            .collect();

        let any_failed = results.iter().any(|(_, r)| r.is_err());

        if json {
            let attachments: Vec<_> = results
                .iter()
                .map(|(path, result)| match result {
                    Ok(chars) => serde_json::json!({
                        "path": path,
                        "extracted": true,
                        "chars": chars,
                    }),
                    Err(e) => serde_json::json!({
                        "path": path,
                        "extracted": false,
                        "error": e,
                    }),
                })
                .collect();
            let out = serde_json::json!({
                "cite_key": cite_key,
                "attachments": attachments,
            });
            emit_json(&out);
        } else {
            for (path, result) in &results {
                match result {
                    Ok(chars) => emit(&format!("Extracted {chars} characters from '{path}'")),
                    Err(e) => emit(&format!("Failed to extract '{path}': {e}")),
                }
            }
        }

        if any_failed {
            std::process::exit(1);
        }
    }

fn cmd_import(conn: &rusqlite::Connection, path: PathBuf, json: bool) {
        let entries = match bibtex::import(&path) {
            Ok(e) => e,
            Err(e) => die(&format!("failed to import '{}': {e}", path.display())),
        };

        let mut imported = Vec::new();
        let mut skipped = Vec::new();
        let mut rejected: Vec<(String, String)> = Vec::new();

        for entry in &entries {
            match db::insert_entry(conn, entry) {
                Ok(_) => imported.push(entry.cite_key.clone()),
                Err(e) => {
                    // Already-held rows are the only expected failure
                    // mode; anything else is bad data.
                    if is_duplicate(&e) {
                        skipped.push(entry.cite_key.clone());
                    } else {
                        rejected.push((entry.cite_key.clone(), e.to_string()));
                    }
                }
            }
        }

        // Skips are non-fatal by design, so re-importing a file you
        // already hold is a successful no-op. Only genuine bad data is a
        // failure -- keying the exit code off `imported` instead would
        // make 2 duplicates exit 1 while 1 duplicate + 1 new exits 0.
        let failed = !rejected.is_empty();

        if json {
            let out = serde_json::json!({
                "imported": imported.len(),
                "skipped": skipped.len(),
                "rejected": rejected.len(),
                "skipped_keys": skipped,
                "rejected_keys": rejected
                    .iter()
                    .map(|(k, r)| serde_json::json!({ "cite_key": k, "reason": r }))
                    .collect::<Vec<_>>(),
            });
            emit_json(&out);
        } else {
            emit(&format!(
                "imported {}, skipped {} (already in the library), rejected {} (bad data)",
                imported.len(),
                skipped.len(),
                rejected.len()
            ));
        }

        if failed {
            std::process::exit(1);
        }
    }

// What happened when trying to fetch an open-access PDF for an entry via
// Unpaywall. `doi` rides along in both branches since every caller reports
// it regardless of outcome, and `fetch_pdf_for_entry` is the only place that
// looked it up.
enum FetchOutcome {
    // Unpaywall was queried; either the paper isn't OA at all, or it's OA
    // with no direct PDF link -- `is_oa` is what tells those two apart, and
    // callers must not conflate them (telling someone their OA paper isn't
    // OA sends them looking for the wrong thing).
    NoPdfFound { doi: String, is_oa: bool },
    // A PDF was landed at `path` (or was already there, per
    // `already_present`) and attached as `attachment_id`. Extraction is a
    // separate, partial step -- a failed extraction still leaves the
    // attachment in place, so it travels inside `Ok` rather than failing the
    // whole fetch.
    Downloaded {
        doi: String,
        path: String,
        // Carried per the design brief for future consumers (e.g. a TUI
        // "re-extract" action); neither cmd_fetch nor the TUI's fetch
        // handler reads it today, so it stays here rather than being
        // dropped only to be re-derived later.
        #[allow(dead_code)]
        attachment_id: i64,
        already_present: bool,
        extraction: Result<usize, String>,
    },
}

// The actual work of `fetch`: DOI lookup on the entry, email resolution,
// Unpaywall query, download+land+extract. Pulled out of cmd_fetch so the TUI
// can call it too without dying on failure -- every error path here returns
// Err instead of calling die()/process::exit, which cmd_fetch alone still
// does, at the same messages it always has.
fn fetch_pdf_for_entry(
    conn: &rusqlite::Connection,
    cite_key: &str,
    email: Option<String>,
) -> Result<FetchOutcome, String> {
    let entry = db::get_entry(conn, cite_key)
        .map_err(|e| format!("failed to fetch entry: {e}"))?
        .ok_or_else(|| format!("no entry found with cite_key '{cite_key}'"))?;

    let Some(doi_value) = entry.doi.clone() else {
        return Err(format!(
            "'{cite_key}' has no DOI on record; set one with `ferref edit {cite_key} --doi <doi>`"
        ));
    };

    let resolved_email = config::resolve_email(email)?;

    let oa = doi::fetch_oa_pdf_url(&doi_value, &resolved_email)
        .map_err(|e| format!("failed to query Unpaywall: {e}"))?;

    // Having no PDF to fetch is a normal, legitimate answer, not an error.
    let Some(pdf_url) = oa.pdf_url else {
        return Ok(FetchOutcome::NoPdfFound {
            doi: doi_value,
            is_oa: oa.is_oa,
        });
    };

    let (path_str, attachment_id, already_present) = land_downloaded_pdf(conn, cite_key, &pdf_url)?;
    let abs_path = PathBuf::from(&path_str);

    // Partial failure: the attachment persists even if extraction fails --
    // same rule as `attach --extract` (Phase 7).
    let extraction: Result<usize, String> =
        text::extract_text(&abs_path).and_then(|extracted| save_extracted(conn, attachment_id, &extracted));

    Ok(FetchOutcome::Downloaded {
        doi: doi_value,
        path: path_str,
        attachment_id,
        already_present,
        extraction,
    })
}

fn cmd_fetch(
    conn: &rusqlite::Connection,
    cite_key: String,
    email: Option<String>,
    json: bool,
) {
        let outcome = match fetch_pdf_for_entry(conn, &cite_key, email) {
            Ok(o) => o,
            Err(e) => die(&e),
        };

        match outcome {
            FetchOutcome::NoPdfFound { doi, is_oa } => {
                if json {
                    let out = serde_json::json!({
                        "cite_key": cite_key,
                        "doi": doi,
                        "oa_found": false,
                        "is_oa": is_oa,
                    });
                    emit_json(&out);
                } else if is_oa {
                    emit(&format!(
                        "'{cite_key}' (DOI {doi}) is open access, but Unpaywall \
                         has no direct PDF link for it -- only landing pages"
                    ));
                } else {
                    emit(&format!(
                        "No open-access copy found for '{cite_key}' (DOI {doi})"
                    ));
                }
            }
            FetchOutcome::Downloaded {
                doi,
                path,
                already_present,
                extraction,
                ..
            } => {
                if json {
                    let mut out = serde_json::json!({
                        "cite_key": cite_key,
                        "doi": doi,
                        "oa_found": true,
                        "path": path,
                        "already_present": already_present,
                        "extracted": extraction.is_ok(),
                    });
                    match &extraction {
                        Ok(chars) => out["chars"] = serde_json::json!(chars),
                        Err(e) => out["extract_error"] = serde_json::json!(e),
                    }
                    emit_json(&out);
                } else {
                    emit(&format!(
                        "Downloaded open-access PDF for '{cite_key}' to '{path}'"
                    ));
                    match &extraction {
                        Ok(chars) => emit(&format!("Extracted {chars} characters from '{path}'")),
                        Err(e) => emit(&format!("Warning: extraction failed for '{path}': {e}")),
                    }
                }

                if extraction.is_err() {
                    std::process::exit(1);
                }
            }
        }
    }

// Roadmap item, scoped here: a read-only scan for attachment paths that no
// longer resolve on disk (a moved/deleted file, or a hand-edited DB row --
// the database is hand-editable by design, so this is a real, reachable
// state, not just a defensive check). Doesn't touch the filesystem beyond
// `Path::is_file`, and doesn't offer to fix anything -- that's future work
// once the report itself has been useful for a while.
fn cmd_doctor(conn: &rusqlite::Connection, json: bool) {
    let attachments = match db::all_attachment_paths(conn) {
        Ok(a) => a,
        Err(e) => die(&format!("failed to list attachments: {e}")),
    };

    let broken: Vec<(&String, &String)> = attachments
        .iter()
        .filter(|(_, path)| !std::path::Path::new(path).is_file())
        .map(|(cite_key, path)| (cite_key, path))
        .collect();

    if json {
        let out = serde_json::json!({
            "checked": attachments.len(),
            "broken": broken
                .iter()
                .map(|(cite_key, path)| serde_json::json!({ "cite_key": cite_key, "path": path }))
                .collect::<Vec<_>>(),
        });
        emit_json(&out);
    } else if broken.is_empty() {
        emit(&format!("All {} attachments resolve.", attachments.len()));
    } else {
        emit(&format!(
            "{} of {} attachments do not resolve on disk:",
            broken.len(),
            attachments.len()
        ));
        for (cite_key, path) in &broken {
            emit(&format!("  {cite_key}: {path}"));
        }
    }

    if !broken.is_empty() {
        std::process::exit(1);
    }
}

fn dispatch_collection(conn: &rusqlite::Connection, command: cli::CollectionCommand) {
    use cli::CollectionCommand;

    match command {
        CollectionCommand::New { path, json } => match db::create_collection(conn, &path) {
            Ok(id) => {
                if json {
                    let out = serde_json::json!({ "path": path, "id": id });
                    emit_json(&out);
                } else {
                    emit(&format!("Created collection '{path}' (id {id})"));
                }
            }
            Err(e) => die(&db_error("create collection", e)),
        },

        CollectionCommand::Ls { json } => {
            let tree = match db::collection_tree(conn) {
                Ok(t) => t,
                Err(e) => die(&format!("failed to list collections: {e}")),
            };

            // path is rebuilt from the (depth, collection) pre-order walk --
            // see db::collection_tree_paths, shared with the TUI.
            if json {
                let paths = db::collection_tree_paths(&tree);
                let rows: Vec<_> = tree
                    .iter()
                    .zip(paths.iter())
                    .map(|((depth, c), path)| {
                        serde_json::json!({
                            "id": c.id,
                            "name": c.name,
                            "parent_id": c.parent_id,
                            "depth": depth,
                            "path": path,
                            "entry_count": c.entry_count,
                        })
                    })
                    .collect();
                emit_json(&rows);
            } else {
                for (depth, c) in &tree {
                    emit(&format!("{}{} ({})", "  ".repeat(*depth), c.name, c.entry_count));
                }
            }
        }

        CollectionCommand::Add { path, cite_key, json } => {
            match db::add_to_collection(conn, &path, &cite_key) {
                Ok(changed) => {
                    if json {
                        let out = serde_json::json!({
                            "path": path,
                            "cite_key": cite_key,
                            "changed": changed,
                        });
                        emit_json(&out);
                    } else if changed {
                        emit(&format!("Added '{cite_key}' to '{path}'"));
                    } else {
                        emit(&format!("'{cite_key}' is already in '{path}'"));
                    }
                }
                Err(e) => die(&collection_entry_error(&cite_key, "add entry to collection", e)),
            }
        }

        CollectionCommand::Rm { path, cite_key, json } => {
            match db::remove_from_collection(conn, &path, &cite_key) {
                Ok(changed) => {
                    if json {
                        let out = serde_json::json!({
                            "path": path,
                            "cite_key": cite_key,
                            "changed": changed,
                        });
                        emit_json(&out);
                    } else if changed {
                        emit(&format!("Removed '{cite_key}' from '{path}'"));
                    } else {
                        emit(&format!("'{cite_key}' was not in '{path}'"));
                    }
                }
                Err(e) => {
                    die(&collection_entry_error(&cite_key, "remove entry from collection", e))
                }
            }
        }

        CollectionCommand::Mv { path, parent, root, json } => {
            let new_parent = match (parent, root) {
                (Some(p), false) => Some(p),
                (None, true) => None,
                (None, false) => die("collection mv requires either --parent <path> or --root"),
                (Some(_), true) => die("collection mv cannot take both --parent and --root"),
            };

            match db::move_collection(conn, &path, new_parent.as_deref()) {
                Ok(()) => {
                    if json {
                        let out = serde_json::json!({ "path": path, "new_parent": new_parent });
                        emit_json(&out);
                    } else {
                        match &new_parent {
                            Some(p) => emit(&format!("Moved '{path}' under '{p}'")),
                            None => emit(&format!("Moved '{path}' to the root")),
                        }
                    }
                }
                Err(e) => die(&db_error("move collection", e)),
            }
        }

        CollectionCommand::Delete { path, json } => match db::delete_collection(conn, &path) {
            Ok(count) => {
                if json {
                    let out = serde_json::json!({ "path": path, "deleted": count });
                    emit_json(&out);
                } else {
                    emit(&format!("Deleted '{path}' and {count} collection(s)"));
                }
            }
            Err(e) => die(&db_error("delete collection", e)),
        },
    }
}

// Derives a cite_key from the first author's last name + year when --doi is
// used without an explicit --key (e.g. "kucsko2013"), ASCII-sanitized. On
// collision with an existing key, appends "b", "c", ... up to "z" before
// giving up -- good enough for the practically-never case of 25 same-author-
// same-year entries.
fn derive_cite_key(conn: &rusqlite::Connection, entry: &Entry) -> Result<String, String> {
    let last_name = entry
        .authors
        .first()
        .map(|a| a.last_name.as_str())
        .unwrap_or("");
    let sanitized_name: String = last_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    let sanitized_name = if sanitized_name.is_empty() {
        "entry".to_string()
    } else {
        sanitized_name
    };
    let base = match entry.year {
        Some(y) => format!("{sanitized_name}{y}"),
        None => sanitized_name,
    };

    let exists = |key: &str| -> Result<bool, String> {
        db::get_entry(conn, key)
            .map(|e| e.is_some())
            .map_err(|e| format!("failed to check cite_key '{key}': {e}"))
    };

    if !exists(&base)? {
        return Ok(base);
    }
    for suffix in 'b'..='z' {
        let candidate = format!("{base}{suffix}");
        if !exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(format!(
        "could not derive a unique cite_key from '{base}' (too many collisions)"
    ))
}

fn die(msg: &str) -> ! {
    eprintln!("Error: {msg}");
    std::process::exit(1);
}

// All stdout goes through here. `println!` panics if the reader closed the pipe,
// which turns `ferref list --json | head` into an exit-101 panic once output
// exceeds the pipe buffer — unacceptable for a tool built to be piped.
fn emit(s: &str) {
    use std::io::{ErrorKind, Write};

    if let Err(e) = writeln!(std::io::stdout(), "{s}") {
        if e.kind() == ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        die(&format!("failed writing to stdout: {e}"));
    }
}

// All JSON output goes through here, streamed straight to stdout rather than
// built as a String first. `to_string_pretty` materialized the whole document
// beside the data it was serializing: on a 275MB corpus, `list --full-text
// --json` peaked at 562MB against 286MB for the same query without --json.
// That's the pipe-into-a-script path this tool exists to serve, so it's the
// one that must not double.
//
// BufWriter because stdout is line-buffered by default, which costs one
// write(2) per line -- measurable (30ms over 5000 entries) once the output
// isn't the bottleneck.
fn emit_json(value: &impl serde::Serialize) {
    use std::io::{ErrorKind, Write};

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    // serde_json wraps the underlying io::Error, so a closed pipe has to be
    // recovered from it rather than matched directly -- same exit-0 rule as
    // `emit`, which exists because Rust ignores SIGPIPE.
    let result = serde_json::to_writer_pretty(&mut out, value)
        .map_err(std::io::Error::from)
        .and_then(|()| writeln!(out))
        .and_then(|()| out.flush());

    if let Err(e) = result {
        if e.kind() == ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        die(&format!("failed writing to stdout: {e}"));
    }
}

// The system file opener. `status()` rather than `spawn()`: both xdg-open and
// macOS `open` hand off and exit immediately, and waiting is what lets a
// missing opener be reported instead of silently doing nothing.
fn open_path(path: &str) -> Result<(), String> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };

    // Detach all three streams from ours. stdout/stderr because xdg-open et
    // al. chatter on a headless system and inherited stdio scribbles over
    // the caller's terminal (the TUI screen, in particular).
    //
    // stdin because this blocks on the child: an opener that finds no
    // handler and drops to a prompt would otherwise sit reading the real
    // terminal, swallowing every keystroke meant for the TUI -- including
    // Ctrl-C, which raw mode delivers as a byte rather than a signal. That
    // is an unrecoverable freeze from inside the session. With stdin at
    // /dev/null the prompt reads EOF and the child gives up immediately.
    match std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("{opener} failed on '{path}' ({status})")),
        Err(e) => Err(format!("could not run {opener}: {e}")),
    }
}

// The PDF half of `add --from-url`: download what the page advertised, attach
// it, extract its text. Errors are returned rather than fatal -- see the call
// site for why the entry survives a failed download.
fn add_pdf_from_page(
    conn: &rusqlite::Connection,
    cite_key: &str,
    pdf_url: &str,
) -> Result<String, String> {
    let (path, attachment_id, _) = land_downloaded_pdf(conn, cite_key, pdf_url)?;
    let extracted = text::extract_text(Path::new(&path))
        .and_then(|text| save_extracted(conn, attachment_id, &text));
    match extracted {
        Ok(_) => Ok(path),
        // The attachment stands; only the text is missing.
        Err(e) => Err(format!("attached '{path}' but extraction failed: {e}")),
    }
}

// Downloads a PDF and lands it in ./pdfs/ under the entry's cite_key, then
// attaches it. Shared by `fetch` (URL from Unpaywall) and `add --from-url` (URL
// from a landing page's citation_pdf_url) -- the two differ only in where the
// URL came from, and writing it twice is how two copies drift apart.
//
// Returns (stored path, attachment id, whether the file was already there).
fn land_downloaded_pdf(
    conn: &rusqlite::Connection,
    cite_key: &str,
    pdf_url: &str,
) -> Result<(String, i64, bool), String> {
    let filename = doi::sanitize_filename(cite_key)?;
    let root = config::library_root()?;
    let dir = root.join("pdfs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create '{}': {e}", dir.display()))?;

    let (target, already_present) = pdf_target(conn, cite_key, &dir, &filename, "pdf", None)?;

    if !already_present {
        let bytes =
            doi::download_pdf(pdf_url).map_err(|e| format!("failed to download PDF: {e}"))?;
        // create_new claims the name atomically, the same rule copy_into_library
        // follows: two downloads racing on one cite_key both saw a free name,
        // and one silently overwrote the other.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "'{}' appeared while downloading; run the command again",
                    target.display()
                ));
            }
            Err(e) => return Err(format!("failed to create '{}': {e}", target.display())),
        }
        if let Err(e) = std::fs::write(&target, &bytes) {
            let _ = std::fs::remove_file(&target);
            return Err(format!("failed to save PDF to '{}': {e}", target.display()));
        }
    }

    // Every failure from here on has to clean up too, not just the attach: a
    // file this run wrote but never attached is invisible to `pdf_target`'s
    // "is it mine?" check, so it would squat on the name forever.
    let cleanup = |e: String| {
        if !already_present {
            let _ = std::fs::remove_file(&target);
        }
        e
    };

    let abs = target
        .canonicalize()
        .map_err(|e| cleanup(format!("failed to resolve saved PDF path '{}': {e}", target.display())))?;
    let path_str = abs
        .to_str()
        .ok_or_else(|| cleanup(format!("path {} is not valid UTF-8", abs.display())))?
        .to_string();

    let (attachment_id, _changed) = db::attach(conn, cite_key, &path_str)
        .map_err(|e| cleanup(entry_error(cite_key, "attach downloaded PDF", e)))?;

    Ok((path_str, attachment_id, already_present))
}

// Picks where a PDF goes under ./pdfs/, and says whether the file is already
// there.
//
// `doi::sanitize_filename` is many-to-one -- "a/b" and a literal "a_b" both
// become "a_b" -- and on a case-insensitive filesystem "Smith2020" and
// "smith2020" are one file as well. Treating whatever sits at the path as
// "already downloaded" would then attach another entry's PDF and report
// success, so a file is only reused when THIS entry already has it attached;
// otherwise we move to the next free name.
fn pdf_target(
    conn: &rusqlite::Connection,
    cite_key: &str,
    dir: &Path,
    base: &str,
    ext: &str,
    source: Option<&Path>,
) -> Result<(std::path::PathBuf, bool), String> {
    let mine: Vec<String> = db::attachments_for_cite_key(conn, cite_key)
        // An unknown cite_key surfaces here first, before anything is written
        // to disk, so it has to read as "no such entry" and not as a lookup
        // failure.
        .map_err(|e| entry_error(cite_key, &format!("list attachments for '{cite_key}'"), e))?
        .into_iter()
        .map(|(_, path)| path)
        .collect();
    pick_target(dir, base, ext, &mine, source)
}

// The Nth candidate in the `<base>.<ext>`, `<base>-2.<ext>`, `<base>-3.<ext>`,
// ... naming scheme every attachment-placing path (attach, fetch, merge)
// shares. Just the name, not a claim on it -- see claim_free_name for that.
fn nth_candidate_name(dir: &Path, base: &str, ext: &str, n: u32) -> std::path::PathBuf {
    if n == 1 {
        dir.join(format!("{base}.{ext}"))
    } else {
        dir.join(format!("{base}-{n}.{ext}"))
    }
}

// The filename choice on its own, so it can be tested without a database.
// `source` is the file about to be copied in, if any: `fetch` re-downloads one
// fixed URL per entry, so any file of this entry's already sitting at the name
// is that same PDF, but `attach` can be handed a second, different file for
// the same entry -- reusing the name there would silently drop it.
fn pick_target(
    dir: &Path,
    base: &str,
    ext: &str,
    mine: &[String],
    source: Option<&Path>,
) -> Result<(std::path::PathBuf, bool), String> {
    for n in 1..=50 {
        let candidate = nth_candidate_name(dir, base, ext, n);

        if !candidate.exists() {
            return Ok((candidate, false));
        }
        let is_mine = candidate
            .canonicalize()
            .ok()
            .and_then(|abs| abs.to_str().map(str::to_string))
            .is_some_and(|abs| mine.contains(&abs));
        if is_mine && source.is_none_or(|src| same_contents(src, &candidate)) {
            return Ok((candidate, true));
        }
    }

    Err(format!(
        "could not find a free filename for '{base}' in {}",
        dir.display()
    ))
}

// Atomically claims a free `<base>.<ext>` / `<base>-2.<ext>` / ... name in
// `dir` via O_EXCL (create_new): checking a name is free and then writing to
// it are two steps, and a second mover can land in between them (measured
// directly, when this raced inside `copy_into_library`: 14 of 60 concurrent
// pairs lost a file outright, each side reporting success). Unlike
// `pick_target`, never reuses an existing file -- callers that want that
// (attach/fetch's "is this already mine?" policy) do that check themselves
// before ever getting here; this only ever hands back a name nothing existed
// at a moment ago.
pub(crate) fn claim_free_name(dir: &Path, base: &str, ext: &str) -> std::io::Result<std::path::PathBuf> {
    const MAX_ATTEMPTS: u32 = 50;
    for n in 1..=MAX_ATTEMPTS {
        let candidate = nth_candidate_name(dir, base, ext, n);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!(
        "could not find a free filename for '{base}' in {}",
        dir.display()
    )))
}

// Length first, so the common "different paper" case never reads either file.
// Unreadable either side counts as different, which costs at worst a redundant
// copy under a new name.
fn same_contents(a: &Path, b: &Path) -> bool {
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(ma), Ok(mb)) if ma.len() == mb.len() => {
            matches!((std::fs::read(a), std::fs::read(b)), (Ok(x), Ok(y)) if x == y)
        }
        _ => false,
    }
}

// `attach` copies the file into ./pdfs/ under the same <cite_key>.<ext> scheme
// `fetch` uses, so a library is one directory of papers rather than a set of
// pointers into wherever each file happened to be downloaded. The original is
// left where it is -- this is a copy, not a move. Returns the stored path and
// whether a new file was actually written.
fn copy_into_library(
    conn: &rusqlite::Connection,
    cite_key: &str,
    source: &Path,
) -> Result<(String, bool), String> {
    let base = doi::sanitize_filename(cite_key)?;
    // Anything unusual becomes "pdf": the extension lands in a filename, and a
    // library of papers is overwhelmingly PDFs anyway.
    let ext = source
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_else(|| "pdf".to_string());

    let root = config::library_root()?;
    let dir = root.join("pdfs");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create '{}': {e}", dir.display()))?;

    // Choosing the name and writing it has to be one indivisible step.
    // `pick_target` decides a name is free by looking, and two attaches racing
    // on the same cite_key both look before either writes: measured at 14 of 60
    // concurrent pairs losing a file outright, each process reporting success.
    // O_EXCL (create_new) makes the claim atomic, so a loser sees the name
    // taken and moves to the next one instead of overwriting.
    for _ in 0..8 {
        let (target, already_there) =
            pdf_target(conn, cite_key, &dir, &base, &ext, Some(source))?;
        if already_there {
            return stored_path(&target, false);
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_) => {
                if let Err(e) = std::fs::copy(source, &target) {
                    // Don't let a failed copy leave an empty file squatting
                    // on the name forever.
                    let _ = std::fs::remove_file(&target);
                    return Err(format!(
                        "failed to copy '{}' to '{}': {e}",
                        source.display(),
                        target.display()
                    ));
                }
                return stored_path(&target, true);
            }
            // Someone claimed it between the look and the open: go round again
            // and pick the next free name.
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to create '{}': {e}", target.display())),
        }
    }

    Err(format!(
        "gave up claiming a filename for '{cite_key}' in {} -- too many attaches at once",
        dir.display()
    ))
}

fn stored_path(target: &Path, copied: bool) -> Result<(String, bool), String> {
    let abs = target
        .canonicalize()
        .map_err(|e| format!("failed to resolve '{}': {e}", target.display()))?;
    abs.to_str()
        .map(str::to_string)
        .map(|s| (s, copied))
        .ok_or_else(|| format!("path {} is not valid UTF-8", abs.display()))
}

// --collection takes a path; Filter takes a resolved id. An unresolvable path
// matches nothing, the same treatment an unknown tag gets.
//
// SENTINEL is a collection id that cannot exist (ids are AUTOINCREMENT and
// positive), so an unknown path filters everything out instead of silently
// behaving like no collection filter at all -- which would print the whole
// library and read like success.
const NO_SUCH_COLLECTION: i64 = -1;

fn resolve_collection_filter(conn: &rusqlite::Connection, path: Option<String>) -> Option<i64> {
    let path = path?;
    match db::collection_by_path(conn, &path) {
        Ok(Some(id)) => Some(id),
        Ok(None) => Some(NO_SUCH_COLLECTION),
        Err(e) => die(&format!("failed to resolve collection '{path}': {e}")),
    }
}

// Saves extracted text and returns its length in characters.
//
// A zero row count means the attachment row is gone (a concurrent `rm`, say).
// Reporting that as a successful extraction would tell a script it has text in
// the DB when nothing was written, so it's an error.
fn save_extracted(conn: &rusqlite::Connection, attachment_id: i64, text: &str) -> Result<usize, String> {
    match db::set_full_text(conn, attachment_id, text) {
        Ok(0) => Err("attachment row no longer exists; nothing was saved".to_string()),
        Ok(_) => Ok(text.chars().count()),
        Err(e) => Err(format!("failed to save extracted text: {e}")),
    }
}

// add_tag/remove_tag/attach report an unknown cite_key as QueryReturnedNoRows, which
// would otherwise reach the user as "Query returned no rows". Every other
// command names the key it couldn't find; these should too.
fn entry_error(cite_key: &str, action: &str, e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::QueryReturnedNoRows => {
            format!("no entry found with cite_key '{cite_key}'")
        }
        e => format!("failed to {action}: {e}"),
    }
}

// "You already have this paper", by either name: the same cite_key (SQLite's
// own UNIQUE constraint) or the same DOI (db::insert_entry's guard, raised as
// InvalidParameterName with a message already written for a human -- see
// reject_duplicate_doi in db.rs). Import counts both as a skip rather than a
// failure, so re-importing a .bib you already hold stays a successful no-op
// even when its keys have drifted.
//
// Checked via the typed error, not by matching SQLite's English constraint
// message: rusqlite exposes the real error code, and a locale/version change
// to that message text shouldn't silently turn "already have this" into
// "corrupt data" for every future import.
fn is_duplicate(e: &rusqlite::Error) -> bool {
    e.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation)
        || matches!(e, rusqlite::Error::InvalidParameterName(msg) if msg.contains("is already on entry"))
}

// Collection functions (create_collection, move_collection, delete_collection)
// report an unknown/invalid path via InvalidParameterName(msg), where msg is
// already a complete, human-readable message -- see db.rs.
fn db_error(action: &str, e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::InvalidParameterName(msg) => msg,
        e => format!("failed to {action}: {e}"),
    }
}

// add_to_collection/remove_from_collection can fail on either an unknown
// cite_key (QueryReturnedNoRows, same as add_tag) or an unknown collection
// path (InvalidParameterName, already human-readable). Combines entry_error
// and db_error's handling.
fn collection_entry_error(cite_key: &str, action: &str, e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::QueryReturnedNoRows => {
            format!("no entry found with cite_key '{cite_key}'")
        }
        rusqlite::Error::InvalidParameterName(msg) => msg,
        e => format!("failed to {action}: {e}"),
    }
}

fn output_entry(entry: &Entry, json: bool) {
    if json {
        emit_json(entry);
    } else {
        emit(&format_entry(entry));
    }
}

fn format_authors(authors: &[Author]) -> String {
    authors
        .iter()
        .map(|a| match &a.first_name {
            Some(first) => format!("{}, {}", a.last_name, first),
            None => a.last_name.clone(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn format_entry(entry: &Entry) -> String {
    let mut out = format!("{} [{}]\n", entry.title, entry.cite_key);
    out.push_str(&format!("  Type: {}\n", entry.entry_type));
    if !entry.authors.is_empty() {
        out.push_str(&format!("  Authors: {}\n", format_authors(&entry.authors)));
    }
    if !entry.tags.is_empty() {
        out.push_str(&format!("  Tags: {}\n", entry.tags.join(", ")));
    }
    if let Some(year) = entry.year {
        out.push_str(&format!("  Year: {year}\n"));
    }
    for (label, value) in [
        ("Journal", &entry.journal),
        ("Volume", &entry.volume),
        ("Pages", &entry.pages),
        ("DOI", &entry.doi),
        ("URL", &entry.url),
        ("Abstract", &entry.abstract_text),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("  {label}: {value}\n"));
        }
    }
    for attachment in &entry.attachments {
        out.push_str(&format!("  Attachment: {}\n", attachment.path));
    }
    out.pop(); // emit() adds the trailing newline
    out
}

fn format_list_line(entry: &Entry) -> String {
    let year = entry
        .year
        .map(|y| y.to_string())
        .unwrap_or_else(|| "----".to_string());
    format!(
        "{:<15} {:<6} {:<50} {}",
        entry.cite_key,
        year,
        entry.title,
        format_authors(&entry.authors)
    )
}

// `search --text`'s result shape. Deliberately not `Entry`: this is a
// bounded result (capped snippets + a true count), not another path that
// can dump a whole library's extracted text -- see the module note on
// `--full-text` for why that distinction matters.
#[derive(Debug, Clone, serde::Serialize)]
struct AttachmentMatch {
    path: String,
    snippets: Vec<String>,
    total_matches: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct TextSearchResult {
    cite_key: String,
    title: String,
    year: Option<i32>,
    matches: Vec<AttachmentMatch>,
}

// Not exposed as CLI flags -- see find_snippets's doc comment on why a
// fixed cap is the point, not a limitation.
const SNIPPET_CONTEXT_BYTES: usize = 50;
const SNIPPET_MAX_MATCHES: usize = 3;

// PDF extraction leaves ugly line-wrapping; collapses any run of whitespace
// (including newlines) in a snippet down to a single space.
fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Finds up to `max_matches` non-overlapping, ASCII-case-insensitive
// occurrences of `query` in `text`, each surrounded by up to
// `context_bytes` bytes of context (rounded outward to a char boundary --
// hence "bytes", not "chars", in the name). Returns the capped snippets
// plus the *true* total occurrence count, so a caller can report "+N more"
// beyond what's shown. ASCII-only case folding matches what the SQL LIKE
// filter upstream already applied, so results stay consistent with it.
//
// `to_ascii_lowercase` only maps 'A'-'Z' and never changes a string's byte
// length or the position of any other byte, so offsets found in the
// lowercased copy are valid offsets into the original `text` -- this is
// what makes slicing `text` (not the lowercased copy) at those offsets
// correct, snippets keep the source's original casing.
fn find_snippets(
    text: &str,
    query: &str,
    context_bytes: usize,
    max_matches: usize,
) -> (Vec<String>, usize) {
    let lower_text = text.to_ascii_lowercase();
    let lower_query = query.to_ascii_lowercase();
    if lower_query.is_empty() {
        return (Vec::new(), 0);
    }

    let mut snippets = Vec::new();
    let mut count = 0;
    let mut cursor = 0;
    while let Some(offset) = lower_text[cursor..].find(&lower_query) {
        let match_start = cursor + offset;
        let match_end = match_start + lower_query.len();
        count += 1;

        if snippets.len() < max_matches {
            let start = text.floor_char_boundary(match_start.saturating_sub(context_bytes));
            let end = text.ceil_char_boundary((match_end + context_bytes).min(text.len()));
            let mut snippet = collapse_whitespace(&text[start..end]);
            if start > 0 {
                snippet = format!("…{snippet}");
            }
            if end < text.len() {
                snippet = format!("{snippet}…");
            }
            snippets.push(snippet);
        }

        cursor = match_end; // non-overlapping: advance past this match
    }

    (snippets, count)
}

// Builds the bounded result set for `search --text` from entries
// list_entries already SQL-filtered to "at least one attachment matches".
// Loads each entry's attachment text one entry at a time (db::
// attachments_for_entry), rather than the caller bulk-loading the whole
// matching set up front -- on a large library that bulk load is hundreds of
// MB resident before a single snippet is cut, the same unbounded-memory
// shape --full-text already needed a --json guard for. Peak here is one
// entry's attachments at a time.
//
// Re-checks per attachment here (not just per entry): a multi-attachment
// entry can have one matching file and one that doesn't, and only the
// matching one belongs in the output. An entry whose SQL match doesn't
// reproduce in Rust (shouldn't happen, but ASCII vs. LIKE case-folding
// could in principle disagree), or whose attachment lookup itself fails
// (shouldn't happen either -- the entry came from this same connection a
// moment ago), is skipped rather than shown empty or panicking.
fn text_search_results(conn: &rusqlite::Connection, entries: &[Entry], query: &str) -> Vec<TextSearchResult> {
    entries
        .iter()
        .filter_map(|entry| {
            let attachments = db::attachments_for_entry(conn, entry.id?, true).ok()?;
            let matches: Vec<AttachmentMatch> = attachments
                .iter()
                .filter_map(|att| {
                    let full_text = att.full_text.as_deref()?;
                    let (snippets, total_matches) =
                        find_snippets(full_text, query, SNIPPET_CONTEXT_BYTES, SNIPPET_MAX_MATCHES);
                    if total_matches == 0 {
                        return None;
                    }
                    Some(AttachmentMatch {
                        path: att.path.clone(),
                        snippets,
                        total_matches,
                    })
                })
                .collect();

            if matches.is_empty() {
                return None;
            }

            Some(TextSearchResult {
                cite_key: entry.cite_key.clone(),
                title: entry.title.clone(),
                year: entry.year,
                matches,
            })
        })
        .collect()
}

fn format_text_search_result(result: &TextSearchResult) -> String {
    let year = result
        .year
        .map(|y| y.to_string())
        .unwrap_or_else(|| "----".to_string());
    let mut out = format!("{:<15}{} ({year})\n", result.cite_key, result.title);
    for m in &result.matches {
        for snippet in &m.snippets {
            out.push_str(&format!("  {snippet}\n"));
        }
        let shown = m.snippets.len();
        if m.total_matches > shown {
            let file_name = Path::new(&m.path)
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| m.path.clone());
            out.push_str(&format!(
                "  (+{} more matches in {file_name})\n",
                m.total_matches - shown
            ));
        }
    }
    out.pop(); // emit() adds the trailing newline
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // The reuse rule: a name is only "already there" when the file at it
    // belongs to this entry AND matches what's being copied in. Getting this
    // wrong drops the second file a user attaches to one entry.
    #[test]
    fn pick_target_only_reuses_an_identical_file_of_ours() {
        let dir = std::env::temp_dir().join("ferref-pick-target-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        let paper = dir.join("paper.pdf");
        std::fs::write(&paper, b"paper bytes").unwrap();

        // Nothing at the name yet: take it, nothing is present.
        let (target, present) = pick_target(&dir, "smith2024", "pdf", &[], Some(&paper)).unwrap();
        assert_eq!(target, dir.join("smith2024.pdf"));
        assert!(!present);

        std::fs::copy(&paper, &target).unwrap();
        let mine = vec![target.canonicalize().unwrap().to_str().unwrap().to_string()];

        // Same file again: reuse the copy already in place.
        let (again, present) =
            pick_target(&dir, "smith2024", "pdf", &mine, Some(&paper)).unwrap();
        assert_eq!(again, target);
        assert!(present);

        // A different file for the same entry must not land on that name.
        let supplement = dir.join("supplement.pdf");
        std::fs::write(&supplement, b"supplement bytes").unwrap();
        let (next, present) =
            pick_target(&dir, "smith2024", "pdf", &mine, Some(&supplement)).unwrap();
        assert_eq!(next, dir.join("smith2024-2.pdf"));
        assert!(!present);

        // Someone else's file at our name is skipped, source or not.
        let (skipped, present) = pick_target(&dir, "smith2024", "pdf", &[], None).unwrap();
        assert_eq!(skipped, dir.join("smith2024-2.pdf"));
        assert!(!present);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn find_snippets_truncates_with_ellipses_only_where_the_window_was_cut() {
        // Long enough on both sides that a 5-byte context window is
        // truncated by the fixed window, not by hitting the text's edge.
        let text = "0123456789needle0123456789";
        let (snippets, count) = find_snippets(text, "needle", 5, 3);
        assert_eq!(count, 1);
        assert_eq!(snippets, vec!["…56789needle01234…".to_string()]);
    }

    #[test]
    fn find_snippets_no_leading_ellipsis_when_match_is_at_the_very_start() {
        let text = "needle0123456789";
        let (snippets, count) = find_snippets(text, "needle", 5, 3);
        assert_eq!(count, 1);
        assert_eq!(snippets, vec!["needle01234…".to_string()]);
    }

    #[test]
    fn find_snippets_no_trailing_ellipsis_when_match_is_at_the_very_end() {
        let text = "0123456789needle";
        let (snippets, count) = find_snippets(text, "needle", 5, 3);
        assert_eq!(count, 1);
        assert_eq!(snippets, vec!["…56789needle".to_string()]);
    }

    #[test]
    fn find_snippets_caps_returned_snippets_but_counts_every_occurrence() {
        let text = "needle needle needle needle needle";
        let (snippets, count) = find_snippets(text, "needle", 3, 2);
        assert_eq!(count, 5, "every non-overlapping occurrence must be counted");
        assert_eq!(snippets.len(), 2, "only max_matches snippets are actually built");
    }

    #[test]
    fn find_snippets_returns_nothing_and_does_not_panic_when_absent() {
        let (snippets, count) = find_snippets("no match in here", "xyz", 10, 3);
        assert!(snippets.is_empty());
        assert_eq!(count, 0);
    }

    // 'é' (2-byte UTF-8) sits adjacent to the match on both sides, with no
    // ASCII byte between it and "match". A 1-byte context window would,
    // without boundary rounding, try to slice inside 'é' on both ends and
    // panic; rounding must widen the window to include the whole character
    // instead.
    #[test]
    fn find_snippets_rounds_the_window_outward_at_a_multibyte_boundary() {
        let text = "aématchéb";
        let (snippets, count) = find_snippets(text, "match", 1, 3);
        assert_eq!(count, 1);
        assert_eq!(snippets, vec!["…ématché…".to_string()]);
    }
}
