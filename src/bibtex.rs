// BibTeX import/export, built on the `biblatex` crate (parses/writes .bib,
// we don't hand-roll any of that). This module only handles the mapping
// between `biblatex::Entry` and our `models::Entry`.
use std::ops::Range;
use std::path::Path;

use biblatex::{
    Bibliography, ChunksExt, Date, DateValue, Datetime, Entry as BibEntry, EntryType,
    PermissiveType, Person, Type,
};

use crate::models::{Author, Entry};

pub fn import(path: &Path) -> Result<Vec<Entry>, String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    parse_bibtex_str(&src)
}

// Factored out of `import` so tests can feed a string directly instead of
// writing a temp file.
fn parse_bibtex_str(src: &str) -> Result<Vec<Entry>, String> {
    let bib = Bibliography::parse(src).map_err(|e| format!("failed to parse BibTeX: {e}"))?;
    Ok(bib.iter().map(from_biblatex).collect())
}

// Two output formats, because they are genuinely two formats and not a
// quality setting. Legacy BibTeX has no `@online` or `@dataset`, so the
// `biblatex` crate downgrades both to `@misc` on its way out, and writes
// `year`/`journal` where BibLaTeX writes `date`/`journaltitle`. Emitting
// BibLaTeX by default would hand a plain-BibTeX pipeline fields its styles
// don't read, so the caller says which one it wants.
pub fn export(entries: &[Entry], biblatex_syntax: bool) -> String {
    let mut bib = Bibliography::new();
    for entry in entries {
        bib.insert(to_biblatex(entry));
    }
    if biblatex_syntax {
        bib.to_biblatex_string()
    } else {
        bib.to_bibtex_string()
    }
}

// --- biblatex::Entry -> our Entry ---------------------------------------

fn from_biblatex(e: &BibEntry) -> Entry {
    let entry_type = entry_type_to_string(&e.entry_type);
    let title = e.title().ok().map(|c| c.format_verbatim()).unwrap_or_default();

    let mut entry = Entry::new(entry_type, e.key.clone(), title);

    if let Ok(persons) = e.author() {
        for p in &persons {
            if let Some(author) = person_to_author(p) {
                entry.add_author(author);
            }
        }
    }

    entry.year = e.date().ok().and_then(date_to_year);
    entry.journal = e.journal().ok().map(|c| c.format_verbatim());
    // Read volume as the raw field text, not through volume()'s
    // PermissiveType<i64>: the crate's i64 parser also accepts Roman
    // numerals, so a volume of "II" comes back as 2. We store a String
    // anyway, so the verbatim text is both simpler and lossless.
    entry.volume = e.get("volume").map(|c| c.format_verbatim());
    entry.pages = e.pages().ok().map(pages_to_string);
    entry.doi = e.doi().ok();
    entry.url = e.url().ok();
    entry.abstract_text = e.abstract_().ok().map(|c| c.format_verbatim());
    entry.tags = e
        .keywords()
        .ok()
        .map(|c| split_keywords(&c.format_verbatim()))
        .unwrap_or_default();

    entry
}

// BibTeX's `keywords` is a free-text field with no agreed separator; comma
// and semicolon are both common in the wild, so both are honoured. The values
// are left as written -- db::insert_entry normalizes them on the way in, the
// same as `ferref tag` does.
fn split_keywords(raw: &str) -> Vec<String> {
    raw.split([',', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

// last_name = prefix + " " + name (trimmed) so "van der Berg" survives as
// one unit, matching what cli::parse_author("van der Berg, Jan") produces.
// An author whose computed last_name is empty is dropped rather than
// inserted with a garbage empty name (Phase 2's authors.last_name is NOT
// NULL but happily accepts "").
fn person_to_author(p: &Person) -> Option<Author> {
    let last_name = format!("{} {}", p.prefix, p.name).trim().to_string();
    if last_name.is_empty() {
        return None;
    }

    let first_name = match (p.given_name.is_empty(), p.suffix.is_empty()) {
        (true, true) => None,
        (_, true) => Some(p.given_name.clone()),
        (_, false) => Some(format!("{}, {}", p.given_name, p.suffix)),
    };

    Some(Author::new(last_name, first_name))
}

fn date_to_year(d: PermissiveType<Date>) -> Option<i32> {
    match d {
        PermissiveType::Typed(date) => Some(match date.value {
            DateValue::At(dt) | DateValue::After(dt) | DateValue::Before(dt) => dt.year,
            DateValue::Between(dt, _) => dt.year,
        }),
        PermissiveType::Chunks(_) => None,
    }
}

// BibTeX convention: "123--145" for a range, a bare number for a single
// page. Non-numeric pages ("e12345", "S4-S9", "in press") arrive as the
// literal-chunk variant and are rendered verbatim, never dropped.
fn pages_to_string(p: PermissiveType<Vec<Range<u32>>>) -> String {
    match p {
        PermissiveType::Typed(ranges) => ranges
            .iter()
            .map(|r| {
                if r.start == r.end {
                    r.start.to_string()
                } else {
                    format!("{}--{}", r.start, r.end)
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
        PermissiveType::Chunks(chunks) => chunks.format_verbatim(),
    }
}

// `EntryType::Unknown` drops the original string on its `Display` impl, so
// it's special-cased here rather than trusting `to_string()`.
fn entry_type_to_string(ty: &EntryType) -> String {
    match ty {
        EntryType::Unknown(s) => s.clone(),
        other => other.to_string(),
    }
}

// --- our Entry -> biblatex::Entry ---------------------------------------

fn to_biblatex(entry: &Entry) -> BibEntry {
    let ty = EntryType::new(&entry.entry_type.to_lowercase());
    let mut e = BibEntry::new(entry.cite_key.clone(), ty);

    e.set_title(entry.title.to_chunks());

    if !entry.authors.is_empty() {
        let persons: Vec<Person> = entry.authors.iter().map(author_to_person).collect();
        e.set_author(persons);
    }

    if let Some(year) = entry.year {
        e.set_date(PermissiveType::Typed(Date {
            value: DateValue::At(Datetime { year, month: None, day: None, time: None }),
            uncertain: false,
            approximate: false,
        }));
    }
    if let Some(journal) = &entry.journal {
        e.set_journal(journal.to_chunks());
    }
    if let Some(volume) = &entry.volume {
        e.set_volume(parse_volume(volume));
    }
    if let Some(pages) = &entry.pages {
        e.set_pages(parse_pages(pages));
    }
    if let Some(doi) = &entry.doi {
        e.set_doi(doi.clone());
    }
    if let Some(url) = &entry.url {
        e.set_url(url.clone());
    }
    if let Some(abstract_text) = &entry.abstract_text {
        e.set_abstract_(abstract_text.to_chunks());
    }
    if !entry.tags.is_empty() {
        e.set_keywords(entry.tags.join(", ").to_chunks());
    }

    e
}

// Our Author only has two name parts; the surname goes whole into `name`
// (not split into prefix/name) -- biblatex's own bibtex-style parser
// re-splits it on re-import, and person_to_author's prefix+name join
// reconstructs the original either way.
//
// `first_name` is split back into given name and suffix on the first comma,
// mirroring person_to_author's "given, suffix" join. Without this, a name
// like {last: "Smith", first: "John, Jr."} serializes as the two-comma
// "Smith, John, Jr.", which BibTeX reads as "Last, Suffix, First" -- so
// John and Jr. come back transposed.
fn author_to_person(a: &Author) -> Person {
    let (given_name, suffix) = match a.first_name.as_deref() {
        Some(first) => match first.split_once(',') {
            Some((given, suffix)) => (given.trim().to_string(), suffix.trim().to_string()),
            None => (first.trim().to_string(), String::new()),
        },
        None => (String::new(), String::new()),
    };

    Person {
        name: a.last_name.clone(),
        given_name,
        prefix: String::new(),
        suffix,
        id: None,
        prefix_initials: None,
        given_initials: None,
        use_prefix: None,
    }
}

fn parse_volume(s: &str) -> PermissiveType<i64> {
    match s.trim().parse::<i64>() {
        Ok(n) => PermissiveType::Typed(n),
        Err(_) => PermissiveType::Chunks(s.to_string().to_chunks()),
    }
}

fn parse_pages(s: &str) -> PermissiveType<Vec<Range<u32>>> {
    let trimmed = s.trim();
    let parts: Vec<&str> = if let Some(idx) = trimmed.find("--") {
        vec![&trimmed[..idx], &trimmed[idx + 2..]]
    } else if trimmed.contains('-') {
        trimmed.splitn(2, '-').collect()
    } else {
        vec![trimmed]
    };

    let numbers: Option<Vec<u32>> = parts.iter().map(|p| p.trim().parse::<u32>().ok()).collect();

    match numbers.as_deref() {
        Some([n]) => PermissiveType::Typed(vec![*n..*n]),
        Some([start, end]) => PermissiveType::Typed(vec![*start..*end]),
        _ => PermissiveType::Chunks(trimmed.to_string().to_chunks()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_traps() {
        let mut entry = Entry::new("article".into(), "berg2020".into(), "A Study".into());
        entry.add_author(Author::new("van der Berg".into(), Some("Jan".into())));
        entry.year = Some(2020);
        entry.journal = Some("Nature".into());
        entry.volume = Some("12A".into());
        entry.pages = Some("123--145".into());
        entry.abstract_text = Some("An abstract about things.".into());

        let bibtex_str = export(std::slice::from_ref(&entry), false);
        let imported = parse_bibtex_str(&bibtex_str).unwrap();
        assert_eq!(imported.len(), 1);
        let round_tripped = &imported[0];

        assert_eq!(round_tripped.cite_key, "berg2020");
        assert_eq!(round_tripped.authors.len(), 1);
        assert_eq!(round_tripped.authors[0].last_name, "van der Berg");
        assert_eq!(round_tripped.authors[0].first_name, Some("Jan".to_string()));
        assert_eq!(round_tripped.year, Some(2020));
        assert_eq!(round_tripped.journal.as_deref(), Some("Nature"));
        assert_eq!(round_tripped.volume.as_deref(), Some("12A"));
        assert_eq!(round_tripped.pages.as_deref(), Some("123--145"));
        assert_eq!(
            round_tripped.abstract_text.as_deref(),
            Some("An abstract about things.")
        );
    }

    #[test]
    fn bibtex_style_journal_and_bare_year() {
        let src = r#"@article{smith2024,
            title = {A Title},
            journal = {Science},
            year = {2024},
        }"#;
        let entries = parse_bibtex_str(src).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].journal.as_deref(), Some("Science"));
        assert_eq!(entries[0].year, Some(2024));
    }

    #[test]
    fn biblatex_style_journaltitle() {
        let src = r#"@article{doe2023,
            title = {Another Title},
            journaltitle = {Cell},
            year = {2023},
        }"#;
        let entries = parse_bibtex_str(src).unwrap();
        assert_eq!(entries[0].journal.as_deref(), Some("Cell"));
    }

    #[test]
    fn non_numeric_pages_survive() {
        let src = r#"@article{ep2022,
            title = {Electronic Paper},
            pages = {e12345},
        }"#;
        let entries = parse_bibtex_str(src).unwrap();
        assert_eq!(entries[0].pages.as_deref(), Some("e12345"));
    }

    #[test]
    fn author_with_empty_name_is_dropped() {
        let src = r#"@article{noname2021,
            title = {No Name},
            author = {Smith, John and , }
        }"#;
        let entries = parse_bibtex_str(src).unwrap();
        assert_eq!(entries[0].authors.len(), 1);
        assert_eq!(entries[0].authors[0].last_name, "Smith");
    }

    #[test]
    fn malformed_bib_is_reported_not_panicked() {
        let result = parse_bibtex_str("@article{unterminated,");
        assert!(result.is_err());
    }

    fn round_trip(entry: Entry) -> Entry {
        let exported = export(&[entry], false);
        parse_bibtex_str(&exported)
            .expect("exported BibTeX should re-parse")
            .pop()
            .expect("round trip should yield an entry")
    }

    // BibTeX reads a two-comma name as "Last, Suffix, First", so folding our
    // suffix into first_name without splitting it back out transposed them:
    // "John, Jr." came back as "Jr., John".
    #[test]
    fn author_suffix_survives_round_trip() {
        let mut entry = Entry::new("article".into(), "k".into(), "T".into());
        entry.add_author(Author::new("Smith".into(), Some("John, Jr.".into())));
        entry.add_author(Author::new("van der Berg".into(), Some("Jan".into())));

        let back = round_trip(entry);
        assert_eq!(back.authors[0].last_name, "Smith");
        assert_eq!(back.authors[0].first_name, Some("John, Jr.".to_string()));
        assert_eq!(back.authors[1].last_name, "van der Berg");
        assert_eq!(back.authors[1].first_name, Some("Jan".to_string()));
    }

    // The two things a BibTeX -> LaTeX pipeline actually loses: tags, which
    // have a home in `keywords`, and BibLaTeX-only entry types, which legacy
    // BibTeX has no slot for and so silently become @misc.
    #[test]
    fn tags_and_biblatex_types_survive_export() {
        let mut entry = Entry::new("online".into(), "web2024".into(), "A Web Thing".into());
        entry.tags = vec!["entropy".into(), "information theory".into()];

        // Legacy BibTeX has no @online, so it downgrades -- expected, and why
        // the flag exists.
        let legacy = export(std::slice::from_ref(&entry), false);
        assert!(legacy.contains("@misc{"), "legacy BibTeX should downgrade: {legacy}");
        assert!(legacy.contains("keywords"), "keywords should still be written: {legacy}");

        // BibLaTeX keeps it.
        let modern = export(std::slice::from_ref(&entry), true);
        assert!(modern.contains("@online{"), "BibLaTeX should keep @online: {modern}");

        // Tags round-trip through `keywords`, in order, either way.
        for exported in [legacy, modern] {
            let back = parse_bibtex_str(&exported).unwrap().pop().unwrap();
            assert_eq!(back.tags, vec!["entropy", "information theory"]);
        }
    }

    // A `keywords` field written by hand may use either separator, and may
    // carry the stray whitespace a human leaves behind.
    #[test]
    fn keywords_split_on_either_separator() {
        assert_eq!(split_keywords("a, b ,c"), vec!["a", "b", "c"]);
        assert_eq!(split_keywords("a; b;;c "), vec!["a", "b", "c"]);
        assert!(split_keywords("   ").is_empty());
    }

    // biblatex's i64 parser also accepts Roman numerals, so reading volume
    // through the typed accessor turned "II" into "2".
    #[test]
    fn roman_numeral_volume_is_not_converted() {
        for volume in ["II", "IV", "12A", "7"] {
            let mut entry = Entry::new("article".into(), "k".into(), "T".into());
            entry.volume = Some(volume.to_string());
            assert_eq!(
                round_trip(entry).volume.as_deref(),
                Some(volume),
                "volume {volume:?} changed across a round trip"
            );
        }
    }
}
