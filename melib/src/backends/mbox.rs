/*
 * meli - mailbox module.
 *
 * Copyright 2017 Manos Pitsidianakis
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

//! # Mbox formats
//!
//! ## Resources
//!
//! - [0] <https://web.archive.org/web/20160812091518/https://jdebp.eu./FGA/mail-mbox-formats.html>
//! - [1] <https://wiki2.dovecot.org/MailboxFormat/mbox>
//! - [2] <https://manpages.debian.org/buster/mutt/mbox.5.en.html>
//!
//! ## `mbox` format
//! `mbox` describes a family of incompatible legacy formats.
//!
//! "All of the 'mbox' formats store all of the messages in the mailbox in a single file. Delivery appends new messages to the end of the file." [0]
//!
//! "Each message is preceded by a From_ line and followed by a blank line. A From_ line is a line that begins with the five characters 'F', 'r', 'o', 'm', and ' '." [0]
//!
//! ## `From ` / postmark line
//!
//! "An mbox is a text file containing an arbitrary number of e-mail messages. Each message
//! consists of a postmark, followed by an e-mail message formatted according to RFC822, RFC2822.
//! The file format is line-oriented. Lines are separated by line feed characters (ASCII 10).
//!
//! "A postmark line consists of the four characters 'From', followed by a space character,
//! followed by the message's envelope sender address, followed by whitespace, and followed by a
//! time stamp. This line is often called From_ line.
//!
//! "The sender address is expected to be addr-spec as defined in RFC2822 3.4.1. The date is expected
//! to be date-time as output by asctime(3). For compatibility reasons with legacy software,
//! two-digit years greater than or equal to 70 should be interpreted as the years 1970+, while
//! two-digit years less than 70 should be interpreted as the years 2000-2069. Software reading
//! files in this format should also be prepared to accept non-numeric timezone information such as
//! 'CET DST' for Central European Time, daylight saving time.
//!
//! "Example:
//!
//!```text
//!From example@example.com Fri Jun 23 02:56:55 2000
//!```
//!
//! "In order to avoid misinterpretation of lines in message bodies which begin with the four
//! characters 'From', followed by a space character, the mail delivery agent must quote
//! any occurrence of 'From ' at the start of a body line." [2]
//!
//! ## Metadata
//!
//! `melib` recognizes the CClient (a [Pine client API](https://web.archive.org/web/20050203003235/http://www.washington.edu/imap/)) convention for metadata in `mbox` format:
//!
//! - `Status`: R (Seen) and O (non-Recent) flags
//! - `X-Status`: A (Answered), F (Flagged), T (Draft) and D (Deleted) flags
//! - `X-Keywords`: Message’s keywords
//!
//! ## Parsing an mbox file
//!
//! ```
//! # use melib::{Result, Envelope, EnvelopeHash, mbox::*};
//! # use std::collections::HashMap;
//! # use std::sync::{Arc, Mutex};
//! let file_contents = vec![]; // Replace with actual mbox file contents
//! let index: Arc<Mutex<HashMap<EnvelopeHash, (Offset, Length)>>> = Arc::new(Mutex::new(HashMap::default()));
//! let mut message_iter = MessageIterator {
//!     index: index.clone(),
//!     input: &file_contents.as_slice(),
//!     offset: 0,
//!     file_offset: 0,
//!     format: Some(MboxFormat::MboxCl2),
//! };
//! let envelopes: Result<Vec<Envelope>> = message_iter.collect();
//! ```
//!
//! ## Writing / Appending an mbox file
//!
//! ```no_run
//! # use melib::mbox::*;
//! # use std::io::Write;
//! let mbox_1: &[u8] = br#"From: <a@b.c>\n\nHello World"#;
//! let mbox_2: &[u8] = br#"From: <d@e.f>\n\nHello World #2"#;
//! let mut file = std::io::BufWriter::new(std::fs::File::create(&"out.mbox")?);
//! let format = MboxFormat::MboxCl2;
//! format.append(
//!     &mut file,
//!     mbox_1,
//!     None, // Envelope From
//!     Some(melib::datetime::now()), // Delivered date
//!     Default::default(), // Flags and tags
//!     MboxMetadata::None,
//!     true,
//!     false,
//! )?;
//! format.append(
//!     &mut file,
//!     mbox_2,
//!     None,
//!     Some(melib::datetime::now()),
//!     Default::default(), // Flags and tags
//!     MboxMetadata::None,
//!     false,
//!     false,
//! )?;
//! file.flush()?;
//! # Ok::<(), melib::MeliError>(())
//! ```

use crate::backends::*;
use crate::collection::Collection;
use crate::conf::AccountSettings;
use crate::email::parser::BytesExt;
use crate::email::*;
use crate::error::{MeliError, Result};
use crate::get_path_hash;
use crate::shellexpand::ShellExpandTrait;
use nom::bytes::complete::tag;
use nom::character::complete::digit1;
use nom::combinator::map_res;
use nom::{self, error::ErrorKind, IResult};

extern crate notify;
use self::notify::{watcher, DebouncedEvent, RecursiveMode, Watcher};
use std::collections::hash_map::{DefaultHasher, HashMap};
use std::fs::File;
use std::hash::Hasher;
use std::io::{BufReader, Read};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, RwLock};

pub mod write;

pub mod watch;

pub type Offset = usize;
pub type Length = usize;

#[cfg(target_os = "linux")]
const F_OFD_SETLKW: libc::c_int = 38;

// Open file description locking
// # man fcntl
fn get_rw_lock_blocking(f: &File, path: &Path) -> Result<()> {
    let fd: libc::c_int = f.as_raw_fd();
    let mut flock: libc::flock = libc::flock {
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0, /* "Specifying 0 for l_len has the special meaning: lock all bytes starting at the location
                  specified by l_whence and l_start through to the end of file, no matter how large the file grows." */
        l_pid: 0, /* "By contrast with traditional record locks, the l_pid field of that structure must be set to zero when using the commands described below." */
        #[cfg(target_os = "freebsd")]
        l_sysid: 0,
    };
    let ptr: *mut libc::flock = &mut flock;
    #[cfg(not(target_os = "linux"))]
    let ret_val = unsafe { libc::fcntl(fd, libc::F_SETLKW, ptr as *mut libc::c_void) };
    #[cfg(target_os = "linux")]
    let ret_val = unsafe { libc::fcntl(fd, F_OFD_SETLKW, ptr as *mut libc::c_void) };
    if ret_val == -1 {
        let err = nix::errno::Errno::from_i32(nix::errno::errno());
        return Err(MeliError::new(format!(
            "Could not lock {}: fcntl() returned {}",
            path.display(),
            err.desc()
        )));
    }
    Ok(())
}

#[derive(Debug)]
pub struct MboxMailbox {
    hash: MailboxHash,
    name: String,
    path: PathBuf,
    fs_path: PathBuf,
    content: Vec<u8>,
    children: Vec<MailboxHash>,
    parent: Option<MailboxHash>,
    usage: Arc<RwLock<SpecialUsageMailbox>>,
    is_subscribed: bool,
    permissions: MailboxPermissions,
    pub total: Arc<Mutex<usize>>,
    pub unseen: Arc<Mutex<usize>>,
    index: Arc<Mutex<HashMap<EnvelopeHash, (Offset, Length)>>>,
}

impl BackendMailbox for MboxMailbox {
    fn hash(&self) -> MailboxHash {
        self.hash
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn path(&self) -> &str {
        /* We know it's valid UTF-8 because we supplied it */
        self.path.to_str().unwrap()
    }

    fn change_name(&mut self, s: &str) {
        self.name = s.to_string();
    }

    fn clone(&self) -> Mailbox {
        Box::new(MboxMailbox {
            hash: self.hash,
            name: self.name.clone(),
            path: self.path.clone(),
            fs_path: self.fs_path.clone(),
            content: self.content.clone(),
            children: self.children.clone(),
            usage: self.usage.clone(),
            is_subscribed: self.is_subscribed,
            parent: self.parent,
            permissions: self.permissions,
            unseen: self.unseen.clone(),
            total: self.total.clone(),
            index: self.index.clone(),
        })
    }

    fn children(&self) -> &[MailboxHash] {
        &self.children
    }

    fn parent(&self) -> Option<MailboxHash> {
        self.parent
    }

    fn special_usage(&self) -> SpecialUsageMailbox {
        *self.usage.read().unwrap()
    }

    fn permissions(&self) -> MailboxPermissions {
        self.permissions
    }
    fn is_subscribed(&self) -> bool {
        self.is_subscribed
    }
    fn set_is_subscribed(&mut self, new_val: bool) -> Result<()> {
        self.is_subscribed = new_val;
        Ok(())
    }
    fn set_special_usage(&mut self, new_val: SpecialUsageMailbox) -> Result<()> {
        *self.usage.write()? = new_val;
        Ok(())
    }

    fn count(&self) -> Result<(usize, usize)> {
        Ok((*self.unseen.lock()?, *self.total.lock()?))
    }
}

/// `BackendOp` implementor for Mbox
#[derive(Debug, Default)]
pub struct MboxOp {
    hash: EnvelopeHash,
    path: PathBuf,
    offset: Offset,
    length: Length,
    slice: std::cell::RefCell<Option<Vec<u8>>>,
}

impl MboxOp {
    pub fn new(hash: EnvelopeHash, path: &Path, offset: Offset, length: Length) -> Self {
        MboxOp {
            hash,
            path: path.to_path_buf(),
            slice: std::cell::RefCell::new(None),
            offset,
            length,
        }
    }
}

impl BackendOp for MboxOp {
    fn as_bytes(&mut self) -> ResultFuture<Vec<u8>> {
        if self.slice.get_mut().is_none() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)?;
            get_rw_lock_blocking(&file, &self.path)?;
            let mut buf_reader = BufReader::new(file);
            let mut contents = Vec::new();
            buf_reader.read_to_end(&mut contents)?;
            *self.slice.get_mut() = Some(contents);
        }
        let ret = Ok(self.slice.get_mut().as_ref().unwrap().as_slice()
            [self.offset..self.offset + self.length]
            .to_vec());
        Ok(Box::pin(async move { ret }))
    }

    fn fetch_flags(&self) -> ResultFuture<Flag> {
        let mut flags = Flag::empty();
        if self.slice.borrow().is_none() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.path)?;
            get_rw_lock_blocking(&file, &self.path)?;
            let mut buf_reader = BufReader::new(file);
            let mut contents = Vec::new();
            buf_reader.read_to_end(&mut contents)?;
            *self.slice.borrow_mut() = Some(contents);
        }
        let slice_ref = self.slice.borrow();
        let (_, headers) = parser::headers::headers_raw(slice_ref.as_ref().unwrap().as_slice())?;
        if let Some(start) = headers.find(b"Status:") {
            if let Some(end) = headers[start..].find(b"\n") {
                let start = start + b"Status:".len();
                let status = headers[start..start + end].trim();
                if status.contains(&b'F') {
                    flags.set(Flag::FLAGGED, true);
                }
                if status.contains(&b'A') {
                    flags.set(Flag::REPLIED, true);
                }
                if status.contains(&b'R') {
                    flags.set(Flag::SEEN, true);
                }
                if status.contains(&b'D') {
                    flags.set(Flag::TRASHED, true);
                }
                if status.contains(&b'T') {
                    flags.set(Flag::DRAFT, true);
                }
            }
        }
        if let Some(start) = headers.find(b"X-Status:") {
            let start = start + b"X-Status:".len();
            if let Some(end) = headers[start..].find(b"\n") {
                let status = headers[start..start + end].trim();
                if status.contains(&b'F') {
                    flags.set(Flag::FLAGGED, true);
                }
                if status.contains(&b'A') {
                    flags.set(Flag::REPLIED, true);
                }
                if status.contains(&b'R') {
                    flags.set(Flag::SEEN, true);
                }
                if status.contains(&b'D') {
                    flags.set(Flag::TRASHED, true);
                }
                if status.contains(&b'T') {
                    flags.set(Flag::DRAFT, true);
                }
            }
        }
        Ok(Box::pin(async move { Ok(flags) }))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum MboxMetadata {
    /// Dovecot uses C-Client (ie. UW-IMAP, Pine) compatible headers in mbox messages to store me
    /// - X-IMAPbase: Contains UIDVALIDITY, last used UID and list of used keywords
    /// - X-IMAP: Same as X-IMAPbase but also specifies that the message is a “pseudo message”
    /// - X-UID: Message’s allocated UID
    /// - Status: R (Seen) and O (non-Recent) flags
    /// - X-Status: A (Answered), F (Flagged), T (Draft) and D (Deleted) flags
    /// - X-Keywords: Message’s keywords
    /// - Content-Length: Length of the message body in bytes
    CClient,
    None,
}

/// Choose between "mboxo", "mboxrd", "mboxcl", "mboxcl2". For new mailboxes, prefer "mboxcl2"
/// which does not alter the mail body.
#[derive(Debug, Clone, Copy)]
pub enum MboxFormat {
    MboxO,
    MboxRd,
    MboxCl,
    MboxCl2,
}

impl Default for MboxFormat {
    fn default() -> Self {
        Self::MboxCl2
    }
}

macro_rules! find_From__line {
    ($input:expr) => {{
        //debug!("find_From__line invocation");
        let input = $input;
        let mut ptr = 0;
        let mut found = None;
        while ptr < input.len() {
            // Find next From_ candidate line.
            const TAG: &'static [u8] = b"\n\nFrom ";
            if let Some(end) = input[ptr..].find(TAG) {
                // This candidate is a valid From_ if it ends in a new line and the next line is
                // a header.
                if let Some(line_end) = input[ptr + end + TAG.len()..].find(b"\n") {
                    if crate::email::parser::headers::header(
                        &input[ptr + end + TAG.len() + line_end + 1..],
                    )
                    .is_ok()
                    {
                        found = Some(ptr + end);
                        break;
                    } else {
                        /* Ignore invalid From_ line. */
                        ptr += end + TAG.len() + line_end;
                    }
                } else {
                    /* Ignore invalid From_ line. */
                    ptr += end + TAG.len();
                }
            } else {
                found = Some(input.len());
                break;
            }
        }
        found
    }};
}

impl MboxFormat {
    pub fn parse<'i>(&self, input: &'i [u8]) -> IResult<&'i [u8], Envelope> {
        let orig_input = input;
        let mut input = input;
        match self {
            Self::MboxO => {
                let next_offset: Option<(usize, usize)> = find_From__line!(input)
                    .and_then(|end| input.find(b"\n").map(|start| (start + 1, end)));

                if let Some((start, len)) = next_offset {
                    match Envelope::from_bytes(&input[start..len], None) {
                        Ok(mut env) => {
                            let mut flags = Flag::empty();
                            if env.other_headers().contains_key("Status") {
                                if env.other_headers()["Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                            }
                            if env.other_headers().contains_key("X-Status") {
                                if env.other_headers()["X-Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["X-Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["X-Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["X-Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                                if env.other_headers()["X-Status"].contains('T') {
                                    flags.set(Flag::DRAFT, true);
                                }
                            }
                            env.set_flags(flags);
                            if len == input.len() {
                                Ok((&[], env))
                            } else {
                                input = &input[len + 2..];
                                Ok((input, env))
                            }
                        }
                        Err(err) => {
                            debug!("Could not parse mail {:?}", err);
                            Err(nom::Err::Error((input, ErrorKind::Tag)))
                        }
                    }
                } else {
                    let start: Offset = input.find(b"\n").map(|v| v + 1).unwrap_or(0);
                    match Envelope::from_bytes(&input[start..], None) {
                        Ok(mut env) => {
                            let mut flags = Flag::empty();
                            if env.other_headers().contains_key("Status") {
                                if env.other_headers()["Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                            }
                            if env.other_headers().contains_key("X-Status") {
                                if env.other_headers()["X-Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["X-Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["X-Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["X-Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                                if env.other_headers()["X-Status"].contains('T') {
                                    flags.set(Flag::DRAFT, true);
                                }
                            }
                            env.set_flags(flags);
                            Ok((&[], env))
                        }
                        Err(err) => {
                            debug!("Could not parse mail at {:?}", err);
                            Err(nom::Err::Error((input, ErrorKind::Tag)))
                        }
                    }
                }
            }
            Self::MboxRd => {
                let next_offset: Option<(usize, usize)> = find_From__line!(input)
                    .and_then(|end| input.find(b"\n").map(|start| (start + 1, end)));

                if let Some((start, len)) = next_offset {
                    match Envelope::from_bytes(&input[start..len], None) {
                        Ok(mut env) => {
                            let mut flags = Flag::empty();
                            if env.other_headers().contains_key("Status") {
                                if env.other_headers()["Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                            }
                            if env.other_headers().contains_key("X-Status") {
                                if env.other_headers()["X-Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["X-Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["X-Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["X-Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                                if env.other_headers()["X-Status"].contains('T') {
                                    flags.set(Flag::DRAFT, true);
                                }
                            }
                            env.set_flags(flags);
                            if len == input.len() {
                                Ok((&[], env))
                            } else {
                                input = &input[len + 2..];
                                Ok((input, env))
                            }
                        }
                        Err(err) => {
                            debug!("Could not parse mail {:?}", err);
                            Err(nom::Err::Error((input, ErrorKind::Tag)))
                        }
                    }
                } else {
                    let start: Offset = input.find(b"\n").map(|v| v + 1).unwrap_or(0);
                    match Envelope::from_bytes(&input[start..], None) {
                        Ok(mut env) => {
                            let mut flags = Flag::empty();
                            if env.other_headers().contains_key("Status") {
                                if env.other_headers()["Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                            }
                            if env.other_headers().contains_key("X-Status") {
                                if env.other_headers()["X-Status"].contains('F') {
                                    flags.set(Flag::FLAGGED, true);
                                }
                                if env.other_headers()["X-Status"].contains('A') {
                                    flags.set(Flag::REPLIED, true);
                                }
                                if env.other_headers()["X-Status"].contains('R') {
                                    flags.set(Flag::SEEN, true);
                                }
                                if env.other_headers()["X-Status"].contains('D') {
                                    flags.set(Flag::TRASHED, true);
                                }
                                if env.other_headers()["X-Status"].contains('T') {
                                    flags.set(Flag::DRAFT, true);
                                }
                            }
                            env.set_flags(flags);
                            Ok((&[], env))
                        }
                        Err(err) => {
                            debug!("Could not parse mail {:?}", err);
                            Err(nom::Err::Error((input, ErrorKind::Tag)))
                        }
                    }
                }
            }
            Self::MboxCl | Self::MboxCl2 => {
                let start: Offset = input.find(b"\n").map(|v| v + 1).unwrap_or(0);
                input = &input[start..];
                let headers_end: usize = input.find(b"\n\n").unwrap_or(input.len());
                let content_length = if let Some(v) = input[..headers_end].find(b"Content-Length: ")
                {
                    v
                } else {
                    // Is not MboxCl{,2}
                    return Self::MboxRd.parse(orig_input);
                };
                let (_input, _) = if let Ok(s) = tag::<_, &[u8], (&[u8], nom::error::ErrorKind)>(
                    "Content-Length:",
                )(&input[content_length..])
                {
                    s
                } else {
                    return Self::MboxRd.parse(orig_input);
                };
                let (_input, bytes) = if let Ok(s) =
                    map_res::<&[u8], _, _, (&[u8], nom::error::ErrorKind), _, _, _>(
                        digit1,
                        |s: &[u8]| String::from_utf8_lossy(s).parse::<usize>(),
                    )(_input.ltrim())
                {
                    s
                } else {
                    return Self::MboxRd.parse(orig_input);
                };

                match Envelope::from_bytes(&input[..headers_end + bytes], None) {
                    Ok(mut env) => {
                        let mut flags = Flag::empty();
                        if env.other_headers().contains_key("Status") {
                            if env.other_headers()["Status"].contains('F') {
                                flags.set(Flag::FLAGGED, true);
                            }
                            if env.other_headers()["Status"].contains('A') {
                                flags.set(Flag::REPLIED, true);
                            }
                            if env.other_headers()["Status"].contains('R') {
                                flags.set(Flag::SEEN, true);
                            }
                            if env.other_headers()["Status"].contains('D') {
                                flags.set(Flag::TRASHED, true);
                            }
                        }
                        if env.other_headers().contains_key("X-Status") {
                            if env.other_headers()["X-Status"].contains('F') {
                                flags.set(Flag::FLAGGED, true);
                            }
                            if env.other_headers()["X-Status"].contains('A') {
                                flags.set(Flag::REPLIED, true);
                            }
                            if env.other_headers()["X-Status"].contains('R') {
                                flags.set(Flag::SEEN, true);
                            }
                            if env.other_headers()["X-Status"].contains('D') {
                                flags.set(Flag::TRASHED, true);
                            }
                            if env.other_headers()["X-Status"].contains('T') {
                                flags.set(Flag::DRAFT, true);
                            }
                        }
                        env.set_flags(flags);
                        if headers_end + 2 + bytes >= input.len() {
                            Ok((&[], env))
                        } else {
                            input = &input[headers_end + 3 + bytes..];
                            Ok((input, env))
                        }
                    }
                    Err(_err) => Self::MboxRd.parse(orig_input),
                }
            }
        }
    }
}

pub fn mbox_parse(
    index: Arc<Mutex<HashMap<EnvelopeHash, (Offset, Length)>>>,
    input: &[u8],
    file_offset: usize,
    format: Option<MboxFormat>,
) -> IResult<&[u8], Vec<Envelope>> {
    if input.is_empty() {
        return Err(nom::Err::Error((input, ErrorKind::Tag)));
    }
    let mut offset = 0;
    let mut index = index.lock().unwrap();
    let mut envelopes = Vec::with_capacity(32);

    let format = format.unwrap_or(MboxFormat::MboxCl2);
    while !input[offset + file_offset..].is_empty() {
        let (next_input, env) = match format.parse(&input[offset + file_offset..]) {
            Ok(v) => v,
            Err(e) => {
                // Try to recover from this error by finding a new candidate From_ line
                if let Some(next_offset) = find_From__line!(&input[offset + file_offset..]) {
                    offset += next_offset;
                    if offset != input.len() {
                        // If we are not at EOF, we will be at this point
                        //    "\n\nFrom ..."
                        //     ↑
                        // So, skip those two newlines.
                        offset += 2;
                    }
                } else {
                    return Err(e);
                }
                continue;
            }
        };
        let start: Offset = input[offset + file_offset..]
            .find(b"\n")
            .map(|v| v + 1)
            .unwrap_or(0);
        let len = input.len() - next_input.len() - offset - file_offset - start;
        index.insert(env.hash(), (offset + file_offset + start, len));
        offset += len + start;

        envelopes.push(env);
    }
    Ok((&[], envelopes))
}

pub struct MessageIterator<'a> {
    pub index: Arc<Mutex<HashMap<EnvelopeHash, (Offset, Length)>>>,
    pub input: &'a [u8],
    pub file_offset: usize,
    pub offset: usize,
    pub format: Option<MboxFormat>,
}

impl<'a> Iterator for MessageIterator<'a> {
    type Item = Result<Envelope>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.input.is_empty() {
            return None;
        }
        let mut index = self.index.lock().unwrap();

        let format = self.format.unwrap_or(MboxFormat::MboxCl2);
        while !self.input[self.offset + self.file_offset..].is_empty() {
            let (next_input, env) =
                match format.parse(&self.input[self.offset + self.file_offset..]) {
                    Ok(v) => v,
                    Err(e) => {
                        // Try to recover from this error by finding a new candidate From_ line
                        if let Some(next_offset) =
                            find_From__line!(&self.input[self.offset + self.file_offset..])
                        {
                            self.offset += next_offset;
                            if self.offset != self.input.len() {
                                // If we are not at EOF, we will be at this point
                                //    "\n\nFrom ..."
                                //     ↑
                                // So, skip those two newlines.
                                self.offset += 2;
                            }
                        } else {
                            self.input = b"";
                            return Some(Err(e.into()));
                        }
                        continue;
                    }
                };
            let start: Offset = self.input[self.offset + self.file_offset..]
                .find(b"\n")
                .map(|v| v + 1)
                .unwrap_or(0);
            let len = self.input.len() - next_input.len() - self.offset - self.file_offset - start;
            index.insert(env.hash(), (self.offset + self.file_offset + start, len));
            self.offset += len + start;

            return Some(Ok(env));
        }
        None
    }
}

/// Mbox backend
#[derive(Debug)]
pub struct MboxType {
    account_name: String,
    path: PathBuf,
    collection: Collection,
    mailbox_index: Arc<Mutex<HashMap<EnvelopeHash, MailboxHash>>>,
    mailboxes: Arc<Mutex<HashMap<MailboxHash, MboxMailbox>>>,
    prefer_mbox_type: Option<MboxFormat>,
    event_consumer: BackendEventConsumer,
}

impl MailBackend for MboxType {
    fn capabilities(&self) -> MailBackendCapabilities {
        const CAPABILITIES: MailBackendCapabilities = MailBackendCapabilities {
            is_async: false,
            is_remote: false,
            supports_search: false,
            extensions: None,
            supports_tags: false,
            supports_submission: false,
        };
        CAPABILITIES
    }

    fn is_online(&self) -> ResultFuture<()> {
        Ok(Box::pin(async { Ok(()) }))
    }

    fn fetch(
        &mut self,
        mailbox_hash: MailboxHash,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Vec<Envelope>>> + Send + 'static>>> {
        struct FetchState {
            mailbox_hash: MailboxHash,
            mailbox_index: Arc<Mutex<HashMap<EnvelopeHash, MailboxHash>>>,
            mailboxes: Arc<Mutex<HashMap<MailboxHash, MboxMailbox>>>,
            prefer_mbox_type: Option<MboxFormat>,
            offset: usize,
            file_offset: usize,
            contents: Vec<u8>,
        }
        impl FetchState {
            async fn fetch(&mut self) -> Result<Option<Vec<Envelope>>> {
                let mailboxes_lck = self.mailboxes.lock().unwrap();
                let index = mailboxes_lck[&self.mailbox_hash].index.clone();
                drop(mailboxes_lck);
                let mut message_iter = MessageIterator {
                    index,
                    input: &self.contents.as_slice(),
                    offset: self.offset,
                    file_offset: self.file_offset,
                    format: self.prefer_mbox_type,
                };
                let mut payload = vec![];
                let mut done = false;
                'iter_for_loop: for _i in 0..150 {
                    match message_iter.next() {
                        Some(Ok(env)) => {
                            payload.push(env);
                        }
                        Some(Err(_err)) => {
                            debug!(&_err);
                        }
                        None => {
                            done = true;
                            break 'iter_for_loop;
                        }
                    }
                }
                self.offset = message_iter.offset;
                self.file_offset = message_iter.file_offset;
                {
                    let mut mailbox_index_lck = self.mailbox_index.lock().unwrap();
                    for env in &payload {
                        mailbox_index_lck.insert(env.hash(), self.mailbox_hash);
                    }
                }
                if done {
                    if payload.is_empty() {
                        Ok(None)
                    } else {
                        let mut mailbox_lock = self.mailboxes.lock().unwrap();
                        let contents = std::mem::replace(&mut self.contents, vec![]);
                        mailbox_lock
                            .entry(self.mailbox_hash)
                            .and_modify(|f| f.content = contents);
                        Ok(Some(payload))
                    }
                } else {
                    Ok(Some(payload))
                }
            }
        }
        let mailboxes = self.mailboxes.clone();

        let mailbox_path = mailboxes.lock().unwrap()[&mailbox_hash].fs_path.clone();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&mailbox_path)?;
        get_rw_lock_blocking(&file, &mailbox_path)?;
        let mut buf_reader = BufReader::new(file);
        let mut contents = Vec::new();
        buf_reader.read_to_end(&mut contents)?;
        let mut state = FetchState {
            mailbox_hash,
            mailboxes,
            mailbox_index: self.mailbox_index.clone(),
            prefer_mbox_type: self.prefer_mbox_type,
            contents,
            offset: 0,
            file_offset: 0,
        };
        Ok(Box::pin(async_stream::try_stream! {
            loop {
                if let Some(res) = state.fetch().await.map_err(|err| {
                    debug!("fetch err {:?}", &err);
                    err})? {
                    yield res;
                } else {
                    return;
                }
            }
        }))
    }

    fn refresh(&mut self, _mailbox_hash: MailboxHash) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn watcher(&self) -> Result<Box<dyn BackendWatcher>> {
        let account_hash = {
            let mut hasher = DefaultHasher::new();
            hasher.write(self.account_name.as_bytes());
            hasher.finish()
        };
        let event_consumer = self.event_consumer.clone();
        let mailbox_index = self.mailbox_index.clone();
        let mailboxes = self.mailboxes.clone();
        let prefer_mbox_type = self.prefer_mbox_type.clone();
        Ok(Box::new(watch::MboxWatcher {
            account_hash,
            event_consumer,
            mailbox_hashes: BTreeSet::default(),
            mailbox_index,
            mailboxes,
            polling_period: std::time::Duration::from_secs(60),
            prefer_mbox_type,
        }))
    }

    fn mailboxes(&self) -> ResultFuture<HashMap<MailboxHash, Mailbox>> {
        let ret = Ok(self
            .mailboxes
            .lock()
            .unwrap()
            .iter()
            .map(|(h, f)| (*h, f.clone() as Mailbox))
            .collect());
        Ok(Box::pin(async { ret }))
    }

    fn operation(&self, env_hash: EnvelopeHash) -> Result<Box<dyn BackendOp>> {
        let mailbox_hash = self.mailbox_index.lock().unwrap()[&env_hash];
        let mailboxes_lck = self.mailboxes.lock().unwrap();
        let (offset, length) = {
            let index = mailboxes_lck[&mailbox_hash].index.lock().unwrap();
            index[&env_hash]
        };
        let mailbox_path = mailboxes_lck[&mailbox_hash].fs_path.clone();
        Ok(Box::new(MboxOp::new(
            env_hash,
            mailbox_path.as_path(),
            offset,
            length,
        )))
    }

    fn copy_messages(
        &mut self,
        _env_hashes: EnvelopeHashBatch,
        _source_mailbox_hash: MailboxHash,
        _destination_mailbox_hash: MailboxHash,
        _move_: bool,
    ) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn set_flags(
        &mut self,
        _env_hashes: EnvelopeHashBatch,
        _mailbox_hash: MailboxHash,
        _flags: SmallVec<[(std::result::Result<Flag, String>, bool); 8]>,
    ) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn delete_messages(
        &mut self,
        _env_hashes: EnvelopeHashBatch,
        _mailbox_hash: MailboxHash,
    ) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn save(
        &self,
        _bytes: Vec<u8>,
        _mailbox_hash: MailboxHash,
        _flags: Option<Flag>,
    ) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn collection(&self) -> Collection {
        self.collection.clone()
    }
}

macro_rules! get_conf_val {
    ($s:ident[$var:literal]) => {
        $s.extra.get($var).ok_or_else(|| {
            MeliError::new(format!(
                "Configuration error ({}): mbox backend requires the field `{}` set",
                $s.name.as_str(),
                $var
            ))
        })
    };
    ($s:ident[$var:literal], $default:expr) => {
        $s.extra
            .get($var)
            .map(|v| {
                <_>::from_str(v).map_err(|e| {
                    MeliError::new(format!(
                        "Configuration error ({}): Invalid value for field `{}`: {}\n{}",
                        $s.name.as_str(),
                        $var,
                        v,
                        e
                    ))
                })
            })
            .unwrap_or_else(|| Ok($default))
    };
}

impl MboxType {
    pub fn new(
        s: &AccountSettings,
        _is_subscribed: Box<dyn Fn(&str) -> bool>,
        event_consumer: BackendEventConsumer,
    ) -> Result<Box<dyn MailBackend>> {
        let path = Path::new(s.root_mailbox.as_str()).expand();
        if !path.exists() {
            return Err(MeliError::new(format!(
                "\"root_mailbox\" {} for account {} is not a valid path.",
                s.root_mailbox.as_str(),
                s.name()
            )));
        }
        let prefer_mbox_type: String = get_conf_val!(s["prefer_mbox_type"], "auto".to_string())?;
        let ret = MboxType {
            account_name: s.name().to_string(),
            event_consumer,
            path,
            prefer_mbox_type: match prefer_mbox_type.as_str() {
                "auto" => None,
                "mboxo" => Some(MboxFormat::MboxO),
                "mboxrd" => Some(MboxFormat::MboxRd),
                "mboxcl" => Some(MboxFormat::MboxCl),
                "mboxcl2" => Some(MboxFormat::MboxCl2),
                _ => {
                    return Err(MeliError::new(format!(
                        "{} invalid `prefer_mbox_type` value: `{}`",
                        s.name(),
                        prefer_mbox_type,
                    )))
                }
            },
            collection: Collection::default(),
            mailbox_index: Default::default(),
            mailboxes: Default::default(),
        };
        let name: String = ret
            .path
            .file_name()
            .map(|f| f.to_string_lossy().into())
            .unwrap_or_default();
        let hash = get_path_hash!(&ret.path);

        let read_only = if let Ok(metadata) = std::fs::metadata(&ret.path) {
            metadata.permissions().readonly()
        } else {
            true
        };

        ret.mailboxes.lock().unwrap().insert(
            hash,
            MboxMailbox {
                hash,
                path: name.clone().into(),
                fs_path: ret.path.clone(),
                name,
                content: Vec::new(),
                children: Vec::new(),
                parent: None,
                usage: Arc::new(RwLock::new(SpecialUsageMailbox::Normal)),
                is_subscribed: true,
                permissions: MailboxPermissions {
                    create_messages: !read_only,
                    remove_messages: !read_only,
                    set_flags: !read_only,
                    create_child: !read_only,
                    rename_messages: !read_only,
                    delete_messages: !read_only,
                    delete_mailbox: !read_only,
                    change_permissions: false,
                },
                unseen: Arc::new(Mutex::new(0)),
                total: Arc::new(Mutex::new(0)),
                index: Default::default(),
            },
        );
        /* Look for other mailboxes */
        for (k, f) in s.mailboxes.iter() {
            if let Some(path_str) = f.extra.get("path") {
                let hash = get_path_hash!(path_str);
                let pathbuf: PathBuf = path_str.into();
                if !pathbuf.exists() || pathbuf.is_dir() {
                    return Err(MeliError::new(format!(
                        "mbox mailbox configuration entry \"{}\" path value {} is not a file.",
                        k, path_str
                    )));
                }
                let read_only = if let Ok(metadata) = std::fs::metadata(&pathbuf) {
                    metadata.permissions().readonly()
                } else {
                    true
                };

                ret.mailboxes.lock().unwrap().insert(
                    hash,
                    MboxMailbox {
                        hash,
                        name: k.to_string(),
                        fs_path: pathbuf,
                        path: k.into(),
                        content: Vec::new(),
                        children: Vec::new(),
                        parent: None,
                        usage: Arc::new(RwLock::new(f.usage.unwrap_or_default())),
                        is_subscribed: f.subscribe.is_true(),
                        permissions: MailboxPermissions {
                            create_messages: !read_only,
                            remove_messages: !read_only,
                            set_flags: !read_only,
                            create_child: !read_only,
                            rename_messages: !read_only,
                            delete_messages: !read_only,
                            delete_mailbox: !read_only,
                            change_permissions: false,
                        },
                        unseen: Arc::new(Mutex::new(0)),
                        total: Arc::new(Mutex::new(0)),
                        index: Default::default(),
                    },
                );
            } else {
                return Err(MeliError::new(format!(
                    "mbox mailbox configuration entry \"{}\" should have a \"path\" value set pointing to an mbox file.",
                    k
                )));
            }
        }
        Ok(Box::new(ret))
    }

    pub fn validate_config(s: &AccountSettings) -> Result<()> {
        let path = Path::new(s.root_mailbox.as_str()).expand();
        if !path.exists() {
            return Err(MeliError::new(format!(
                "\"root_mailbox\" {} for account {} is not a valid path.",
                s.root_mailbox.as_str(),
                s.name()
            )));
        }
        let prefer_mbox_type: Result<String> =
            get_conf_val!(s["prefer_mbox_type"], "auto".to_string());
        prefer_mbox_type?;
        Ok(())
    }
}
