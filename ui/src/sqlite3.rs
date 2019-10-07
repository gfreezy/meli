/*
 * meli - sqlite3.rs
 *
 * Copyright 2019 Manos Pitsidianakis
 *
 * This file is part of meli.
 *
 * meli is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * meli is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with meli. If not, see <http://www.gnu.org/licenses/>.
 */

use crate::state::Context;
use melib::{backends::FolderHash, email::EnvelopeHash, MeliError, Result, StackVec};
use rusqlite::{params, Connection};
use std::convert::TryInto;

pub fn open_db(context: &crate::state::Context) -> Result<Connection> {
    let data_dir =
        xdg::BaseDirectories::with_prefix("meli").map_err(|e| MeliError::new(e.to_string()))?;
    let conn = Connection::open(
        data_dir
            .place_data_file("meli.db")
            .map_err(|e| MeliError::new(e.to_string()))?,
    )
    .map_err(|e| MeliError::new(e.to_string()))?;
    //let conn = Connection::open_in_memory().map_err(|e| MeliError::new(e.to_string()))?;

    /*
         *
    pub struct Envelope {
        date: String,
        from: Vec<Address>,
        to: Vec<Address>,
        cc: Vec<Address>,
        bcc: Vec<Address>,
        subject: Option<Vec<u8>>,
        message_id: MessageID,
        in_reply_to: Option<MessageID>,
        references: Option<References>,
        other_headers: FnvHashMap<String, String>,

        timestamp: UnixTimestamp,
        thread: ThreadHash,

        hash: EnvelopeHash,

        flags: Flag,
        has_attachments: bool,
    }
    */

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS envelopes (
                    id               INTEGER PRIMARY KEY,
                    hash             BLOB NOT NULL,
                    date             TEXT NOT NULL,
                    _from             TEXT NOT NULL,
                    _to               TEXT NOT NULL,
                    cc              TEXT NOT NULL,
                    bcc              TEXT NOT NULL,
                    subject          TEXT NOT NULL,
                    message_id       TEXT NOT NULL,
                    in_reply_to      TEXT NOT NULL,
                    _references       TEXT NOT NULL,
                    flags            INTEGER NOT NULL,
                    has_attachments  BOOLEAN NOT NULL,
                    body_text        TEXT NOT NULL,
                    timestamp        BLOB NOT NULL
                  );


CREATE INDEX IF NOT EXISTS envelope_timestamp_index ON envelopes (timestamp);
CREATE INDEX IF NOT EXISTS envelope__from_index ON envelopes (_from);
CREATE INDEX IF NOT EXISTS envelope__to_index ON envelopes (_to);
CREATE INDEX IF NOT EXISTS envelope_cc_index ON envelopes (cc);
CREATE INDEX IF NOT EXISTS envelope_bcc_index ON envelopes (bcc);
CREATE INDEX IF NOT EXISTS envelope_message_id_index ON envelopes (message_id);

        CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(subject, body_text, content=envelopes, content_rowid=id);

-- Triggers to keep the FTS index up to date.
CREATE TRIGGER IF NOT EXISTS envelopes_ai AFTER INSERT ON envelopes BEGIN
  INSERT INTO fts(rowid, subject, body_text) VALUES (new.id, new.subject, new.body_text);
END;

CREATE TRIGGER IF NOT EXISTS envelopes_ad AFTER DELETE ON envelopes BEGIN
  INSERT INTO fts(fts, rowid, subject, body_text) VALUES('delete', old.id, old.subject, old.body_text);
END;

CREATE TRIGGER IF NOT EXISTS envelopes_au AFTER UPDATE ON envelopes BEGIN
  INSERT INTO fts(fts, rowid, subject, body_text) VALUES('delete', old.id, old.subject, old.body_text);
  INSERT INTO fts(rowid, subject, body_text) VALUES (new.id, new.subject, new.body_text);
END; ",
    )
    .map_err(|e| MeliError::new(e.to_string()))?;

    Ok(conn)
}

pub fn insert(context: &crate::state::Context) -> Result<()> {
    let data_dir =
        xdg::BaseDirectories::with_prefix("meli").map_err(|e| MeliError::new(e.to_string()))?;
    let conn = Connection::open(
        data_dir
            .place_data_file("meli.db")
            .map_err(|e| MeliError::new(e.to_string()))?,
    )
    .map_err(|e| MeliError::new(e.to_string()))?;
    for acc in context.accounts.iter() {
        debug!("inserting {} envelopes", acc.collection.envelopes.len());
        for e in acc.collection.envelopes.values() {
            conn.execute(
                "INSERT OR REPLACE INTO envelopes (hash, date, _from, _to, cc, bcc, subject, message_id, in_reply_to, _references, flags, has_attachments, body_text, timestamp)
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![e.hash().to_be_bytes().to_vec(), e.date_as_str(), e.field_from_to_string(), e.field_to_to_string(), e.field_cc_to_string(), e.field_bcc_to_string(), e.subject().into_owned().trim_end_matches('\u{0}'), e.message_id_display().to_string(), e.in_reply_to_display().map(|f| f.to_string()).unwrap_or(String::new()), e.field_references_to_string(), i64::from(e.flags().bits()), if e.has_attachments() { 1 } else { 0 }, String::from("sdfsa"), e.hash().to_be_bytes().to_vec()],
            )
            .map_err(|e| MeliError::new(e.to_string()))?;
        }
    }

    Ok(())
}

pub fn search(
    term: &str,
    _context: &Context,
    _account_idx: usize,
    _folder_hash: FolderHash,
) -> Result<StackVec<EnvelopeHash>> {
    let data_dir =
        xdg::BaseDirectories::with_prefix("meli").map_err(|e| MeliError::new(e.to_string()))?;
    let conn = Connection::open(
        data_dir
            .place_data_file("meli.db")
            .map_err(|e| MeliError::new(e.to_string()))?,
    )
    .map_err(|e| MeliError::new(e.to_string()))?;
    let mut stmt=        conn.prepare(
                "SELECT hash FROM envelopes INNER JOIN fts ON fts.rowid = envelopes.id WHERE fts MATCH ?;")
    .map_err(|e| MeliError::new(e.to_string()))?;

    let results = stmt
        .query_map(&[term], |row| Ok(row.get(0)?))
        .map_err(|e| MeliError::new(e.to_string()))?
        .map(|r: std::result::Result<Vec<u8>, rusqlite::Error>| {
            Ok(u64::from_be_bytes(
                r.map_err(|e| MeliError::new(e.to_string()))?
                    .as_slice()
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| MeliError::new(e.to_string()))?,
            ))
        })
        .collect::<Result<StackVec<EnvelopeHash>>>();
    results
}

pub fn from(term: &str) -> Result<StackVec<EnvelopeHash>> {
    let data_dir =
        xdg::BaseDirectories::with_prefix("meli").map_err(|e| MeliError::new(e.to_string()))?;
    let conn = Connection::open_with_flags(
        data_dir
            .place_data_file("meli.db")
            .map_err(|e| MeliError::new(e.to_string()))?,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| MeliError::new(e.to_string()))?;
    let mut stmt = conn
        .prepare("SELECT hash FROM envelopes WHERE _from LIKE ?;")
        .map_err(|e| MeliError::new(e.to_string()))?;

    let results = stmt
        .query_map(&[term.trim()], |row| Ok(row.get(0)?))
        .map_err(|e| MeliError::new(e.to_string()))?
        .map(|r: std::result::Result<Vec<u8>, rusqlite::Error>| {
            Ok(u64::from_be_bytes(
                r.map_err(|e| MeliError::new(e.to_string()))?
                    .as_slice()
                    .try_into()
                    .map_err(|e: std::array::TryFromSliceError| MeliError::new(e.to_string()))?,
            ))
        })
        .collect::<Result<StackVec<EnvelopeHash>>>();
    results
}
