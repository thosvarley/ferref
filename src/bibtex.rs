// BibTeX import/export, built on the `biblatex` crate (parses/writes .bib,
// we don't hand-roll any of that). This module only handles the mapping
// between `biblatex::Entry` and our `models::Entry`.
use std::ops::Range;
use std::path::Path;

use biblatex::{
    Bibliography, ChunksExt, Date, DateValue, Datetime, Entry as BibEntry, EntryType,
    PermissiveType, Person, RawBibliography, RawChunk, RawEntry, Type,
};

use crate::cli::validate_cite_key;
use crate::models::{Author, Entry};

// Entries parsed successfully, and entries rejected during parsing itself
// (currently just a missing/empty title -- see from_biblatex) as
// (cite_key, reason) pairs -- the same shape cmd_import already uses for
// entries rejected at insert time, so the two can be merged into one report.
type ImportResult = (Vec<Entry>, Vec<(String, String)>);

pub fn import(path: &Path) -> Result<ImportResult, String> {
    let src = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read '{}': {e}", path.display()))?;
    parse_bibtex_str(&src)
}

// Factored out of `import` so tests can feed a string directly instead of
// writing a temp file.
fn parse_bibtex_str(src: &str) -> Result<ImportResult, String> {
    // `Bibliography::parse` resolves `crossref`/`xdata` by recursing straight
    // down the reference chain with no cycle check -- a self-reference
    // (`crossref={a}` on entry `a`) or a longer cycle overflows the stack and
    // takes the whole process down with it. Parse to the crate's raw,
    // unresolved representation first and reject a cycle there, before the
    // resolving parse ever runs.
    let raw = RawBibliography::parse(src).map_err(|e| format!("failed to parse BibTeX: {e}"))?;
    check_crossref_cycles(&raw)?;

    let bib = Bibliography::parse(src).map_err(|e| format!("failed to parse BibTeX: {e}"))?;
    let mut entries = Vec::new();
    let mut rejected = Vec::new();
    for e in bib.iter() {
        match from_biblatex(e) {
            Ok(entry) => entries.push(entry),
            Err(reason) => rejected.push((e.key.clone(), reason)),
        }
    }
    Ok((entries, rejected))
}

// Concatenates a raw field's chunks into plain text -- good enough for a
// `crossref`/`xdata` value, which is always a bare key or comma-separated
// list of keys, never rich text worth distinguishing normal text from an
// abbreviation reference.
fn raw_field_text(field: &[biblatex::Spanned<RawChunk>]) -> String {
    field
        .iter()
        .map(|c| match &c.v {
            RawChunk::Normal(s) => *s,
            RawChunk::Abbreviation(s) => *s,
        })
        .collect::<String>()
}

// The keys one entry points at via `crossref` (a single key) and `xdata` (a
// comma-separated list) -- the two fields `Bibliography::parse` recurses
// through without a cycle check.
fn crossref_targets(e: &RawEntry) -> Vec<String> {
    let mut targets = Vec::new();
    for pair in &e.fields {
        match pair.key.v.to_ascii_lowercase().as_str() {
            "crossref" => {
                let text = raw_field_text(&pair.value.v);
                if !text.trim().is_empty() {
                    targets.push(text.trim().to_string());
                }
            }
            "xdata" => {
                for part in raw_field_text(&pair.value.v).split(',') {
                    let part = part.trim();
                    if !part.is_empty() {
                        targets.push(part.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    targets
}

// Detects a self-reference or longer cycle in the `crossref`/`xdata` graph
// before the recursive resolver ever runs. Iterative (not recursive) DFS on
// purpose -- this is exactly the "don't trust recursion depth on untrusted
// input" boundary that's being fixed, so it would be self-defeating to write
// the check itself recursively.
fn check_crossref_cycles(raw: &RawBibliography) -> Result<(), String> {
    use std::collections::HashMap;

    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for e in &raw.entries {
        edges
            .entry(e.v.key.v.to_string())
            .or_default()
            .extend(crossref_targets(&e.v));
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Visiting,
        Done,
    }
    let mut state: HashMap<String, State> = HashMap::new();

    let starts: Vec<String> = edges.keys().cloned().collect();
    for start in starts {
        if state.get(&start) == Some(&State::Done) {
            continue;
        }

        let mut path: Vec<String> = vec![start.clone()];
        let mut stack: Vec<(String, usize)> = vec![(start.clone(), 0)];
        state.insert(start, State::Visiting);

        while let Some((node, idx)) = stack.last().cloned() {
            let children = edges.get(&node).cloned().unwrap_or_default();
            if idx >= children.len() {
                state.insert(node, State::Done);
                path.pop();
                stack.pop();
                continue;
            }
            stack.last_mut().unwrap().1 = idx + 1;

            let child = &children[idx];
            match state.get(child) {
                Some(State::Visiting) => {
                    let mut cycle = path.clone();
                    cycle.push(child.clone());
                    return Err(format!(
                        "crossref/xdata cycle detected: {}",
                        cycle.join(" -> ")
                    ));
                }
                Some(State::Done) => {}
                None => {
                    state.insert(child.clone(), State::Visiting);
                    path.push(child.clone());
                    stack.push((child.clone(), 0));
                }
            }
        }
    }

    Ok(())
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

// A missing (or present-but-empty, post-trim) title used to default to ""
// and import successfully -- inconsistent with every other path that puts a
// title in the DB (`add --title` is required by clap, the TUI's edit
// rejects an empty title outright). Reported as a rejection instead, the
// same "bad data" treatment cmd_import already gives an entry that fails
// db::insert_entry.
fn from_biblatex(e: &BibEntry) -> Result<Entry, String> {
    // A .bib key that fails our validation (blank, whitespace, or a BibTeX
    // delimiter like the comma in `weird key,x`) would round-trip right back
    // out unreadable on the next `export` -- reject it here the same way a
    // missing title already is, rather than importing a key ferref itself
    // can never re-emit correctly.
    validate_cite_key(&e.key).map_err(|msg| format!("entry '{}': {msg}", e.key))?;

    let entry_type = entry_type_to_string(&e.entry_type);
    let title = e
        .title()
        .ok()
        .map(|c| c.format_verbatim())
        .unwrap_or_default();

    if title.trim().is_empty() {
        return Err(format!("entry '{}' has no title", e.key));
    }

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

    Ok(entry)
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
        e.set("author", authors_to_chunks(&entry.authors));
    }

    if let Some(year) = entry.year {
        e.set_date(PermissiveType::Typed(Date {
            value: DateValue::At(Datetime {
                year,
                month: None,
                day: None,
                time: None,
            }),
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

// L2: an author with no first_name is a single-name/organization author
// (e.g. Crossref's "LIGO Scientific Collaboration and Virgo Collaboration"),
// whose whole name lives in `last_name`. biblatex's own `Vec<Person>`
// serializer writes that name as a bare `Chunk::Normal`, so a literal
// " and " or "," inside it (there's no separator convention that could
// avoid this -- an org name is free text) reads back on import as two or
// three authors instead of one.
//
// The fix is the standard BibTeX one: brace-protect the whole name.
// `Chunk::Verbatim` is what makes the writer add that extra brace pair
// (`{{...}}`, see biblatex's `ChunksExt::to_biblatex_string`), and the
// resolver reads anything inside a nested brace back as protected content
// that keyword-splitting (the `Vec<Person>`/`Vec<Chunks>` "and" split) skips
// over -- so it round-trips as one author again. An author that does have a
// first name is unaffected, still built through the normal `Person` path.
fn authors_to_chunks(authors: &[Author]) -> biblatex::Chunks {
    let per_author: Vec<biblatex::Chunks> = authors
        .iter()
        .map(|a| {
            if a.first_name.is_none() {
                vec![biblatex::Spanned::detached(biblatex::Chunk::Verbatim(
                    a.last_name.clone(),
                ))]
            } else {
                vec![author_to_person(a)].to_chunks()
            }
        })
        .collect();
    per_author.to_chunks()
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

    // clippy's `single_range_in_vec_init` fires here and its suggestion is
    // wrong: biblatex's API wants a Vec<Range<u32>> (a page *range* list), and
    // collecting the range would produce a Vec<u32> of every page number in it.
    #[allow(clippy::single_range_in_vec_init)]
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
        let (imported, rejected) = parse_bibtex_str(&bibtex_str).unwrap();
        assert!(rejected.is_empty());
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
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert!(rejected.is_empty());
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
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert!(rejected.is_empty());
        assert_eq!(entries[0].journal.as_deref(), Some("Cell"));
    }

    #[test]
    fn non_numeric_pages_survive() {
        let src = r#"@article{ep2022,
            title = {Electronic Paper},
            pages = {e12345},
        }"#;
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert!(rejected.is_empty());
        assert_eq!(entries[0].pages.as_deref(), Some("e12345"));
    }

    #[test]
    fn author_with_empty_name_is_dropped() {
        let src = r#"@article{noname2021,
            title = {No Name},
            author = {Smith, John and , }
        }"#;
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert!(rejected.is_empty());
        assert_eq!(entries[0].authors.len(), 1);
        assert_eq!(entries[0].authors[0].last_name, "Smith");
    }

    // M3: a key like `weird key,x` would export back out as
    // `@article{weird key,x,`, unreadable by any BibTeX parser -- reject it
    // at import instead of writing it to the DB.
    #[test]
    fn bad_cite_key_is_rejected_but_sibling_entry_still_imports() {
        let src = r#"@article{weird#key2020,
            title = {Bad Key},
        }
        @article{goodkey2021,
            title = {Good Key},
        }"#;
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cite_key, "goodkey2021");
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn malformed_bib_is_reported_not_panicked() {
        let result = parse_bibtex_str("@article{unterminated,");
        assert!(result.is_err());
    }

    // M4: `Bibliography::parse` (the biblatex crate) resolves `crossref` by
    // recursing straight down the chain with no cycle check -- a
    // self-reference used to overflow the stack and crash the process. Must
    // be rejected as a clean error instead.
    #[test]
    fn self_referencing_crossref_is_rejected_cleanly() {
        let src = "@article{a, crossref={a}, title={x}}";
        let err = parse_bibtex_str(src).unwrap_err();
        assert!(err.contains("cycle"), "error was: {err}");
        assert!(err.contains('a'), "error was: {err}");
    }

    #[test]
    fn two_entry_crossref_cycle_is_rejected_cleanly() {
        let src = "@article{a, crossref={b}, title={x}}\n\
                    @article{b, crossref={a}, title={y}}";
        let err = parse_bibtex_str(src).unwrap_err();
        assert!(err.contains("cycle"), "error was: {err}");
    }

    // A normal, non-cyclic crossref (a child inheriting from a parent
    // collection entry) must still import -- the cycle check must not reject
    // the ordinary case it exists alongside.
    #[test]
    fn normal_crossref_still_imports() {
        let src = r#"@inproceedings{child2020,
            crossref = {parent2020},
            title = {A Chapter},
        }
        @proceedings{parent2020,
            title = {The Proceedings},
            year = {2020},
        }"#;
        let (entries, rejected) = parse_bibtex_str(src).unwrap();
        assert!(rejected.is_empty(), "rejected: {rejected:?}");
        assert_eq!(entries.len(), 2);
    }

    // B5: a missing (or empty) title used to default to "" and import
    // successfully -- inconsistent with every other path that requires a
    // real title. It must be rejected instead, without aborting a sibling
    // entry in the same file that does have a title (this codebase's usual
    // per-entry partial-failure rule).
    #[test]
    fn entry_missing_title_is_rejected_but_sibling_entry_still_imports() {
        let src = r#"@article{notitle2021,
            author = {Smith, John},
        }
        @article{blanktitle2021,
            title = {},
        }
        @article{realtitle2021,
            title = {A Real Title},
        }"#;
        let (entries, rejected) = parse_bibtex_str(src).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cite_key, "realtitle2021");

        assert_eq!(rejected.len(), 2);
        let rejected_keys: Vec<&str> = rejected.iter().map(|(k, _)| k.as_str()).collect();
        assert!(rejected_keys.contains(&"notitle2021"));
        assert!(rejected_keys.contains(&"blanktitle2021"));
        for (_, reason) in &rejected {
            assert!(reason.contains("title"));
        }
    }

    fn round_trip(entry: Entry) -> Entry {
        let exported = export(&[entry], false);
        let (mut entries, rejected) =
            parse_bibtex_str(&exported).expect("exported BibTeX should re-parse");
        assert!(rejected.is_empty());
        entries.pop().expect("round trip should yield an entry")
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

    // L2: a single-name/organization author's name can contain " and " or
    // "," as ordinary text (not a separator) -- Crossref's own
    // "LIGO Scientific Collaboration and Virgo Collaboration" is exactly
    // this: one author, no given name. Unbraced, that splits into two
    // authors on re-import; brace-protected, it must come back as one.
    #[test]
    fn organization_author_with_and_survives_round_trip() {
        let mut entry = Entry::new("article".into(), "ligo2016".into(), "T".into());
        entry.add_author(Author::new(
            "LIGO Scientific Collaboration and Virgo Collaboration".into(),
            None,
        ));
        entry.add_author(Author::new("Smith".into(), Some("John".into())));

        let back = round_trip(entry);
        assert_eq!(back.authors.len(), 2);
        assert_eq!(
            back.authors[0].last_name,
            "LIGO Scientific Collaboration and Virgo Collaboration"
        );
        assert_eq!(back.authors[0].first_name, None);
        assert_eq!(back.authors[1].last_name, "Smith");
        assert_eq!(back.authors[1].first_name, Some("John".to_string()));
    }

    // An organization name containing a comma is the other half of the same
    // bug -- BibTeX's plain-Person parser reads an unbraced comma as
    // "Last, First".
    #[test]
    fn organization_author_with_comma_survives_round_trip() {
        let mut entry = Entry::new("article".into(), "org2020".into(), "T".into());
        entry.add_author(Author::new("Some Org, Inc.".into(), None));

        let back = round_trip(entry);
        assert_eq!(back.authors.len(), 1);
        assert_eq!(back.authors[0].last_name, "Some Org, Inc.");
        assert_eq!(back.authors[0].first_name, None);
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
        assert!(
            legacy.contains("@misc{"),
            "legacy BibTeX should downgrade: {legacy}"
        );
        assert!(
            legacy.contains("keywords"),
            "keywords should still be written: {legacy}"
        );

        // BibLaTeX keeps it.
        let modern = export(std::slice::from_ref(&entry), true);
        assert!(
            modern.contains("@online{"),
            "BibLaTeX should keep @online: {modern}"
        );

        // Tags round-trip through `keywords`, in order, either way.
        for exported in [legacy, modern] {
            let (mut entries, rejected) = parse_bibtex_str(&exported).unwrap();
            assert!(rejected.is_empty());
            let back = entries.pop().unwrap();
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
