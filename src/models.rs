// Current Unix timestamp in seconds. Shared by Entry::new and db::update_entry
// so "modified" times are stamped the same way everywhere.
pub fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// A struct is like a blueprint for data: it defines what fields a value has
// and their types. #[derive(...)] is an "attribute" -- it generates code for
// you. Debug lets us print the struct with {:?}; Clone lets us copy it;
// Serialize is what makes `--json` work.
//
// full_text is always serialized (never skip_serializing_if): a script must
// be able to tell "not extracted yet" (null) from "this field isn't carried
// at all", and null is how it tells.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Attachment {
    pub path: String,
    pub full_text: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Entry {
    // Option<T> means "this might not exist"
    // Option is an enum with two variants: Some(value) or None
    // We use Option<i64> for id because new entries don't have an ID yet
    // i64 is a 64-bit integer (matches SQLite's INTEGER type)
    pub id: Option<i64>,
    
    // String is Rust's owned string type
    // "Owned" means this struct owns the data and is responsible for cleaning it up
    pub entry_type: String,
    pub cite_key: String,
    pub title: String,
    
    // Vec<T> is a "vector" - a growable array/list
    // Vec<Author> means "a list of Author structs"
    pub authors: Vec<Author>,
    pub tags: Vec<String>,
    pub attachments: Vec<Attachment>,

    // Optional fields use Option<T>
    // i32 is a 32-bit integer (good enough for years)
    pub year: Option<i32>,
    
    // Option<String> means "might be Some(string) or None"
    pub journal: Option<String>,
    pub volume: Option<String>,
    pub pages: Option<String>,
    pub doi: Option<String>,
    pub url: Option<String>,
    #[serde(rename = "abstract")]
    pub abstract_text: Option<String>,
    
    // Timestamps stored as Unix epoch (seconds since Jan 1, 1970)
    // i64 can hold very large numbers
    pub date_added: i64,
    pub date_modified: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Author {
    // first_name is optional (some citations only have last names)
    pub first_name: Option<String>,
    // last_name is required (always Some, never None)
    pub last_name: String,
}

// 'impl' means "implementation" - this is where we add methods
// Methods are functions that belong to a struct
impl Entry {
    // 'new' is a Rust convention for constructor functions
    // This is an "associated function" (called with Entry::new, not entry.new)
    // It takes parameters and returns a new Entry
    pub fn new(
        entry_type: String,
        cite_key: String,
        title: String,
    ) -> Self {
        // Self refers to Entry (the type we're implementing for)
        // This creates and returns a new Entry with sensible defaults
        
        // Unix timestamp in seconds (SQLite uses signed integers)
        let now = now();

        // Return a new Entry struct
        // If a field name matches a variable name, you can use shorthand
        // 'title' is short for 'title: title'
        Self {
            id: None,  // No ID yet - will be assigned by database
            entry_type,
            cite_key,
            title,
            authors: Vec::new(),  // Empty vector of authors
            tags: Vec::new(),
            attachments: Vec::new(),
            year: None,
            journal: None,
            volume: None,
            pages: None,
            doi: None,
            url: None,
            abstract_text: None,
            date_added: now,
            date_modified: now,
        }
    }
    
    // A regular method (takes &self, so called on an instance)
    // &self means "borrow the instance, don't take ownership"
    // This lets us use the Entry after calling this method
    pub fn add_author(&mut self, author: Author) {
        // &mut self means "mutable reference" - we can modify the Entry
        // .push() adds an item to the end of the vector
        self.authors.push(author);
    }
}

// Implementation block for Author
impl Author {
    // Constructor for Author
    // last_name is required (String), first_name is optional (Option<String>)
    pub fn new(last_name: String, first_name: Option<String>) -> Self {
        Self {
            first_name,
            last_name,
        }
    }
}

// Locks the JSON API surface: "abstract" (not "abstract_text") is the field
// name external scripts key against, and id/cite_key must always be present.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_json_uses_abstract_key() {
        let mut entry = Entry::new("article".into(), "smith2024".into(), "Title".into());
        entry.id = Some(1);
        let json = serde_json::to_string(&entry).unwrap();

        assert!(json.contains("\"abstract\""));
        assert!(!json.contains("abstract_text"));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"cite_key\":\"smith2024\""));
    }
}
