use std::path::PathBuf;

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
