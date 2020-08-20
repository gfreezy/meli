/*
 * meli - notmuch backend
 *
 * Copyright 2019 - 2020 Manos Pitsidianakis
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

use crate::backends::*;
use crate::conf::AccountSettings;
use crate::email::{Envelope, EnvelopeHash, Flag};
use crate::error::{MeliError, Result, ResultIntoMeliError};
use crate::shellexpand::ShellExpandTrait;
use smallvec::SmallVec;
use std::collections::{
    hash_map::{DefaultHasher, HashMap},
    BTreeMap,
};
use std::error::Error;
use std::ffi::{CStr, CString, OsStr};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

pub mod bindings;
use bindings::*;

#[derive(Debug, Clone)]
struct DbConnection {
    lib: Arc<libloading::Library>,
    inner: Arc<RwLock<*mut notmuch_database_t>>,
    database_ph: std::marker::PhantomData<&'static mut notmuch_database_t>,
}

unsafe impl Send for DbConnection {}
unsafe impl Sync for DbConnection {}

macro_rules! call {
    ($lib:expr, $func:ty) => {{
        let func: libloading::Symbol<$func> = $lib.get(stringify!($func).as_bytes()).unwrap();
        func
    }};
}

#[derive(Debug)]
pub struct NotmuchError(String);

impl std::fmt::Display for NotmuchError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl Error for NotmuchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        None
    }
}

macro_rules! try_call {
    ($lib:expr, $call:expr) => {{
        let status = $call;
        if status == _notmuch_status_NOTMUCH_STATUS_SUCCESS {
            Ok(())
        } else {
            let c_str = call!($lib, notmuch_status_to_string)(status);
            Err(NotmuchError(
                CStr::from_ptr(c_str).to_string_lossy().into_owned(),
            ))
        }
    }};
}

impl Drop for DbConnection {
    fn drop(&mut self) {
        let inner = self.inner.write().unwrap();
        unsafe {
            call!(self.lib, notmuch_database_close)(*inner);
            call!(self.lib, notmuch_database_destroy)(*inner);
        }
    }
}

#[derive(Debug)]
pub struct NotmuchDb {
    lib: Arc<libloading::Library>,
    revision_uuid: Arc<RwLock<u64>>,
    mailboxes: Arc<RwLock<HashMap<MailboxHash, NotmuchMailbox>>>,
    index: Arc<RwLock<HashMap<EnvelopeHash, CString>>>,
    mailbox_index: Arc<RwLock<HashMap<EnvelopeHash, SmallVec<[MailboxHash; 16]>>>>,
    tag_index: Arc<RwLock<BTreeMap<u64, String>>>,
    path: PathBuf,
    account_name: String,
    event_consumer: BackendEventConsumer,
    save_messages_to: Option<PathBuf>,
}

unsafe impl Send for NotmuchDb {}
unsafe impl Sync for NotmuchDb {}

#[derive(Debug, Clone, Default)]
struct NotmuchMailbox {
    hash: MailboxHash,
    children: Vec<MailboxHash>,
    parent: Option<MailboxHash>,
    name: String,
    path: String,
    query_str: String,
    usage: Arc<RwLock<SpecialUsageMailbox>>,

    total: Arc<Mutex<usize>>,
    unseen: Arc<Mutex<usize>>,
}

impl BackendMailbox for NotmuchMailbox {
    fn hash(&self) -> MailboxHash {
        self.hash
    }

    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn path(&self) -> &str {
        self.path.as_str()
    }

    fn change_name(&mut self, _s: &str) {}

    fn clone(&self) -> Mailbox {
        Box::new(std::clone::Clone::clone(self))
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
        MailboxPermissions::default()
    }

    fn is_subscribed(&self) -> bool {
        true
    }

    fn set_is_subscribed(&mut self, _new_val: bool) -> Result<()> {
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

unsafe impl Send for NotmuchMailbox {}
unsafe impl Sync for NotmuchMailbox {}

impl NotmuchDb {
    pub fn new(
        s: &AccountSettings,
        _is_subscribed: Box<dyn Fn(&str) -> bool>,
        event_consumer: BackendEventConsumer,
    ) -> Result<Box<dyn MailBackend>> {
        let lib = Arc::new(libloading::Library::new("libnotmuch.so.5")?);
        let path = Path::new(s.root_mailbox.as_str()).expand();
        if !path.exists() {
            return Err(MeliError::new(format!(
                "\"root_mailbox\" {} for account {} is not a valid path.",
                s.root_mailbox.as_str(),
                s.name()
            )));
        }

        let mut mailboxes = HashMap::default();
        for (k, f) in s.mailboxes.iter() {
            if let Some(query_str) = f.extra.get("query") {
                let hash = {
                    let mut h = DefaultHasher::new();
                    k.hash(&mut h);
                    h.finish()
                };
                mailboxes.insert(
                    hash,
                    NotmuchMailbox {
                        hash,
                        name: k.to_string(),
                        path: k.to_string(),
                        children: vec![],
                        parent: None,
                        query_str: query_str.to_string(),
                        usage: Arc::new(RwLock::new(SpecialUsageMailbox::Normal)),
                        total: Arc::new(Mutex::new(0)),
                        unseen: Arc::new(Mutex::new(0)),
                    },
                );
            } else {
                return Err(MeliError::new(format!(
                    "notmuch mailbox configuration entry \"{}\" should have a \"query\" value set.",
                    k
                )));
            }
        }

        Ok(Box::new(NotmuchDb {
            lib,
            revision_uuid: Arc::new(RwLock::new(0)),
            path,
            index: Arc::new(RwLock::new(Default::default())),
            mailbox_index: Arc::new(RwLock::new(Default::default())),
            tag_index: Arc::new(RwLock::new(Default::default())),

            mailboxes: Arc::new(RwLock::new(mailboxes)),
            save_messages_to: None,
            account_name: s.name().to_string(),
            event_consumer,
        }))
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
        for (k, f) in s.mailboxes.iter() {
            if f.extra.get("query").is_none() {
                return Err(MeliError::new(format!(
                    "notmuch mailbox configuration entry \"{}\" should have a \"query\" value set.",
                    k
                )));
            }
        }
        Ok(())
    }

    pub fn search(&self, query_s: &str) -> Result<SmallVec<[EnvelopeHash; 512]>> {
        let database = Self::new_connection(self.path.as_path(), self.lib.clone(), false)?;
        let database_lck = database.inner.read().unwrap();
        let query: Query = Query::new(self.lib.clone(), &database_lck, query_s)?;
        let mut ret = SmallVec::new();
        let iter = query.search()?;
        for message in iter {
            let msg_id = unsafe { call!(self.lib, notmuch_message_get_message_id)(message) };
            let c_str = unsafe { CStr::from_ptr(msg_id) };
            let env_hash = {
                let mut hasher = DefaultHasher::default();
                c_str.hash(&mut hasher);
                hasher.finish()
            };
            ret.push(env_hash);
        }

        Ok(ret)
    }

    fn new_connection(
        path: &Path,
        lib: Arc<libloading::Library>,
        write: bool,
    ) -> Result<DbConnection> {
        let path_c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
        let path_ptr = path_c.as_ptr();
        let mut database: *mut notmuch_database_t = std::ptr::null_mut();
        let status = unsafe {
            call!(lib, notmuch_database_open)(
                path_ptr,
                if write {
                    notmuch_database_mode_t_NOTMUCH_DATABASE_MODE_READ_WRITE
                } else {
                    notmuch_database_mode_t_NOTMUCH_DATABASE_MODE_READ_ONLY
                },
                &mut database as *mut _,
            )
        };
        if status != 0 {
            return Err(MeliError::new(format!(
                "Could not open notmuch database at path {}. notmuch_database_open returned {}.",
                path.display(),
                status
            )));
        }
        assert!(!database.is_null());
        Ok(DbConnection {
            lib,
            inner: Arc::new(RwLock::new(database)),
            database_ph: std::marker::PhantomData,
        })
    }
}

impl MailBackend for NotmuchDb {
    fn capabilities(&self) -> MailBackendCapabilities {
        const CAPABILITIES: MailBackendCapabilities = MailBackendCapabilities {
            is_async: false,
            is_remote: false,
            supports_search: true,
            extensions: None,
            supports_tags: true,
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
            database: Arc<DbConnection>,
            index: Arc<RwLock<HashMap<EnvelopeHash, CString>>>,
            mailbox_index: Arc<RwLock<HashMap<EnvelopeHash, SmallVec<[MailboxHash; 16]>>>>,
            mailboxes: Arc<RwLock<HashMap<u64, NotmuchMailbox>>>,
            tag_index: Arc<RwLock<BTreeMap<u64, String>>>,
            lib: Arc<libloading::Library>,
            iter: std::vec::IntoIter<CString>,
        }
        impl FetchState {
            async fn fetch(&mut self) -> Result<Option<Vec<Envelope>>> {
                let mut unseen_count = 0;
                let chunk_size = 250;
                let mut mailbox_index_lck = self.mailbox_index.write().unwrap();
                let mut ret: Vec<Envelope> = Vec::with_capacity(chunk_size);
                let mut done: bool = false;
                for _ in 0..chunk_size {
                    if let Some(message_id) = self.iter.next() {
                        let mut message: *mut notmuch_message_t = std::ptr::null_mut();
                        unsafe {
                            call!(self.lib, notmuch_database_find_message)(
                                *self.database.inner.read().unwrap(),
                                message_id.as_ptr(),
                                &mut message as *mut _,
                            )
                        };
                        if message.is_null() {
                            continue;
                        }
                        match notmuch_message_into_envelope(
                            self.lib.clone(),
                            self.index.clone(),
                            self.tag_index.clone(),
                            self.database.clone(),
                            message,
                        ) {
                            Ok(env) => {
                                mailbox_index_lck
                                    .entry(env.hash())
                                    .or_default()
                                    .push(self.mailbox_hash);
                                if !env.is_seen() {
                                    unseen_count += 1;
                                }
                                ret.push(env);
                            }
                            Err(err) => {
                                debug!("could not parse message {:?} {}", err, {
                                    let fs_path = unsafe {
                                        call!(self.lib, notmuch_message_get_filename)(message)
                                    };
                                    let c_str = unsafe { CStr::from_ptr(fs_path) };
                                    String::from_utf8_lossy(c_str.to_bytes())
                                });
                            }
                        }
                    } else {
                        done = true;
                        break;
                    }
                }
                {
                    let mailboxes_lck = self.mailboxes.read().unwrap();
                    let mailbox = mailboxes_lck.get(&self.mailbox_hash).unwrap();
                    let mut unseen_lck = mailbox.unseen.lock().unwrap();
                    *unseen_lck += unseen_count;
                }
                if done && ret.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(ret))
                }
            }
        }
        let database = Arc::new(NotmuchDb::new_connection(
            self.path.as_path(),
            self.lib.clone(),
            false,
        )?);
        let index = self.index.clone();
        let mailbox_index = self.mailbox_index.clone();
        let tag_index = self.tag_index.clone();
        let mailboxes = self.mailboxes.clone();
        let lib = self.lib.clone();
        let v: Vec<CString>;
        {
            let database_lck = database.inner.read().unwrap();
            let mailboxes_lck = mailboxes.read().unwrap();
            let mailbox = mailboxes_lck.get(&mailbox_hash).unwrap();
            let query: Query =
                Query::new(self.lib.clone(), &database_lck, mailbox.query_str.as_str())?;
            {
                let mut total_lck = mailbox.total.lock().unwrap();
                let mut unseen_lck = mailbox.unseen.lock().unwrap();
                *total_lck = query.count()? as usize;
                *unseen_lck = 0;
            }
            v = query
                .search()?
                .into_iter()
                .map(|m| notmuch_message_insert(&lib, &index, m))
                .collect();
        }

        let mut state = FetchState {
            mailbox_hash,
            mailboxes,
            database,
            lib,
            index,
            mailbox_index,
            tag_index,
            iter: v.into_iter(),
        };
        Ok(Box::pin(async_stream::try_stream! {
            while let Some(res) = state.fetch().await.map_err(|err| { debug!("fetch err {:?}", &err); err})? {
                yield res;
            }
        }))
    }

    fn refresh(&mut self, _mailbox_hash: MailboxHash) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }

    fn watch(&self) -> ResultFuture<()> {
        Err(MeliError::new("Unimplemented."))
    }
    /*
        fn watch(&self) -> ResultFuture<()> {
            extern crate notify;
            use crate::backends::RefreshEventKind::*;
            use notify::{watcher, RecursiveMode, Watcher};
            let sender = self.event_consumer.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = watcher(tx, std::time::Duration::from_secs(2)).unwrap();
            watcher.watch(&self.path, RecursiveMode::Recursive).unwrap();
            let path = self.path.clone();
            let lib = self.lib.clone();
            let tag_index = self.tag_index.clone();
            let index = self.index.clone();
            let account_hash = {
                let mut hasher = DefaultHasher::new();
                hasher.write(self.account_name.as_bytes());
                hasher.finish()
            };
            let mailbox_index = self.mailbox_index.clone();
            let mailboxes = self.mailboxes.clone();
            {
                let database = NotmuchDb::new_connection(path.as_path(), lib.clone(), false)?;
                let mut revision_uuid_lck = self.revision_uuid.write().unwrap();

                *revision_uuid_lck = unsafe {
                    call!(lib, notmuch_database_get_revision)(
                        *database.inner.read().unwrap(),
                        std::ptr::null_mut(),
                    )
                };
            }
            let revision_uuid = self.revision_uuid.clone();

            let handle = std::thread::Builder::new()
                .name(format!("watching {}", self.account_name))
                .spawn(move || {
                    let _watcher = watcher;
                    let c = move |sender: &BackendEventConsumer| -> std::result::Result<(), MeliError> {
                        loop {
                            let _ = rx.recv().map_err(|err| err.to_string())?;
                            {
                                let database =
                                    NotmuchDb::new_connection(path.as_path(), lib.clone(), false)?;
                                let database_lck = database.inner.read().unwrap();
                                let mut revision_uuid_lck = revision_uuid.write().unwrap();

                                let new_revision = unsafe {
                                    call!(lib, notmuch_database_get_revision)(
                                        *database_lck,
                                        std::ptr::null_mut(),
                                    )
                                };
                                if new_revision > *revision_uuid_lck {
                                    let query_str =
                                        format!("lastmod:{}..{}", *revision_uuid_lck, new_revision);
                                    let query: Query =
                                        Query::new(lib.clone(), &database_lck, &query_str)?;
                                    drop(database_lck);
                                    let iter = query.search()?;
                                    let mut tag_lock = tag_index.write().unwrap();
                                    let mailbox_index_lck = mailbox_index.write().unwrap();
                                    let mailboxes_lck = mailboxes.read().unwrap();
                                    let database = Arc::new(database);
                                    for message in iter {
                                        let msg_id = unsafe {
                                            call!(lib, notmuch_message_get_message_id)(message)
                                        };
                                        let c_str = unsafe { CStr::from_ptr(msg_id) };
                                        let env_hash = {
                                            let mut hasher = DefaultHasher::default();
                                            c_str.hash(&mut hasher);
                                            hasher.finish()
                                        };
                                        if let Some(mailbox_hashes) = mailbox_index_lck.get(&env_hash) {
                                            let tags: (Flag, Vec<String>) =
                                                TagIterator::new(lib.clone(), message)
                                                    .collect_flags_and_tags();
                                            for tag in tags.1.iter() {
                                                let mut hasher = DefaultHasher::new();
                                                hasher.write(tag.as_bytes());
                                                let num = hasher.finish();
                                                if !tag_lock.contains_key(&num) {
                                                    tag_lock.insert(num, tag.clone());
                                                }
                                            }
                                            for &mailbox_hash in mailbox_hashes {
                                                (sender)(
                                                    account_hash,
                                                    BackendEvent::Refresh(RefreshEvent {
                                                        account_hash,
                                                        mailbox_hash,
                                                        kind: NewFlags(env_hash, tags.clone()),
                                                    }),
                                                );
                                            }
                                        } else {
                                            match notmuch_message_into_envelope(
                                                lib.clone(),
                                                index.clone(),
                                                tag_index.clone(),
                                                database.clone(),
                                                message,
                                            ) {
                                                Ok(env) => {
                                                    for (&mailbox_hash, m) in mailboxes_lck.iter() {
                                                        let query_str = format!(
                                                            "{} id:{}",
                                                            m.query_str.as_str(),
                                                            c_str.to_string_lossy()
                                                        );
                                                        let database_lck =
                                                            database.inner.read().unwrap();
                                                        let query: Query = Query::new(
                                                            lib.clone(),
                                                            &database_lck,
                                                            &query_str,
                                                        )?;
                                                        if query.count().unwrap_or(0) > 0 {
                                                            let mut total_lck = m.total.lock().unwrap();
                                                            let mut unseen_lck =
                                                                m.unseen.lock().unwrap();
                                                            *total_lck += 1;
                                                            if !env.is_seen() {
                                                                *unseen_lck += 1;
                                                            }
                                                            (sender)(
                                                                account_hash,
                                                                BackendEvent::Refresh(RefreshEvent {
                                                                    account_hash,
                                                                    mailbox_hash,
                                                                    kind: Create(Box::new(env.clone())),
                                                                }),
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(err) => {
                                                    debug!("could not parse message {:?}", err);
                                                }
                                            }
                                        }
                                    }
                                    drop(query);
                                    let database_lck = database.inner.read().unwrap();
                                    index.write().unwrap().retain(|&env_hash, msg_id| {
                                        let mut message: *mut notmuch_message_t = std::ptr::null_mut();
                                        if let Err(err) = unsafe {
                                            try_call!(
                                                lib,
                                                call!(lib, notmuch_database_find_message)(
                                                    *database_lck,
                                                    msg_id.as_ptr(),
                                                    &mut message as *mut _,
                                                )
                                            )
                                        } {
                                            debug!(err);
                                            false
                                        } else {
                                            if message.is_null() {
                                                if let Some(mailbox_hashes) =
                                                    mailbox_index_lck.get(&env_hash)
                                                {
                                                    for &mailbox_hash in mailbox_hashes {
                                                        let m = &mailboxes_lck[&mailbox_hash];
                                                        let mut total_lck = m.total.lock().unwrap();
                                                        *total_lck = total_lck.saturating_sub(1);
                                                        (sender)(
                                                            account_hash,
                                                            BackendEvent::Refresh(RefreshEvent {
                                                                account_hash,
                                                                mailbox_hash,
                                                                kind: Remove(env_hash),
                                                            }),
                                                        );
                                                    }
                                                }
                                            }
                                            !message.is_null()
                                        }
                                    });

                                    *revision_uuid_lck = new_revision;
                                }
                            }
                        }
                    };

                    if let Err(err) = c(&sender) {
                        (sender)(
                            account_hash,
                            BackendEvent::Refresh(RefreshEvent {
                                account_hash,
                                mailbox_hash: 0,
                                kind: Failure(err),
                            }),
                        );
                    }
                })?;
            Ok(handle.thread().id())
        }
    */

    fn mailboxes(&self) -> ResultFuture<HashMap<MailboxHash, Mailbox>> {
        let ret = Ok(self
            .mailboxes
            .read()
            .unwrap()
            .iter()
            .map(|(k, f)| (*k, BackendMailbox::clone(f)))
            .collect());
        Ok(Box::pin(async { ret }))
    }

    fn operation(&self, hash: EnvelopeHash) -> Result<Box<dyn BackendOp>> {
        Ok(Box::new(NotmuchOp {
            database: Arc::new(Self::new_connection(
                self.path.as_path(),
                self.lib.clone(),
                true,
            )?),
            lib: self.lib.clone(),
            hash,
            index: self.index.clone(),
            bytes: None,
            tag_index: self.tag_index.clone(),
        }))
    }

    fn save(
        &self,
        bytes: Vec<u8>,
        _mailbox_hash: MailboxHash,
        flags: Option<Flag>,
    ) -> ResultFuture<()> {
        let path = self
            .save_messages_to
            .as_ref()
            .unwrap_or(&self.path)
            .to_path_buf();
        MaildirType::save_to_mailbox(path, bytes, flags)?;
        Ok(Box::pin(async { Ok(()) }))
    }

    fn set_flags(
        &mut self,
        env_hashes: EnvelopeHashBatch,
        _mailbox_hash: MailboxHash,
        flags: SmallVec<[(std::result::Result<Flag, String>, bool); 8]>,
    ) -> ResultFuture<()> {
        let database = Self::new_connection(self.path.as_path(), self.lib.clone(), true)?;
        let tag_index = self.tag_index.clone();
        let mut index_lck = self.index.write().unwrap();
        for env_hash in env_hashes.iter() {
            let mut message: *mut notmuch_message_t = std::ptr::null_mut();
            unsafe {
                call!(self.lib, notmuch_database_find_message)(
                    *database.inner.read().unwrap(),
                    index_lck[&env_hash].as_ptr(),
                    &mut message as *mut _,
                )
            };
            if message.is_null() {
                return Err(MeliError::new(format!(
                    "Error, message with path {:?} not found in notmuch database.",
                    index_lck[&env_hash]
                )));
            }

            let tags = TagIterator::new(self.lib.clone(), message).collect::<Vec<&CStr>>();
            //flags.set(f, value);

            macro_rules! cstr {
                ($l:literal) => {
                    &CStr::from_bytes_with_nul_unchecked($l)
                };
            }
            macro_rules! add_tag {
                ($l:literal) => {{
                    add_tag!(unsafe { cstr!($l) })
                }};
                ($l:expr) => {{
                    let l = $l;
                    if tags.contains(l) {
                        continue;
                    }
                    if let Err(err) = unsafe {
                        try_call!(
                            self.lib,
                            call!(self.lib, notmuch_message_add_tag)(message, l.as_ptr())
                        )
                    } {
                        return Err(
                            MeliError::new("Could not set tag.").set_source(Some(Arc::new(err)))
                        );
                    }
                }};
            }
            macro_rules! remove_tag {
                ($l:literal) => {{
                    remove_tag!(unsafe { cstr!($l) })
                }};
                ($l:expr) => {{
                    let l = $l;
                    if !tags.contains(l) {
                        continue;
                    }
                    if let Err(err) = unsafe {
                        try_call!(
                            self.lib,
                            call!(self.lib, notmuch_message_remove_tag)(message, l.as_ptr())
                        )
                    } {
                        return Err(
                            MeliError::new("Could not set tag.").set_source(Some(Arc::new(err)))
                        );
                    }
                }};
            }

            for (f, v) in flags.iter() {
                let value = *v;
                match f {
                    Ok(Flag::DRAFT) if value => add_tag!(b"draft\0"),
                    Ok(Flag::DRAFT) => remove_tag!(b"draft\0"),
                    Ok(Flag::FLAGGED) if value => add_tag!(b"flagged\0"),
                    Ok(Flag::FLAGGED) => remove_tag!(b"flagged\0"),
                    Ok(Flag::PASSED) if value => add_tag!(b"passed\0"),
                    Ok(Flag::PASSED) => remove_tag!(b"passed\0"),
                    Ok(Flag::REPLIED) if value => add_tag!(b"replied\0"),
                    Ok(Flag::REPLIED) => remove_tag!(b"replied\0"),
                    Ok(Flag::SEEN) if value => remove_tag!(b"unread\0"),
                    Ok(Flag::SEEN) => add_tag!(b"unread\0"),
                    Ok(Flag::TRASHED) if value => add_tag!(b"trashed\0"),
                    Ok(Flag::TRASHED) => remove_tag!(b"trashed\0"),
                    Ok(_) => debug!("flags is {:?} value = {}", f, value),
                    Err(tag) if value => {
                        let c_tag = CString::new(tag.as_str()).unwrap();
                        add_tag!(&c_tag.as_ref());
                    }
                    Err(tag) => {
                        let c_tag = CString::new(tag.as_str()).unwrap();
                        add_tag!(&c_tag.as_ref());
                    }
                }
            }

            /* Update message filesystem path. */
            if let Err(err) = unsafe {
                try_call!(
                    self.lib,
                    call!(self.lib, notmuch_message_tags_to_maildir_flags)(message)
                )
            } {
                return Err(MeliError::new("Could not set flags.").set_source(Some(Arc::new(err))));
            }

            let msg_id = unsafe { call!(self.lib, notmuch_message_get_message_id)(message) };
            let c_str = unsafe { CStr::from_ptr(msg_id) };
            if let Some(p) = index_lck.get_mut(&env_hash) {
                *p = c_str.into();
            }
        }
        for (f, v) in flags.iter() {
            if let (Err(tag), true) = (f, v) {
                let hash = tag_hash!(tag);
                tag_index.write().unwrap().insert(hash, tag.to_string());
            }
        }

        Ok(Box::pin(async { Ok(()) }))
    }

    fn tags(&self) -> Option<Arc<RwLock<BTreeMap<u64, String>>>> {
        Some(self.tag_index.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Debug)]
struct NotmuchOp {
    hash: EnvelopeHash,
    index: Arc<RwLock<HashMap<EnvelopeHash, CString>>>,
    tag_index: Arc<RwLock<BTreeMap<u64, String>>>,
    database: Arc<DbConnection>,
    bytes: Option<Vec<u8>>,
    lib: Arc<libloading::Library>,
}

impl BackendOp for NotmuchOp {
    fn as_bytes(&mut self) -> ResultFuture<Vec<u8>> {
        let mut message: *mut notmuch_message_t = std::ptr::null_mut();
        let index_lck = self.index.write().unwrap();
        unsafe {
            call!(self.lib, notmuch_database_find_message)(
                *self.database.inner.read().unwrap(),
                index_lck[&self.hash].as_ptr(),
                &mut message as *mut _,
            )
        };
        let fs_path = unsafe { call!(self.lib, notmuch_message_get_filename)(message) };
        let c_str = unsafe { CStr::from_ptr(fs_path) };
        let mut f = std::fs::File::open(&OsStr::from_bytes(c_str.to_bytes()))?;
        let mut response = Vec::new();
        f.read_to_end(&mut response)?;
        self.bytes = Some(response);
        let ret = Ok(self.bytes.as_ref().unwrap().to_vec());
        Ok(Box::pin(async move { ret }))
    }

    fn fetch_flags(&self) -> ResultFuture<Flag> {
        let mut message: *mut notmuch_message_t = std::ptr::null_mut();
        let index_lck = self.index.write().unwrap();
        unsafe {
            call!(self.lib, notmuch_database_find_message)(
                *self.database.inner.read().unwrap(),
                index_lck[&self.hash].as_ptr(),
                &mut message as *mut _,
            )
        };
        let (flags, _tags) = TagIterator::new(self.lib.clone(), message).collect_flags_and_tags();
        Ok(Box::pin(async move { Ok(flags) }))
    }
}

pub struct MessageIterator<'query> {
    lib: Arc<libloading::Library>,
    messages: *mut notmuch_messages_t,
    _ph: std::marker::PhantomData<*const Query<'query>>,
}

impl Iterator for MessageIterator<'_> {
    type Item = *mut notmuch_message_t;
    fn next(&mut self) -> Option<Self::Item> {
        if self.messages.is_null() {
            None
        } else if unsafe { call!(self.lib, notmuch_messages_valid)(self.messages) } == 1 {
            let ret = Some(unsafe { call!(self.lib, notmuch_messages_get)(self.messages) });
            unsafe {
                call!(self.lib, notmuch_messages_move_to_next)(self.messages);
            }
            ret
        } else {
            self.messages = std::ptr::null_mut();
            None
        }
    }
}

pub struct TagIterator {
    lib: Arc<libloading::Library>,
    tags: *mut notmuch_tags_t,
    message: *mut notmuch_message_t,
}

impl TagIterator {
    fn new(lib: Arc<libloading::Library>, message: *mut notmuch_message_t) -> Self {
        TagIterator {
            tags: unsafe { call!(lib, notmuch_message_get_tags)(message) },
            lib,
            message,
        }
    }

    fn collect_flags_and_tags(self) -> (Flag, Vec<String>) {
        fn flags(path: &CStr) -> Flag {
            let mut flag = Flag::default();
            let mut ptr = path.to_bytes().len().saturating_sub(1);
            let mut is_valid = true;
            while !path.to_bytes()[..ptr + 1].ends_with(b":2,") {
                match path.to_bytes()[ptr] {
                    b'D' => flag |= Flag::DRAFT,
                    b'F' => flag |= Flag::FLAGGED,
                    b'P' => flag |= Flag::PASSED,
                    b'R' => flag |= Flag::REPLIED,
                    b'S' => flag |= Flag::SEEN,
                    b'T' => flag |= Flag::TRASHED,
                    _ => {
                        is_valid = false;
                        break;
                    }
                }
                if ptr == 0 {
                    is_valid = false;
                    break;
                }
                ptr -= 1;
            }

            if !is_valid {
                return Flag::default();
            }

            flag
        }
        let fs_path = unsafe { call!(self.lib, notmuch_message_get_filename)(self.message) };
        let c_str = unsafe { CStr::from_ptr(fs_path) };

        let tags = self.collect::<Vec<&CStr>>();
        let mut flag = Flag::default();
        let mut vec = vec![];
        for t in tags {
            match t.to_bytes() {
                b"draft" => {
                    flag.set(Flag::DRAFT, true);
                }
                b"flagged" => {
                    flag.set(Flag::FLAGGED, true);
                }
                b"passed" => {
                    flag.set(Flag::PASSED, true);
                }
                b"replied" => {
                    flag.set(Flag::REPLIED, true);
                }
                b"unread" => {
                    flag.set(Flag::SEEN, false);
                }
                b"trashed" => {
                    flag.set(Flag::TRASHED, true);
                }
                _other => {
                    vec.push(t.to_string_lossy().into_owned());
                }
            }
        }

        (flag | flags(c_str), vec)
    }
}

impl Iterator for TagIterator {
    type Item = &'static CStr;
    fn next(&mut self) -> Option<Self::Item> {
        if self.tags.is_null() {
            None
        } else if unsafe { call!(self.lib, notmuch_tags_valid)(self.tags) } == 1 {
            let ret = Some(unsafe { CStr::from_ptr(call!(self.lib, notmuch_tags_get)(self.tags)) });
            unsafe {
                call!(self.lib, notmuch_tags_move_to_next)(self.tags);
            }
            ret
        } else {
            self.tags = std::ptr::null_mut();
            None
        }
    }
}

pub struct Query<'s> {
    lib: Arc<libloading::Library>,
    ptr: *mut notmuch_query_t,
    query_str: &'s str,
}

impl<'s> Query<'s> {
    fn new(
        lib: Arc<libloading::Library>,
        database: &*mut notmuch_database_t,
        query_str: &'s str,
    ) -> Result<Self> {
        let query_cstr = std::ffi::CString::new(query_str)?;
        let query: *mut notmuch_query_t =
            unsafe { call!(lib, notmuch_query_create)(*database, query_cstr.as_ptr()) };
        if query.is_null() {
            return Err(MeliError::new("Could not create query. Out of memory?"));
        }
        Ok(Query {
            lib,
            ptr: query,
            query_str,
        })
    }

    fn count(&self) -> Result<u32> {
        let mut count = 0_u32;
        unsafe {
            try_call!(
                self.lib,
                call!(self.lib, notmuch_query_count_messages)(self.ptr, &mut count as *mut _)
            )
            .map_err(|err| err.0)?;
        }
        Ok(count)
    }

    fn search(&'s self) -> Result<MessageIterator<'s>> {
        let mut messages: *mut notmuch_messages_t = std::ptr::null_mut();
        let status = unsafe {
            call!(self.lib, notmuch_query_search_messages)(self.ptr, &mut messages as *mut _)
        };
        if status != 0 {
            return Err(MeliError::new(format!(
                "Search for {} returned {}",
                self.query_str, status,
            )));
        }
        assert!(!messages.is_null());
        Ok(MessageIterator {
            messages,
            lib: self.lib.clone(),
            _ph: std::marker::PhantomData,
        })
    }
}

impl Drop for Query<'_> {
    fn drop(&mut self) {
        unsafe {
            call!(self.lib, notmuch_query_destroy)(self.ptr);
        }
    }
}

fn notmuch_message_insert(
    lib: &libloading::Library,
    index: &RwLock<HashMap<EnvelopeHash, CString>>,
    message: *mut notmuch_message_t,
) -> CString {
    let msg_id = unsafe { call!(lib, notmuch_message_get_message_id)(message) };
    let env_hash = {
        let c_str = unsafe { CStr::from_ptr(msg_id) };
        let mut hasher = DefaultHasher::default();
        c_str.hash(&mut hasher);
        hasher.finish()
    };
    let c_str = unsafe { CStr::from_ptr(msg_id) };
    index.write().unwrap().insert(env_hash, c_str.into());
    c_str.into()
}

fn notmuch_message_into_envelope(
    lib: Arc<libloading::Library>,
    index: Arc<RwLock<HashMap<EnvelopeHash, CString>>>,
    tag_index: Arc<RwLock<BTreeMap<u64, String>>>,
    database: Arc<DbConnection>,
    message: *mut notmuch_message_t,
) -> Result<Envelope> {
    let mut response = Vec::new();
    let fs_path = unsafe { call!(lib, notmuch_message_get_filename)(message) };
    let c_str = unsafe { CStr::from_ptr(fs_path) };
    let mut f = std::fs::File::open(&OsStr::from_bytes(c_str.to_bytes()))?;
    f.read_to_end(&mut response)?;
    let msg_id = unsafe { call!(lib, notmuch_message_get_message_id)(message) };
    let env_hash = {
        let c_str = unsafe { CStr::from_ptr(msg_id) };
        let mut hasher = DefaultHasher::default();
        c_str.hash(&mut hasher);
        hasher.finish()
    };
    {
        let c_str = unsafe { CStr::from_ptr(msg_id) };
        index.write().unwrap().insert(env_hash, c_str.into());
    }
    let op = Box::new(NotmuchOp {
        database,
        lib: lib.clone(),
        hash: env_hash,
        index: index.clone(),
        bytes: Some(response),
        tag_index: tag_index.clone(),
    });
    Envelope::from_token(op, env_hash)
        .map(|mut env| {
            let mut tag_lock = tag_index.write().unwrap();
            let (flags, tags) = TagIterator::new(lib.clone(), message).collect_flags_and_tags();
            for tag in tags {
                let mut hasher = DefaultHasher::new();
                hasher.write(tag.as_bytes());
                let num = hasher.finish();
                if !tag_lock.contains_key(&num) {
                    tag_lock.insert(num, tag);
                }
                env.labels_mut().push(num);
            }
            env.set_flags(flags);
            env
        })
        .chain_err_summary(|| {
            index.write().unwrap().remove(&env_hash);
            format!("could not parse path {:?}", c_str)
        })
}
