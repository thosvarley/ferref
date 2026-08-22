// APA and MLA formatting as plain string templates over the fields already on
// Entry. Deliberately not a CSL engine (see DESIGN.md Phase 9): two fixed
// styles don't justify one, and the moment a third style or real et-al/ordinal
// correctness is wanted, the answer is `hayagriva`, not more code here.
//
// Everything is best-effort over incomplete data -- a library full of entries
// missing a journal or a year is normal, and a citation with a gap in it beats
// an error.

use crate::models::{Author, Entry};

pub enum Style {
    Apa,
    Mla,
}

pub fn format(entry: &Entry, style: &Style) -> String {
    match style {
        Style::Apa => format_apa(entry),
        Style::Mla => format_mla(entry),
    }
}

// "John Ronald" -> "J. R."; already-initialised "P. C." stays "P. C."
fn initials(first_name: &str) -> String {
    first_name
        .split_whitespace()
        .filter_map(|part| part.chars().next())
        .map(|c| format!("{c}."))
        .collect::<Vec<_>>()
        .join(" ")
}

fn apa_name(author: &Author) -> String {
    match &author.first_name {
        Some(first) if !first.trim().is_empty() => {
            format!("{}, {}", author.last_name, initials(first))
        }
        _ => author.last_name.clone(),
    }
}

// APA joins with commas and an ampersand before the last name.
fn apa_authors(authors: &[Author]) -> String {
    let names: Vec<String> = authors.iter().map(apa_name).collect();
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        2 => format!("{}, & {}", names[0], names[1]),
        _ => {
            let (last, rest) = names.split_last().unwrap();
            format!("{}, & {}", rest.join(", "), last)
        }
    }
}

pub fn format_apa(entry: &Entry) -> String {
    let mut out = String::new();

    let authors = apa_authors(&entry.authors);
    if !authors.is_empty() {
        out.push_str(&authors);
        out.push(' ');
    }

    // "n.d." is APA's own marker for an undated work, not a placeholder we
    // invented, so an entry with no year still produces a valid citation.
    match entry.year {
        Some(year) => out.push_str(&format!("({year}). ")),
        None => out.push_str("(n.d.). "),
    }

    out.push_str(entry.title.trim_end_matches('.'));
    out.push_str(". ");

    if let Some(journal) = &entry.journal {
        out.push_str(journal);
        if let Some(volume) = &entry.volume {
            out.push_str(&format!(", {volume}"));
        }
        if let Some(pages) = &entry.pages {
            out.push_str(&format!(", {}", en_dash(pages)));
        }
        out.push_str(". ");
    }

    if let Some(link) = doi_or_url(entry) {
        out.push_str(&link);
    }

    out.trim_end().to_string()
}

// MLA inverts only the first author; the rest read forename-first, and three
// or more collapse to "et al."
fn mla_authors(authors: &[Author]) -> String {
    let full = |a: &Author| match &a.first_name {
        Some(first) if !first.trim().is_empty() => format!("{} {}", first, a.last_name),
        _ => a.last_name.clone(),
    };
    let inverted = |a: &Author| match &a.first_name {
        Some(first) if !first.trim().is_empty() => format!("{}, {}", a.last_name, first),
        _ => a.last_name.clone(),
    };

    match authors.len() {
        0 => String::new(),
        1 => inverted(&authors[0]),
        2 => format!("{}, and {}", inverted(&authors[0]), full(&authors[1])),
        _ => format!("{}, et al.", inverted(&authors[0])),
    }
}

pub fn format_mla(entry: &Entry) -> String {
    let mut out = String::new();

    let authors = mla_authors(&entry.authors);
    if !authors.is_empty() {
        out.push_str(&authors);
        // "et al." and any name ending in an initial already carry the period;
        // appending another gives "et al..".
        if !authors.ends_with('.') {
            out.push('.');
        }
        out.push(' ');
    }

    out.push_str(&format!("\"{}.\" ", entry.title.trim_end_matches('.')));

    if let Some(journal) = &entry.journal {
        out.push_str(journal);
        if let Some(volume) = &entry.volume {
            out.push_str(&format!(", vol. {volume}"));
        }
        if let Some(year) = entry.year {
            out.push_str(&format!(", {year}"));
        }
        if let Some(pages) = &entry.pages {
            out.push_str(&format!(", pp. {pages}"));
        }
        out.push_str(". ");
    } else if let Some(year) = entry.year {
        out.push_str(&format!("{year}. "));
    }

    if let Some(link) = doi_or_url(entry) {
        out.push_str(&link);
    }

    out.trim_end().to_string()
}

fn doi_or_url(entry: &Entry) -> Option<String> {
    if let Some(doi) = &entry.doi {
        let doi = doi.trim();
        if !doi.is_empty() {
            // A DOI may already be stored as a full URL.
            return Some(if doi.starts_with("http") {
                doi.to_string()
            } else {
                format!("https://doi.org/{doi}")
            });
        }
    }
    entry.url.clone().filter(|u| !u.trim().is_empty())
}

// APA sets page ranges with an en dash. Only a plain "12-34" is converted --
// "e12345" or "S1-S9" are left alone rather than guessed at.
fn en_dash(pages: &str) -> String {
    let (a, b) = match pages.split_once('-') {
        Some(pair) => pair,
        None => return pages.to_string(),
    };
    if !a.is_empty()
        && !b.is_empty()
        && a.chars().all(|c| c.is_ascii_digit())
        && b.chars().all(|c| c.is_ascii_digit())
    {
        format!("{a}\u{2013}{b}")
    } else {
        pages.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_entry() -> Entry {
        let mut e = Entry::new("article".into(), "kucsko2013".into(), "Nanometre-scale thermometry in a living cell".into());
        e.add_author(Author::new("Kucsko".into(), Some("G.".into())));
        e.add_author(Author::new("Maurer".into(), Some("P. C.".into())));
        e.add_author(Author::new("Yao".into(), Some("Norman Y.".into())));
        e.year = Some(2013);
        e.journal = Some("Nature".into());
        e.volume = Some("500".into());
        e.pages = Some("54-58".into());
        e.doi = Some("10.1038/nature12373".into());
        e
    }

    #[test]
    fn apa_and_mla_over_a_complete_entry() {
        let e = full_entry();

        assert_eq!(
            format_apa(&e),
            "Kucsko, G., Maurer, P. C., & Yao, N. Y. (2013). \
             Nanometre-scale thermometry in a living cell. Nature, 500, 54\u{2013}58. \
             https://doi.org/10.1038/nature12373"
        );

        // MLA inverts only the first author and collapses 3+ to et al.
        assert_eq!(
            format_mla(&e),
            "Kucsko, G., et al. \"Nanometre-scale thermometry in a living cell.\" \
             Nature, vol. 500, 2013, pp. 54-58. https://doi.org/10.1038/nature12373"
        );
    }

    // A library full of half-complete entries is the normal case, so missing
    // fields must degrade rather than produce a mangled string.
    #[test]
    fn sparse_entries_degrade_instead_of_breaking() {
        let bare = Entry::new("misc".into(), "x".into(), "Untitled Work".into());
        assert_eq!(format_apa(&bare), "(n.d.). Untitled Work.");
        assert_eq!(format_mla(&bare), "\"Untitled Work.\"");

        let mut two = Entry::new("article".into(), "y".into(), "A Study".into());
        two.add_author(Author::new("Smith".into(), Some("John".into())));
        two.add_author(Author::new("Doe".into(), None));
        two.year = Some(2020);
        assert_eq!(format_apa(&two), "Smith, J., & Doe (2020). A Study.");
        assert_eq!(format_mla(&two), "Smith, John, and Doe. \"A Study.\" 2020.");

        // A trailing initial must not produce a doubled period.
        let mut initialled = Entry::new("article".into(), "w".into(), "Work".into());
        initialled.add_author(Author::new("Kucsko".into(), Some("G.".into())));
        assert_eq!(format_mla(&initialled), "Kucsko, G. \"Work.\"");
    }

    #[test]
    fn page_ranges_and_links() {
        assert_eq!(en_dash("54-58"), "54\u{2013}58");
        assert_eq!(en_dash("e12345"), "e12345");
        assert_eq!(en_dash("S1-S9"), "S1-S9");

        let mut e = Entry::new("misc".into(), "z".into(), "T".into());
        assert_eq!(doi_or_url(&e), None);
        e.url = Some("https://example.org/a".into());
        assert_eq!(doi_or_url(&e).as_deref(), Some("https://example.org/a"));
        // A DOI wins over a URL, and a bare DOI gets the resolver prefix.
        e.doi = Some("10.1/x".into());
        assert_eq!(doi_or_url(&e).as_deref(), Some("https://doi.org/10.1/x"));
        // A DOI already stored as a URL isn't double-prefixed.
        e.doi = Some("https://doi.org/10.1/x".into());
        assert_eq!(doi_or_url(&e).as_deref(), Some("https://doi.org/10.1/x"));
    }
}
