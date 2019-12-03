/*
 * meli - backends module
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
#[cfg(feature = "imap_backend")]
pub mod imap;
#[cfg(feature = "maildir_backend")]
pub mod maildir;
#[cfg(feature = "mbox_backend")]
pub mod mbox;
#[cfg(feature = "notmuch_backend")]
pub mod notmuch;
#[cfg(feature = "notmuch_backend")]
pub use self::notmuch::NotmuchDb;
#[cfg(feature = "jmap_backend")]
pub mod jmap;
#[cfg(feature = "jmap_backend")]
pub use self::jmap::JmapType;

#[cfg(feature = "imap_backend")]
pub use self::imap::ImapType;
use crate::async_workers::*;
use crate::conf::AccountSettings;
use crate::error::{MeliError, Result};

#[cfg(feature = "maildir_backend")]
use self::maildir::MaildirType;
#[cfg(feature = "mbox_backend")]
use self::mbox::MboxType;
use super::email::{Envelope, EnvelopeHash, Flag};
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::ops::Deref;
use std::sync::{Arc, RwLock};

use fnv::FnvHashMap;
use std;

pub type BackendCreator = Box<
    dyn Fn(
        &AccountSettings,
        Box<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Result<Box<dyn MailBackend>>,
>;

/// A hashmap containing all available mail backends.
/// An abstraction over any available backends.
pub struct Backends {
    map: FnvHashMap<std::string::String, Backend>,
}

pub struct Backend {
    pub create_fn: Box<dyn Fn() -> BackendCreator>,
    pub validate_conf_fn: Box<dyn Fn(&AccountSettings) -> Result<()>>,
}

impl Default for Backends {
    fn default() -> Self {
        Backends::new()
    }
}

impl Backends {
    pub fn new() -> Self {
        let mut b = Backends {
            map: FnvHashMap::with_capacity_and_hasher(1, Default::default()),
        };
        #[cfg(feature = "maildir_backend")]
        {
            b.register(
                "maildir".to_string(),
                Backend {
                    create_fn: Box::new(|| Box::new(|f, i| MaildirType::new(f, i))),
                    validate_conf_fn: Box::new(MaildirType::validate_config),
                },
            );
        }
        #[cfg(feature = "mbox_backend")]
        {
            b.register(
                "mbox".to_string(),
                Backend {
                    create_fn: Box::new(|| Box::new(|f, i| MboxType::new(f, i))),
                    validate_conf_fn: Box::new(MboxType::validate_config),
                },
            );
        }
        #[cfg(feature = "imap_backend")]
        {
            b.register(
                "imap".to_string(),
                Backend {
                    create_fn: Box::new(|| Box::new(|f, i| ImapType::new(f, i))),
                    validate_conf_fn: Box::new(ImapType::validate_config),
                },
            );
        }
        #[cfg(feature = "notmuch_backend")]
        {
            b.register(
                "notmuch".to_string(),
                Backend {
                    create_fn: Box::new(|| Box::new(|f, i| NotmuchDb::new(f, i))),
                    validate_conf_fn: Box::new(NotmuchDb::validate_config),
                },
            );
        }
        #[cfg(feature = "jmap_backend")]
        {
            b.register(
                "jmap".to_string(),
                Backend {
                    create_fn: Box::new(|| Box::new(|f, i| JmapType::new(f, i))),
                    validate_conf_fn: Box::new(JmapType::validate_config),
                },
            );
        }
        b
    }

    pub fn get(&self, key: &str) -> BackendCreator {
        if !self.map.contains_key(key) {
            panic!("{} is not a valid mail backend", key);
        }
        (self.map[key].create_fn)()
    }

    pub fn register(&mut self, key: String, backend: Backend) {
        if self.map.contains_key(&key) {
            panic!("{} is an already registered backend", key);
        }
        self.map.insert(key, backend);
    }

    pub fn validate_config(&self, key: &str, s: &AccountSettings) -> Result<()> {
        (self
            .map
            .get(key)
            .ok_or_else(|| MeliError::new(format!("{} is not a valid mail backend", key)))?
            .validate_conf_fn)(s)
    }
}

#[derive(Debug)]
pub enum RefreshEventKind {
    Update(EnvelopeHash, Box<Envelope>),
    /// Rename(old_hash, new_hash)
    Rename(EnvelopeHash, EnvelopeHash),
    Create(Box<Envelope>),
    Remove(EnvelopeHash),
    Rescan,
    Failure(MeliError),
}

#[derive(Debug)]
pub struct RefreshEvent {
    hash: FolderHash,
    kind: RefreshEventKind,
}

impl RefreshEvent {
    pub fn hash(&self) -> FolderHash {
        self.hash
    }
    pub fn kind(self) -> RefreshEventKind {
        /* consumes self! */
        self.kind
    }
}

/// A `RefreshEventConsumer` is a boxed closure that must be used to consume a `RefreshEvent` and
/// send it to a UI provided channel. We need this level of abstraction to provide an interface for
/// all users of mailbox refresh events.
pub struct RefreshEventConsumer(Box<dyn Fn(RefreshEvent) -> () + Send + Sync>);
impl RefreshEventConsumer {
    pub fn new(b: Box<dyn Fn(RefreshEvent) -> () + Send + Sync>) -> Self {
        RefreshEventConsumer(b)
    }
    pub fn send(&self, r: RefreshEvent) {
        self.0(r);
    }
}

pub struct NotifyFn(Box<dyn Fn(FolderHash) -> () + Send + Sync>);

impl fmt::Debug for NotifyFn {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "NotifyFn Box")
    }
}

impl From<Box<dyn Fn(FolderHash) -> () + Send + Sync>> for NotifyFn {
    fn from(kind: Box<dyn Fn(FolderHash) -> () + Send + Sync>) -> Self {
        NotifyFn(kind)
    }
}

impl NotifyFn {
    pub fn new(b: Box<dyn Fn(FolderHash) -> () + Send + Sync>) -> Self {
        NotifyFn(b)
    }
    pub fn notify(&self, f: FolderHash) {
        self.0(f);
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum FolderOperation {
    Create,
    Delete,
    Subscribe,
    Unsubscribe,
    Rename(NewFolderName),
    SetPermissions(FolderPermissions),
}

type NewFolderName = String;

pub trait MailBackend: ::std::fmt::Debug + Send + Sync {
    fn is_online(&self) -> bool;
    fn get(&mut self, folder: &Folder) -> Async<Result<Vec<Envelope>>>;
    fn watch(
        &self,
        sender: RefreshEventConsumer,
        work_context: WorkContext,
    ) -> Result<std::thread::ThreadId>;
    fn folders(&self) -> Result<FnvHashMap<FolderHash, Folder>>;
    fn operation(&self, hash: EnvelopeHash) -> Box<dyn BackendOp>;

    fn save(&self, bytes: &[u8], folder: &str, flags: Option<Flag>) -> Result<()>;
    fn folder_operation(&mut self, _path: &str, _op: FolderOperation) -> Result<()> {
        Ok(())
    }

    fn tags(&self) -> Option<Arc<RwLock<BTreeMap<u64, String>>>> {
        None
    }
    fn as_any(&self) -> &dyn Any;
}

/// A `BackendOp` manages common operations for the various mail backends. They only live for the
/// duration of the operation. They are generated by the `operation` method of `Mailbackend` trait.
///
/// # Motivation
///
/// We need a way to do various operations on individual mails regardless of what backend they come
/// from (eg local or imap).
///
/// # Creation
/// ```no_run
/// /* Create operation from Backend */
///
/// let op = backend.operation(message.hash(), mailbox.folder.hash());
/// ```
///
/// # Example
/// ```
/// use melib::mailbox::backends::{BackendOp};
/// use melib::Result;
/// use melib::{Envelope, Flag};
///
/// #[derive(Debug)]
/// struct FooOp {}
///
/// impl BackendOp for FooOp {
///     fn description(&self) -> String {
///         "Foobar".to_string()
///     }
///     fn as_bytes(&mut self) -> Result<&[u8]> {
///         unimplemented!()
///     }
///     fn fetch_headers(&mut self) -> Result<&[u8]> {
///         unimplemented!()
///     }
///     fn fetch_body(&mut self) -> Result<&[u8]> {
///         unimplemented!()
///     }
///     fn fetch_flags(&self) -> Flag {
///         unimplemented!()
///     }
/// }
///
/// let operation = Box::new(FooOp {});
/// assert_eq!("Foobar", &operation.description());
/// ```
pub trait BackendOp: ::std::fmt::Debug + ::std::marker::Send {
    fn description(&self) -> String;
    fn as_bytes(&mut self) -> Result<&[u8]>;
    //fn delete(&self) -> ();
    //fn copy(&self
    fn fetch_headers(&mut self) -> Result<&[u8]>;
    fn fetch_body(&mut self) -> Result<&[u8]>;
    fn fetch_flags(&self) -> Flag;
    fn set_flag(&mut self, envelope: &mut Envelope, flag: Flag, value: bool) -> Result<()>;
}

/// Wrapper for BackendOps that are to be set read-only.
///
/// Warning: Backend implementations may still cause side-effects (for example IMAP can set the
/// Seen flag when fetching an envelope)
#[derive(Debug)]
pub struct ReadOnlyOp {
    op: Box<dyn BackendOp>,
}

impl ReadOnlyOp {
    pub fn new(op: Box<dyn BackendOp>) -> Box<dyn BackendOp> {
        Box::new(ReadOnlyOp { op })
    }
}

impl BackendOp for ReadOnlyOp {
    fn description(&self) -> String {
        format!("read-only: {}", self.op.description())
    }
    fn as_bytes(&mut self) -> Result<&[u8]> {
        self.op.as_bytes()
    }
    fn fetch_headers(&mut self) -> Result<&[u8]> {
        self.op.fetch_headers()
    }
    fn fetch_body(&mut self) -> Result<&[u8]> {
        self.op.fetch_body()
    }
    fn fetch_flags(&self) -> Flag {
        self.op.fetch_flags()
    }
    fn set_flag(&mut self, _envelope: &mut Envelope, _flag: Flag, _value: bool) -> Result<()> {
        Err(MeliError::new("read-only set."))
    }
}

#[derive(Debug, Copy, Hash, Eq, Clone, Serialize, Deserialize, PartialEq)]
pub enum SpecialUseMailbox {
    Normal,
    Inbox,
    Archive,
    Drafts,
    Flagged,
    Junk,
    Sent,
    Trash,
}

pub trait BackendFolder: Debug {
    fn hash(&self) -> FolderHash;
    fn name(&self) -> &str;
    /// Path of folder within the mailbox hierarchy, with `/` as separator.
    fn path(&self) -> &str;
    fn change_name(&mut self, new_name: &str);
    fn clone(&self) -> Folder;
    fn children(&self) -> &[FolderHash];
    fn parent(&self) -> Option<FolderHash>;

    fn permissions(&self) -> FolderPermissions;
}

#[derive(Debug)]
struct DummyFolder {
    v: Vec<FolderHash>,
}

impl BackendFolder for DummyFolder {
    fn hash(&self) -> FolderHash {
        0
    }

    fn name(&self) -> &str {
        ""
    }

    fn path(&self) -> &str {
        ""
    }

    fn change_name(&mut self, _s: &str) {}

    fn clone(&self) -> Folder {
        folder_default()
    }

    fn children(&self) -> &[FolderHash] {
        &self.v
    }

    fn parent(&self) -> Option<FolderHash> {
        None
    }

    fn permissions(&self) -> FolderPermissions {
        FolderPermissions::default()
    }
}

pub fn folder_default() -> Folder {
    Box::new(DummyFolder {
        v: Vec::with_capacity(0),
    })
}

pub type FolderHash = u64;
pub type Folder = Box<dyn BackendFolder + Send + Sync>;

impl Clone for Folder {
    fn clone(&self) -> Self {
        BackendFolder::clone(self.deref())
    }
}

impl Default for Folder {
    fn default() -> Self {
        folder_default()
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct FolderPermissions {
    pub create_messages: bool,
    pub remove_messages: bool,
    pub set_flags: bool,
    pub create_child: bool,
    pub rename_messages: bool,
    pub delete_messages: bool,
    pub delete_mailbox: bool,
    pub change_permissions: bool,
}

impl Default for FolderPermissions {
    fn default() -> Self {
        FolderPermissions {
            create_messages: false,
            remove_messages: false,
            set_flags: false,
            create_child: false,
            rename_messages: false,
            delete_messages: false,
            delete_mailbox: false,
            change_permissions: false,
        }
    }
}
