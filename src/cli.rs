use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

use crate::models::Author;

#[derive(Parser)]
#[command(name = "ferref", about = "A CLI reference manager")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Add a new entry
    Add {
        #[arg(long = "type")]
        entry_type: String,
        #[arg(long = "key")]
        cite_key: String,
        #[arg(long)]
        title: String,
        /// Repeatable, each as "Last, First"
        #[arg(long = "author")]
        authors: Vec<String>,
        #[arg(long)]
        year: Option<i32>,
        #[arg(long)]
        journal: Option<String>,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        pages: Option<String>,
        #[arg(long)]
        doi: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "abstract")]
        abstract_text: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List all entries
    List {
        /// Exact tag name (case/whitespace-insensitive)
        #[arg(long)]
        tag: Option<String>,
        /// Include each attachment's extracted text (off by default: pulls
        /// every extracted PDF's text into memory otherwise)
        #[arg(long = "full-text")]
        full_text: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a single entry
    Show {
        cite_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Edit an existing entry; only the flags passed are changed
    Edit {
        cite_key: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        year: Option<i32>,
        #[arg(long)]
        journal: Option<String>,
        #[arg(long)]
        volume: Option<String>,
        #[arg(long)]
        pages: Option<String>,
        #[arg(long)]
        doi: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long = "abstract")]
        abstract_text: Option<String>,
        /// Passing this one or more times replaces the whole author list
        #[arg(long = "author")]
        authors: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove an entry
    Rm {
        cite_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Add a tag to an entry. Idempotent -- tagging twice is not an error.
    Tag {
        cite_key: String,
        tag: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a tag from an entry. Idempotent -- untagging twice is not an error.
    Untag {
        cite_key: String,
        tag: String,
        #[arg(long)]
        json: bool,
    },
    /// Search entries. Filters combine with AND; substring matches are
    /// case-insensitive for ASCII.
    Search {
        /// Substring of an author's first or last name
        #[arg(long)]
        author: Option<String>,
        /// Substring of the title
        #[arg(long)]
        title: Option<String>,
        /// Exact year; shorthand for --from Y --to Y
        #[arg(long)]
        year: Option<i32>,
        /// Earliest year, inclusive
        #[arg(long)]
        from: Option<i32>,
        /// Latest year, inclusive
        #[arg(long)]
        to: Option<i32>,
        /// Exact tag name (case/whitespace-insensitive)
        #[arg(long)]
        tag: Option<String>,
        /// Include each attachment's extracted text (off by default: pulls
        /// every extracted PDF's text into memory otherwise)
        #[arg(long = "full-text")]
        full_text: bool,
        #[arg(long)]
        json: bool,
    },
    /// Record a file path against an entry. The file is not copied or moved;
    /// only its path is stored.
    Attach {
        cite_key: String,
        path: PathBuf,
        /// Extract text immediately after attaching
        #[arg(long)]
        extract: bool,
        #[arg(long)]
        json: bool,
    },
    /// (Re)extract full text for every attachment of an entry
    Extract {
        cite_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Open an entry's attachments in the system default application
    Open {
        cite_key: String,
        #[arg(long)]
        json: bool,
    },
    /// Import entries from a BibTeX (.bib) file. Duplicate cite_keys are
    /// skipped, not fatal -- re-importing an overlapping file is normal.
    Import {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Export all entries as BibTeX. Prints to stdout unless --out is given.
    /// No --json here: the output format is BibTeX by definition, not a
    /// choice of representation, so the usual --json rule doesn't apply.
    Export {
        #[arg(long = "out")]
        out: Option<PathBuf>,
    },
}

// "Last, First" -> Author. Splits on the first comma only (names can contain
// more); no comma means the whole string is the last name.
//
// Rejects an empty last name rather than writing it: `authors.last_name` is
// NOT NULL, but NOT NULL still accepts "", so inputs like "" or ",John" would
// otherwise persist a nameless author with no error.
pub fn parse_author(raw: &str) -> Result<Author, String> {
    let (last, first) = match raw.split_once(',') {
        Some((last, first)) => (last.trim(), first.trim()),
        None => (raw.trim(), ""),
    };

    if last.is_empty() {
        return Err(format!(
            "author {raw:?} has no last name (expected \"Last, First\")"
        ));
    }

    let first_name = if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    };
    Ok(Author::new(last.to_string(), first_name))
}

// Attachment paths are stored absolute: the DB outlives any particular working
// directory, and a relative path silently stops resolving after one `cd`.
//
// canonicalize also fails on a path that doesn't exist, which is the point —
// an attach that can't be opened later is almost always a typo, and rejecting
// it now beats discovering it at `open` time. The cost is that a file on an
// unmounted drive can't be pre-registered.
pub fn resolve_attachment_path(path: &Path) -> Result<String, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("cannot attach {}: {e}", path.display()))?;

    abs.to_str()
        .map(|s| s.to_string())
        // SQLite text is UTF-8; a non-UTF-8 path can't round-trip through it.
        .ok_or_else(|| format!("path {} is not valid UTF-8", abs.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_author_last_and_first() {
        let a = parse_author("Smith, John").unwrap();
        assert_eq!(a.last_name, "Smith");
        assert_eq!(a.first_name, Some("John".to_string()));
    }

    #[test]
    fn parse_author_last_only() {
        let a = parse_author("Smith").unwrap();
        assert_eq!(a.last_name, "Smith");
        assert_eq!(a.first_name, None);
    }

    #[test]
    fn parse_author_trailing_comma_is_no_first_name() {
        let a = parse_author("Smith,").unwrap();
        assert_eq!(a.last_name, "Smith");
        assert_eq!(a.first_name, None);
    }

    #[test]
    fn parse_author_splits_on_first_comma_only() {
        let a = parse_author("Smith, John, Jr.").unwrap();
        assert_eq!(a.last_name, "Smith");
        assert_eq!(a.first_name, Some("John, Jr.".to_string()));
    }

    #[test]
    fn parse_author_trims_whitespace() {
        let a = parse_author("  Smith  ,  John  ").unwrap();
        assert_eq!(a.last_name, "Smith");
        assert_eq!(a.first_name, Some("John".to_string()));
    }

    #[test]
    fn resolve_attachment_path_is_absolute_and_rejects_missing() {
        let dir = std::env::temp_dir().join("ferref-attach-test");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("paper.pdf");
        std::fs::write(&file, b"x").unwrap();

        let resolved = resolve_attachment_path(&file).unwrap();
        assert!(Path::new(&resolved).is_absolute());

        assert!(resolve_attachment_path(&dir.join("nope.pdf")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    // An empty last_name satisfies the NOT NULL column but is garbage data,
    // so it has to be rejected before it reaches the DB.
    #[test]
    fn parse_author_rejects_empty_last_name() {
        for raw in ["", "   ", ",", ",John", "  ,  John  "] {
            assert!(
                parse_author(raw).is_err(),
                "expected {raw:?} to be rejected"
            );
        }
    }
}
