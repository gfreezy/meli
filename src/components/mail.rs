/*
 * meli
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

/*! Entities that handle Mail specific functions.
 */
use super::*;
use melib::backends::{AccountHash, Mailbox, MailboxHash};
use melib::email::{attachment_types::*, attachments::*};
use melib::thread::ThreadNodeHash;

pub mod listing;
pub mod listing2;
pub use crate::listing::*;
pub mod view;
pub use crate::view::*;
mod compose;
pub use self::compose::*;

#[cfg(feature = "gpgme")]
pub mod pgp;

mod status;
pub use self::status::*;

fn get_display_name(context: &Context, account_hash: AccountHash) -> String {
    let settings = context.accounts[&account_hash].settings.account();
    if let Some(d) = settings.display_name.as_ref() {
        format!("{} <{}>", d, settings.identity)
    } else {
        settings.identity.to_string()
    }
}
