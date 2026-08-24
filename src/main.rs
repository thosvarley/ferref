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

    let conn = match db::init_db(Path::new("ferref.db")) {
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
            url,
            abstract_text,
            json,
        } => {
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
                entry.cite_key = match cite_key {
                    Some(key) => key,
                    None => match derive_cite_key(&conn, &entry) {
                        Ok(key) => key,
                        Err(e) => die(&e),
                    },
                };
                entry
            } else {
                // clap's required_unless_present="doi" guarantees these are
                // Some when --doi wasn't passed.
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
                Ok(id) => {
                    entry.id = Some(id);
                    output_entry(&entry, json);
                }
                Err(e) => die(&db_error("add entry", e)),
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
            json,
        } => {
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
            };

            match db::list_entries(&conn, &filter, full_text) {
                Ok(entries) => {
                    if json {
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
                    let msg = e.to_string();
                    // Already-held rows are the only expected failure
                    // mode; anything else is bad data.
                    if is_duplicate(&msg) {
                        skipped.push(entry.cite_key.clone());
                    } else {
                        rejected.push((entry.cite_key.clone(), msg));
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

fn cmd_fetch(
    conn: &rusqlite::Connection,
    cite_key: String,
    email: Option<String>,
    json: bool,
) {
        let entry = match db::get_entry(conn, &cite_key) {
            Ok(Some(e)) => e,
            Ok(None) => die(&format!("no entry found with cite_key '{cite_key}'")),
            Err(e) => die(&format!("failed to fetch entry: {e}")),
        };
        let Some(doi_value) = entry.doi.clone() else {
            die(&format!(
                "'{cite_key}' has no DOI on record; set one with `ferref edit {cite_key} --doi <doi>`"
            ));
        };

        let resolved_email = match config::resolve_email(email) {
            Ok(e) => e,
            Err(e) => die(&e),
        };

        let oa = match doi::fetch_oa_pdf_url(&doi_value, &resolved_email) {
            Ok(status) => status,
            Err(e) => die(&format!("failed to query Unpaywall: {e}")),
        };

        // Having no PDF to fetch is a normal, legitimate answer -- exit 0,
        // not an error. But "open access with no direct PDF link" and "not
        // open access" are different facts, and telling a user their OA
        // paper isn't OA would send them looking for the wrong thing.
        let Some(pdf_url) = oa.pdf_url else {
            if json {
                let out = serde_json::json!({
                    "cite_key": cite_key,
                    "doi": doi_value,
                    "oa_found": false,
                    "is_oa": oa.is_oa,
                });
                emit_json(&out);
            } else if oa.is_oa {
                emit(&format!(
                    "'{cite_key}' (DOI {doi_value}) is open access, but Unpaywall \
                     has no direct PDF link for it -- only landing pages"
                ));
            } else {
                emit(&format!(
                    "No open-access copy found for '{cite_key}' (DOI {doi_value})"
                ));
            }
            return;
        };

        let filename = match doi::sanitize_filename(&cite_key) {
            Ok(f) => f,
            Err(e) => die(&e),
        };
        let pdf_dir = Path::new("pdfs");
        if let Err(e) = std::fs::create_dir_all(pdf_dir) {
            die(&format!("failed to create '{}': {e}", pdf_dir.display()));
        }
        let (target, already_present) =
            match pdf_target(conn, &cite_key, pdf_dir, &filename, "pdf", None) {
                Ok(t) => t,
                Err(e) => die(&e),
            };

        if !already_present {
            let bytes = match doi::download_pdf(&pdf_url) {
                Ok(b) => b,
                Err(e) => die(&format!("failed to download PDF: {e}")),
            };
            if let Err(e) = std::fs::write(&target, &bytes) {
                die(&format!("failed to save PDF to '{}': {e}", target.display()));
            }
        }

        let abs_path = match target.canonicalize() {
            Ok(p) => p,
            Err(e) => die(&format!(
                "failed to resolve saved PDF path '{}': {e}",
                target.display()
            )),
        };
        let path_str = match abs_path.to_str() {
            Some(s) => s.to_string(),
            None => die(&format!("path {} is not valid UTF-8", abs_path.display())),
        };

        let (attachment_id, _changed) = match db::attach(conn, &cite_key, &path_str) {
            Ok(pair) => pair,
            Err(e) => {
                // Don't leave a PDF on disk that nothing in the library
                // points at. Only remove what this run downloaded.
                if !already_present {
                    let _ = std::fs::remove_file(&target);
                }
                die(&entry_error(&cite_key, "attach downloaded PDF", e))
            }
        };

        // Partial failure: the attachment persists even if extraction
        // fails -- same rule as `attach --extract` (Phase 7).
        let extraction: Result<usize, String> = text::extract_text(&abs_path)
            .and_then(|extracted| save_extracted(conn, attachment_id, &extracted));

        if json {
            let mut out = serde_json::json!({
                "cite_key": cite_key,
                "doi": doi_value,
                "oa_found": true,
                "path": path_str,
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
                "Downloaded open-access PDF for '{cite_key}' to '{path_str}'"
            ));
            match &extraction {
                Ok(chars) => emit(&format!("Extracted {chars} characters from '{path_str}'")),
                Err(e) => emit(&format!("Warning: extraction failed for '{path_str}': {e}")),
            }
        }

        if extraction.is_err() {
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
        let candidate = if n == 1 {
            dir.join(format!("{base}.{ext}"))
        } else {
            dir.join(format!("{base}-{n}.{ext}"))
        };

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

    let dir = Path::new("pdfs");
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create '{}': {e}", dir.display()))?;

    // Choosing the name and writing it has to be one indivisible step.
    // `pick_target` decides a name is free by looking, and two attaches racing
    // on the same cite_key both look before either writes: measured at 14 of 60
    // concurrent pairs losing a file outright, each process reporting success.
    // O_EXCL (create_new) makes the claim atomic, so a loser sees the name
    // taken and moves to the next one instead of overwriting.
    for _ in 0..8 {
        let (target, already_there) =
            pdf_target(conn, cite_key, dir, &base, &ext, Some(source))?;
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
// own UNIQUE constraint) or the same DOI (db::insert_entry's guard). Import
// counts both as a skip rather than a failure, so re-importing a .bib you
// already hold stays a successful no-op even when its keys have drifted.
fn is_duplicate(msg: &str) -> bool {
    msg.contains("UNIQUE constraint") || msg.contains("is already on entry")
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
}
