/*
 * meli - imap module.
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
use super::*;
use crate::backends::SpecialUsageMailbox;
use std::sync::Arc;

/// Arguments for IMAP watching functions
#[derive(Debug)]
pub struct ImapWatcher {
    pub main_conn: Arc<FutureMutex<ImapConnection>>,
    pub uid_store: Arc<UIDStore>,
    pub mailbox_hashes: BTreeSet<MailboxHash>,
    pub polling_period: std::time::Duration,
    pub server_conf: ImapServerConf,
}

impl BackendWatcher for ImapWatcher {
    fn is_blocking(&self) -> bool {
        false
    }

    fn register_mailbox(
        &mut self,
        mailbox_hash: MailboxHash,
        _urgency: MailboxWatchUrgency,
    ) -> Result<()> {
        self.mailbox_hashes.insert(mailbox_hash);
        Ok(())
    }

    fn set_polling_period(&mut self, period: Option<std::time::Duration>) -> Result<()> {
        if let Some(period) = period {
            self.polling_period = period;
        }
        Ok(())
    }

    fn spawn(mut self: Box<Self>) -> ResultFuture<()> {
        Ok(Box::pin(async move {
            let has_idle: bool = match self.server_conf.protocol {
                ImapProtocol::IMAP {
                    extension_use: ImapExtensionUse { idle, .. },
                } => {
                    idle && self
                        .uid_store
                        .capabilities
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|cap| cap.eq_ignore_ascii_case(b"IDLE"))
                }
                _ => false,
            };
            while let Err(err) = if has_idle {
                self.idle().await
            } else {
                self.poll_with_examine().await
            } {
                let mut main_conn_lck =
                    timeout(self.uid_store.timeout, self.main_conn.lock()).await?;
                if err.kind.is_network() {
                    self.uid_store.is_online.lock().unwrap().1 = Err(err.clone());
                } else {
                    return Err(err);
                }
                debug!("Watch failure: {}", err.to_string());
                match timeout(self.uid_store.timeout, main_conn_lck.connect())
                    .await
                    .and_then(|res| res)
                {
                    Err(err2) => {
                        debug!("Watch reconnect attempt failed: {}", err2.to_string());
                    }
                    Ok(()) => {
                        debug!("Watch reconnect attempt succesful");
                        continue;
                    }
                }
                let account_hash = self.uid_store.account_hash;
                main_conn_lck.add_refresh_event(RefreshEvent {
                    account_hash,
                    mailbox_hash: 0,
                    kind: RefreshEventKind::Failure(err.clone()),
                });
                return Err(err);
            }
            debug!("watch future returning");
            Ok(())
        }))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl ImapWatcher {
    pub async fn idle(&mut self) -> Result<()> {
        debug!("IDLE");
        /* IDLE only watches the connection's selected mailbox. We will IDLE on INBOX and every X
         * minutes wake up and poll the others */
        let ImapWatcher {
            ref main_conn,
            ref uid_store,
            ref mailbox_hashes,
            ref polling_period,
            ref server_conf,
            ..
        } = self;
        let mut connection = ImapConnection::new_connection(server_conf, uid_store.clone());
        connection.connect().await?;
        let mailbox_hash: MailboxHash = match uid_store
            .mailboxes
            .lock()
            .await
            .values()
            .find(|f| f.parent.is_none() && (f.special_usage() == SpecialUsageMailbox::Inbox))
            .map(|f| f.hash)
        {
            Some(h) => h,
            None => {
                return Err(MeliError::new("INBOX mailbox not found in local mailbox index. meli may have not parsed the IMAP mailboxes correctly"));
            }
        };
        let mut response = Vec::with_capacity(8 * 1024);
        let select_response = connection
            .examine_mailbox(mailbox_hash, &mut response, true)
            .await?
            .unwrap();
        {
            let mut uidvalidities = uid_store.uidvalidity.lock().unwrap();

            if let Some(v) = uidvalidities.get(&mailbox_hash) {
                if *v != select_response.uidvalidity {
                    if uid_store.keep_offline_cache {
                        #[cfg(not(feature = "sqlite3"))]
                        let mut cache_handle = super::cache::DefaultCache::get(uid_store.clone())?;
                        #[cfg(feature = "sqlite3")]
                        let mut cache_handle = super::cache::Sqlite3Cache::get(uid_store.clone())?;
                        cache_handle.clear(mailbox_hash, &select_response)?;
                    }
                    connection.add_refresh_event(RefreshEvent {
                        account_hash: uid_store.account_hash,
                        mailbox_hash,
                        kind: RefreshEventKind::Rescan,
                    });
                    /*
                    uid_store.uid_index.lock().unwrap().clear();
                    uid_store.hash_index.lock().unwrap().clear();
                    uid_store.byte_cache.lock().unwrap().clear();
                    */
                }
            } else {
                uidvalidities.insert(mailbox_hash, select_response.uidvalidity);
            }
        }
        let mailboxes: HashMap<MailboxHash, ImapMailbox> = {
            let mailboxes_lck = timeout(uid_store.timeout, uid_store.mailboxes.lock()).await?;
            let mut ret = mailboxes_lck.clone();
            ret.retain(|k, _| mailbox_hashes.contains(k));
            ret
        };
        for (h, mailbox) in mailboxes.iter() {
            if mailbox_hash == *h {
                continue;
            }
            Self::examine_updates(mailbox, &mut connection, &uid_store).await?;
        }
        connection.send_command(b"IDLE").await?;
        let mut blockn = ImapBlockingConnection::from(connection);
        let mut watch = std::time::Instant::now();
        /* duration interval to send heartbeat */
        const _10_MINS: std::time::Duration = std::time::Duration::from_secs(10 * 60);
        /* duration interval to check other mailboxes for changes */
        loop {
            let line = match timeout(
                Some(std::cmp::min(*polling_period, _10_MINS)),
                blockn.as_stream(),
            )
            .await
            {
                Ok(Some(line)) => line,
                Ok(None) => {
                    debug!("IDLE connection dropped: {:?}", &blockn.err());
                    blockn.conn.connect().await?;
                    let mut main_conn_lck = timeout(uid_store.timeout, main_conn.lock()).await?;
                    main_conn_lck.connect().await?;
                    continue;
                }
                Err(_) => {
                    /* Timeout */
                    blockn.conn.send_raw(b"DONE").await?;
                    blockn
                        .conn
                        .read_response(&mut response, RequiredResponses::empty())
                        .await?;
                    blockn.conn.send_command(b"IDLE").await?;
                    let mut main_conn_lck = timeout(uid_store.timeout, main_conn.lock()).await?;
                    main_conn_lck.connect().await?;
                    continue;
                }
            };
            let now = std::time::Instant::now();
            if now.duration_since(watch) >= *polling_period {
                /* Time to poll all inboxes */
                let mut conn = timeout(uid_store.timeout, main_conn.lock()).await?;
                for (_h, mailbox) in mailboxes.iter() {
                    Self::examine_updates(mailbox, &mut conn, &uid_store).await?;
                }
                watch = now;
            }
            if line
                .split_rn()
                .filter(|l| {
                    !l.starts_with(b"+ ")
                        && !l.starts_with(b"* ok")
                        && !l.starts_with(b"* ok")
                        && !l.starts_with(b"* Ok")
                        && !l.starts_with(b"* OK")
                })
                .count()
                == 0
            {
                continue;
            }
            {
                blockn.conn.send_raw(b"DONE").await?;
                blockn
                    .conn
                    .read_response(&mut response, RequiredResponses::empty())
                    .await?;
                for l in line.split_rn().chain(response.split_rn()) {
                    debug!("process_untagged {:?}", String::from_utf8_lossy(&l));
                    if l.starts_with(b"+ ")
                        || l.starts_with(b"* ok")
                        || l.starts_with(b"* ok")
                        || l.starts_with(b"* Ok")
                        || l.starts_with(b"* OK")
                    {
                        debug!("ignore continuation mark");
                        continue;
                    }
                    blockn.conn.process_untagged(l).await?;
                }
                blockn.conn.send_command(b"IDLE").await?;
            }
        }
    }
    pub async fn poll_with_examine(&mut self) -> Result<()> {
        debug!("poll with examine");
        let ImapWatcher {
            ref mailbox_hashes,
            ref uid_store,
            ref polling_period,
            ref server_conf,
            ..
        } = self;
        let mut connection = ImapConnection::new_connection(server_conf, uid_store.clone());
        connection.connect().await?;
        let mailboxes: HashMap<MailboxHash, ImapMailbox> = {
            let mailboxes_lck = timeout(uid_store.timeout, uid_store.mailboxes.lock()).await?;
            let mut ret = mailboxes_lck.clone();
            ret.retain(|k, _| mailbox_hashes.contains(k));
            ret
        };
        loop {
            for (_, mailbox) in mailboxes.iter() {
                Self::examine_updates(mailbox, &mut connection, &uid_store).await?;
            }
            crate::connections::sleep(*polling_period).await;
        }
    }

    pub async fn examine_updates(
        mailbox: &ImapMailbox,
        conn: &mut ImapConnection,
        uid_store: &Arc<UIDStore>,
    ) -> Result<()> {
        if mailbox.no_select {
            return Ok(());
        }
        let mailbox_hash = mailbox.hash();
        debug!("examining mailbox {} {}", mailbox_hash, mailbox.path());
        if let Some(new_envelopes) = conn.resync(mailbox_hash).await? {
            for env in new_envelopes {
                conn.add_refresh_event(RefreshEvent {
                    mailbox_hash,
                    account_hash: uid_store.account_hash,
                    kind: RefreshEventKind::Create(Box::new(env)),
                });
            }
        } else {
            #[cfg(not(feature = "sqlite3"))]
            let mut cache_handle = super::cache::DefaultCache::get(uid_store.clone())?;
            #[cfg(feature = "sqlite3")]
            let mut cache_handle = super::cache::Sqlite3Cache::get(uid_store.clone())?;
            let mut response = Vec::with_capacity(8 * 1024);
            let select_response = conn
                .examine_mailbox(mailbox_hash, &mut response, true)
                .await?
                .unwrap();
            {
                let mut uidvalidities = uid_store.uidvalidity.lock().unwrap();

                if let Some(v) = uidvalidities.get(&mailbox_hash) {
                    if *v != select_response.uidvalidity {
                        if uid_store.keep_offline_cache {
                            cache_handle.clear(mailbox_hash, &select_response)?;
                        }
                        conn.add_refresh_event(RefreshEvent {
                            account_hash: uid_store.account_hash,
                            mailbox_hash,
                            kind: RefreshEventKind::Rescan,
                        });
                        /*
                        uid_store.uid_index.lock().unwrap().clear();
                        uid_store.hash_index.lock().unwrap().clear();
                        uid_store.byte_cache.lock().unwrap().clear();
                        */
                        return Ok(());
                    }
                } else {
                    uidvalidities.insert(mailbox_hash, select_response.uidvalidity);
                }
            }
            if mailbox.is_cold() {
                /* Mailbox hasn't been loaded yet */
                let has_list_status: bool = conn
                    .uid_store
                    .capabilities
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|cap| cap.eq_ignore_ascii_case(b"LIST-STATUS"));
                if has_list_status {
                    conn.send_command(
                        format!(
                            "LIST \"{}\" \"\" RETURN (STATUS (MESSAGES UNSEEN))",
                            mailbox.imap_path()
                        )
                        .as_bytes(),
                    )
                    .await?;
                    conn.read_response(
                        &mut response,
                        RequiredResponses::LIST_REQUIRED | RequiredResponses::STATUS,
                    )
                    .await?;
                    debug!(
                        "list return status out: {}",
                        String::from_utf8_lossy(&response)
                    );
                    for l in response.split_rn() {
                        if !l.starts_with(b"*") {
                            continue;
                        }
                        if let Ok(status) = protocol_parser::status_response(&l).map(|(_, v)| v) {
                            if Some(mailbox_hash) == status.mailbox {
                                if let Some(total) = status.messages {
                                    if let Ok(mut exists_lck) = mailbox.exists.lock() {
                                        exists_lck.clear();
                                        exists_lck.set_not_yet_seen(total);
                                    }
                                }
                                if let Some(total) = status.unseen {
                                    if let Ok(mut unseen_lck) = mailbox.unseen.lock() {
                                        unseen_lck.clear();
                                        unseen_lck.set_not_yet_seen(total);
                                    }
                                }
                                break;
                            }
                        }
                    }
                } else {
                    conn.send_command(b"SEARCH UNSEEN").await?;
                    conn.read_response(&mut response, RequiredResponses::SEARCH)
                        .await?;
                    let unseen_count = protocol_parser::search_results(&response)?.1.len();
                    if let Ok(mut exists_lck) = mailbox.exists.lock() {
                        exists_lck.clear();
                        exists_lck.set_not_yet_seen(select_response.exists);
                    }
                    if let Ok(mut unseen_lck) = mailbox.unseen.lock() {
                        unseen_lck.clear();
                        unseen_lck.set_not_yet_seen(unseen_count);
                    }
                }
                mailbox.set_warm(true);
                return Ok(());
            }

            if select_response.recent > 0 {
                /* UID SEARCH RECENT */
                conn.send_command(b"UID SEARCH RECENT").await?;
                conn.read_response(&mut response, RequiredResponses::SEARCH)
                    .await?;
                let v = protocol_parser::search_results(response.as_slice()).map(|(_, v)| v)?;
                if v.is_empty() {
                    debug!(
                        "search response was empty: {}",
                        String::from_utf8_lossy(&response)
                    );
                    return Ok(());
                }
                let mut cmd = "UID FETCH ".to_string();
                if v.len() == 1 {
                    cmd.push_str(&v[0].to_string());
                } else {
                    cmd.push_str(&v[0].to_string());
                    for n in v.into_iter().skip(1) {
                        cmd.push(',');
                        cmd.push_str(&n.to_string());
                    }
                }
                cmd.push_str(
                    " (UID FLAGS ENVELOPE BODY.PEEK[HEADER.FIELDS (REFERENCES)] BODYSTRUCTURE)",
                );
                conn.send_command(cmd.as_bytes()).await?;
                conn.read_response(&mut response, RequiredResponses::FETCH_REQUIRED)
                    .await?;
            } else if select_response.exists > mailbox.exists.lock().unwrap().len() {
                conn.send_command(
                        format!(
                            "FETCH {}:* (UID FLAGS ENVELOPE BODY.PEEK[HEADER.FIELDS (REFERENCES)] BODYSTRUCTURE)",
                            std::cmp::max(mailbox.exists.lock().unwrap().len(), 1)
                        )
                        .as_bytes(),
                    )
                        .await?;
                conn.read_response(&mut response, RequiredResponses::FETCH_REQUIRED)
                    .await?;
            } else {
                return Ok(());
            }
            debug!(
                "fetch response is {} bytes and {} lines",
                response.len(),
                String::from_utf8_lossy(&response).lines().count()
            );
            let (_, mut v, _) = protocol_parser::fetch_responses(&response)?;
            debug!("responses len is {}", v.len());
            for FetchResponse {
                ref uid,
                ref mut envelope,
                ref mut flags,
                ref references,
                ..
            } in v.iter_mut()
            {
                let uid = uid.unwrap();
                let env = envelope.as_mut().unwrap();
                env.set_hash(generate_envelope_hash(&mailbox.imap_path(), &uid));
                if let Some(value) = references {
                    env.set_references(value);
                }
                let mut tag_lck = uid_store.collection.tag_index.write().unwrap();
                if let Some((flags, keywords)) = flags {
                    env.set_flags(*flags);
                    if !env.is_seen() {
                        mailbox.unseen.lock().unwrap().insert_new(env.hash());
                    }
                    mailbox.exists.lock().unwrap().insert_new(env.hash());
                    for f in keywords {
                        let hash = tag_hash!(f);
                        if !tag_lck.contains_key(&hash) {
                            tag_lck.insert(hash, f.to_string());
                        }
                        env.labels_mut().push(hash);
                    }
                }
            }
            if uid_store.keep_offline_cache {
                if !cache_handle.mailbox_state(mailbox_hash)?.is_none() {
                    cache_handle
                        .insert_envelopes(mailbox_hash, &v)
                        .chain_err_summary(|| {
                            format!(
                                "Could not save envelopes in cache for mailbox {}",
                                mailbox.imap_path()
                            )
                        })?;
                }
            }

            for FetchResponse { uid, envelope, .. } in v {
                if uid.is_none() || envelope.is_none() {
                    continue;
                }
                let uid = uid.unwrap();
                if uid_store
                    .uid_index
                    .lock()
                    .unwrap()
                    .contains_key(&(mailbox_hash, uid))
                {
                    continue;
                }
                let env = envelope.unwrap();
                debug!(
                    "Create event {} {} {}",
                    env.hash(),
                    env.subject(),
                    mailbox.path(),
                );
                uid_store
                    .msn_index
                    .lock()
                    .unwrap()
                    .entry(mailbox_hash)
                    .or_default()
                    .push(uid);
                uid_store
                    .hash_index
                    .lock()
                    .unwrap()
                    .insert(env.hash(), (uid, mailbox_hash));
                uid_store
                    .uid_index
                    .lock()
                    .unwrap()
                    .insert((mailbox_hash, uid), env.hash());
                conn.add_refresh_event(RefreshEvent {
                    account_hash: uid_store.account_hash,
                    mailbox_hash,
                    kind: Create(Box::new(env)),
                });
            }
        }
        Ok(())
    }
}
