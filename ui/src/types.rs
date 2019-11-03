/*
 * meli - ui crate.
 *
 * Copyright 2017-2018 Manos Pitsidianakis
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
extern crate serde;
#[macro_use]
mod helpers;
pub use self::helpers::*;

use super::execute::Action;
use super::terminal::*;

use melib::backends::FolderHash;
use melib::{EnvelopeHash, RefreshEvent};
use std;
use std::fmt;
use std::thread;
use uuid::Uuid;

#[derive(Debug)]
pub enum StatusEvent {
    DisplayMessage(String),
    BufClear,
    BufSet(String),
    UpdateStatus(String),
}

/// `ThreadEvent` encapsulates all of the possible values we need to transfer between our threads
/// to the main process.
#[derive(Debug)]
pub enum ThreadEvent {
    ThreadJoin(thread::ThreadId),
    /// User input.
    Input(Key),
    /// User input and input as raw bytes.
    InputRaw((Key, Vec<u8>)),
    /// A watched folder has been refreshed.
    RefreshMailbox(Box<RefreshEvent>),
    UIEvent(UIEvent),
    /// A thread has updated some of its information
    Pulse,
    //Decode { _ }, // For gpg2 signature check
}

impl From<RefreshEvent> for ThreadEvent {
    fn from(event: RefreshEvent) -> Self {
        ThreadEvent::RefreshMailbox(Box::new(event))
    }
}

#[derive(Debug)]
pub enum ForkType {
    /// Already finished fork, we only want to restore input/output
    Finished,
    /// Embed pty
    Embed,
    Generic(std::process::Child),
    NewDraft(File, std::process::Child),
}

#[derive(Debug)]
pub enum NotificationType {
    INFO,
    ERROR,
    NewMail,
}

#[derive(Debug)]
pub enum UIEvent {
    Input(Key),
    ExInput(Key),
    InsertInput(Key),
    EmbedInput((Key, Vec<u8>)),
    RefreshMailbox((usize, FolderHash)), //view has changed to FolderHash mailbox
    ChildStatusExited(nix::unistd::Pid, i32),
    ChildStatusStopped(nix::unistd::Pid),
    ChildStatusContinued(nix::unistd::Pid),
    Resize,
    /// Force redraw.
    Fork(ForkType),
    ChangeMailbox(usize),
    ChangeMode(UIMode),
    Command(String),
    Notification(Option<String>, String, Option<NotificationType>),
    Action(Action),
    StatusEvent(StatusEvent),
    MailboxUpdate((usize, FolderHash)), // (account_idx, mailbox_idx)
    ComponentKill(Uuid),
    StartupCheck(FolderHash),
    RefreshEvent(Box<RefreshEvent>),
    EnvelopeUpdate(EnvelopeHash),
    EnvelopeRename(EnvelopeHash, EnvelopeHash), // old_hash, new_hash
    EnvelopeRemove(EnvelopeHash),
}

impl From<RefreshEvent> for UIEvent {
    fn from(event: RefreshEvent) -> Self {
        UIEvent::RefreshEvent(Box::new(event))
    }
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum UIMode {
    Normal,
    Insert,
    /// Forward input to an embed pseudoterminal.
    Embed,
    Execute,
    Fork,
}

impl fmt::Display for UIMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                UIMode::Normal => "NORMAL",
                UIMode::Insert => "INSERT",
                UIMode::Execute => "EX",
                UIMode::Fork => "FORK",
                UIMode::Embed => "EMBED",
            }
        )
    }
}

/// An event notification that is passed to Entities for handling.
pub struct Notification {
    _title: String,
    _content: String,

    _timestamp: std::time::Instant,
}
