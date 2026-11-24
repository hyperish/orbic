//! Fixture crate for orbic's pipeline tests (not published). It plays an
//! app's shared crate: models plus `#[server]` functions with its own
//! `server` feature. CI tests it with and without the feature.

#![allow(missing_docs)]

use hyve::prelude::*;
use orbic::prelude::*;

/// A user of the app (the docs' sketches leave it to the app).
#[model]
#[derive(Debug, Clone, PartialEq)]
pub struct User {
    pub id: Id<User>,
    pub name: String,
}

/// `docs/01-dx-and-dsl.md` §1 sketch.
#[model]
#[table("projects")]
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    #[pk]
    pub id: Id<Project>,
    pub owner_id: Id<User>,
    #[validate(len = 1..=80)]
    pub title: String,
    pub is_public: bool,
    pub version: i64,
}

/// Every column type, for round trips on both dialects.
#[model]
#[table("events")]
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    #[pk]
    pub id: Id<Event>,
    pub at: Timestamp,
    pub score: Option<f64>,
    pub payload: Vec<u8>,
    pub flag: Option<bool>,
}

#[model]
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    #[validate(len = 1..=20)]
    pub text: String,
    pub pinned: bool,
}

#[model]
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NoteFilter {
    pub pinned: Option<bool>,
    pub limit: Option<u32>,
}

/// Server-only state, like a database handle.
#[cfg(feature = "server")]
pub struct Store {
    pub prefix: String,
}

#[server(POST "/notes")]
pub async fn add_note(#[server_ctx] store: Ctx<Store>, note: Valid<Note>) -> HyveResult<Note> {
    let mut note = note.into_inner();
    if note.text == "taken" {
        return Err(HyveError::domain(
            "note_taken",
            "a note with this text exists",
        ));
    }
    note.text = format!("{}{}", store.prefix, note.text);
    Ok(note)
}

#[server(GET "/notes/{id}")]
pub async fn get_note(id: Id<Note>) -> HyveResult<Note> {
    let _ = id;
    Err(HyveError::not_found("note"))
}

#[server(GET "/notes")]
pub async fn find_notes(filter: NoteFilter) -> HyveResult<Page<Note>> {
    let n = filter.limit.unwrap_or(1) as usize;
    let note = Note {
        text: "n".into(),
        pinned: filter.pinned.unwrap_or(false),
    };
    Ok(Page::new(vec![note; n], Some("next".into())))
}

#[server(PATCH "/notes/{id}")]
pub async fn rename_note(id: Id<Note>, text: String) -> HyveResult<Note> {
    let _ = id;
    Ok(Note {
        text,
        pinned: false,
    })
}

#[server(DELETE "/notes/{id}")]
pub async fn delete_note(id: Id<Note>) -> HyveResult<()> {
    let _ = id;
    Ok(())
}
