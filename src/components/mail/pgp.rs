/*
 * meli
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
use std::io::Write;
use std::process::{Command, Stdio};

pub fn verify_signature(a: &Attachment, context: &mut Context) -> Vec<u8> {
    match melib::signatures::verify_signature(a) {
        Ok((bytes, sig)) => {
            let bytes_file = MeliFile::create_temp_file(&bytes, None, None, true, true);
            let signature_file = MeliFile::create_temp_file(sig, None, None, true, true);
            match Command::new(
                context
                    .settings
                    .pgp
                    .gpg_binary
                    .as_ref()
                    .map(String::as_str)
                    .unwrap_or("gpg2"),
            )
            .args(&[
                "--output",
                "-",
                "--verify",
                signature_file.path.to_str().unwrap(),
                bytes_file.path.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            {
                Ok(gpg) => {
                    return gpg.wait_with_output().unwrap().stderr;
                }
                Err(err) => {
                    context.replies.push_back(UIEvent::Notification(
                        Some(format!(
                            "Failed to launch {} to verify PGP signature",
                            context
                                .settings
                                .pgp
                                .gpg_binary
                                .as_ref()
                                .map(String::as_str)
                                .unwrap_or("gpg2"),
                        )),
                        format!(
                            "{}\nsee meli.conf(5) for configuration setting pgp.gpg_binary",
                            &err
                        ),
                        Some(NotificationType::Error(melib::error::ErrorKind::External)),
                    ));
                }
            }
        }
        Err(err) => {
            context.replies.push_back(UIEvent::Notification(
                Some("Could not verify signature.".to_string()),
                err.to_string(),
                Some(NotificationType::Error(err.kind)),
            ));
        }
    }
    Vec::new()
}

/// Returns multipart/signed
pub fn sign(
    a: AttachmentBuilder,
    gpg_binary: Option<&str>,
    pgp_key: Option<&str>,
) -> Result<AttachmentBuilder> {
    let mut command = Command::new(gpg_binary.unwrap_or("gpg2"));
    command.args(&[
        "--digest-algo",
        "sha512",
        "--output",
        "-",
        "--detach-sig",
        "--armor",
    ]);
    if let Some(key) = pgp_key {
        command.args(&["--local-user", key]);
    }
    let a: Attachment = a.into();
    let mut gpg = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let sig_attachment = {
        gpg.stdin
            .as_mut()
            .unwrap()
            .write_all(&melib::signatures::convert_attachment_to_rfc_spec(
                a.into_raw().as_bytes(),
            ))
            .unwrap();
        let gpg = gpg.wait_with_output().unwrap();
        Attachment::new(ContentType::PGPSignature, Default::default(), gpg.stdout)
    };

    let a: AttachmentBuilder = a.into();
    let parts = vec![a, sig_attachment.into()];
    let boundary = ContentType::make_boundary(&parts);
    Ok(Attachment::new(
        ContentType::Multipart {
            boundary: boundary.into_bytes(),
            kind: MultipartType::Signed,
            parts: parts.into_iter().map(|a| a.into()).collect::<Vec<_>>(),
        },
        Default::default(),
        Vec::new(),
    )
    .into())
}
