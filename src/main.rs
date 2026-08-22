mod bibtex;
mod cli;
mod db;
mod models;

use std::path::Path;

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
            let mut entry = Entry::new(entry_type, cite_key, title);
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
            entry.doi = doi;
            entry.url = url;
            entry.abstract_text = abstract_text;

            match db::insert_entry(&conn, &entry) {
                Ok(id) => {
                    entry.id = Some(id);
                    output_entry(&entry, json);
                }
                Err(e) => die(&format!("failed to add entry: {e}")),
            }
        }

        Command::List { tag, json } => match db::list_entries(
            &conn,
            &db::Filter {
                tag,
                ..Default::default()
            },
        ) {
            Ok(entries) => {
                if json {
                    emit(&serde_json::to_string_pretty(&entries).unwrap());
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
                die(&format!("failed to update entry: {e}"));
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
                emit(&serde_json::to_string_pretty(&out).unwrap());
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
                        emit(&serde_json::to_string_pretty(&out).unwrap());
                    } else if changed {
                        emit(&format!("Tagged '{cite_key}' with '{normalized}'"));
                    } else {
                        emit(&format!("'{cite_key}' already tagged '{normalized}'"));
                    }
                }
                Err(e) => die(&tag_error(&cite_key, "tag", e)),
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
                        emit(&serde_json::to_string_pretty(&out).unwrap());
                    } else if changed {
                        emit(&format!("Untagged '{cite_key}' from '{normalized}'"));
                    } else {
                        emit(&format!("'{cite_key}' was not tagged '{normalized}'"));
                    }
                }
                Err(e) => die(&tag_error(&cite_key, "untag", e)),
            }
        }

        Command::Search {
            author,
            title,
            year,
            from,
            to,
            tag,
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
            };

            match db::list_entries(&conn, &filter) {
                Ok(entries) => {
                    if json {
                        emit(&serde_json::to_string_pretty(&entries).unwrap());
                    } else {
                        for entry in &entries {
                            emit(&format_list_line(entry));
                        }
                    }
                }
                Err(e) => die(&format!("failed to search entries: {e}")),
            }
        }

        Command::Import { path, json } => {
            let entries = match bibtex::import(&path) {
                Ok(e) => e,
                Err(e) => die(&format!("failed to import '{}': {e}", path.display())),
            };

            let mut imported = Vec::new();
            let mut skipped = Vec::new();
            let mut rejected: Vec<(String, String)> = Vec::new();

            for entry in &entries {
                match db::insert_entry(&conn, entry) {
                    Ok(_) => imported.push(entry.cite_key.clone()),
                    Err(e) => {
                        let msg = e.to_string();
                        // SQLite's UNIQUE constraint on cite_key is the only
                        // expected failure mode; anything else is bad data.
                        if msg.contains("UNIQUE constraint") {
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
                emit(&serde_json::to_string_pretty(&out).unwrap());
            } else {
                emit(&format!(
                    "imported {}, skipped {} (duplicate cite_key), rejected {} (bad data)",
                    imported.len(),
                    skipped.len(),
                    rejected.len()
                ));
            }

            if failed {
                std::process::exit(1);
            }
        }

        Command::Export { out } => {
            let entries = match db::list_entries(&conn, &db::Filter::default()) {
                Ok(e) => e,
                Err(e) => die(&format!("failed to list entries: {e}")),
            };
            let bibtex_str = bibtex::export(&entries);

            match out {
                Some(path) => {
                    if let Err(e) = std::fs::write(&path, &bibtex_str) {
                        die(&format!("failed to write '{}': {e}", path.display()));
                    }
                }
                None => emit(bibtex_str.trim_end()),
            }
        }
    }
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

// add_tag/remove_tag report an unknown cite_key as QueryReturnedNoRows, which
// would otherwise reach the user as "Query returned no rows". Every other
// command names the key it couldn't find; these should too.
fn tag_error(cite_key: &str, verb: &str, e: rusqlite::Error) -> String {
    match e {
        rusqlite::Error::QueryReturnedNoRows => {
            format!("no entry found with cite_key '{cite_key}'")
        }
        e => format!("failed to {verb} entry: {e}"),
    }
}

fn output_entry(entry: &Entry, json: bool) {
    if json {
        emit(&serde_json::to_string_pretty(entry).unwrap());
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
