/* electronicmail.rs
 *
 * Copyright 2024 Alexandre Del Bigio
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */
use std::error::Error;

use encoding_rs::Encoding;
use gio::prelude::*;
use gmime::prelude::Cast;
use gmime::traits::{
  ContentTypeExt, DataWrapperExt, MessageExt, ObjectExt, ParserExt, PartExt, StreamExt,
  StreamMemExt,
};
use gmime::{
  glib, InternetAddressExt, InternetAddressList, InternetAddressListExt, Message, Parser, Part,
  StreamMem,
};

use crate::message::attachment::Attachment;
use crate::message::message::{MessageParser, Protection};

#[allow(unused_variables, dead_code)]
const O_RDONLY: i32 = 0;
#[allow(unused_variables, dead_code)]
pub const O_WRONLY: i32 = 1;
#[allow(unused_variables, dead_code)]
pub const O_RDWR: i32 = 2;
#[allow(unused_variables, dead_code)]
pub const O_CREAT: i32 = 100;
#[allow(unused_variables, dead_code)]
const INVALID_CHARS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

#[derive(Debug, Default, Clone)]
pub struct ElectronicMail {
  data: Vec<u8>,
  pub from: String,
  pub to: String,
  pub cc: String,
  pub bcc: String,
  pub date: Option<gmime::DateTime>,
  pub subject: String,
  pub body_html: Option<String>,
  pub body_text: Option<String>,
  pub attachments: Vec<Attachment>,
  pub protection: Protection,
}

impl ElectronicMail {
  pub fn new(data: Vec<u8>) -> ElectronicMail {
    ElectronicMail {
      data,
      from: String::new(),
      to: String::new(),
      cc: String::new(),
      bcc: String::new(),
      subject: String::new(),
      body_html: None,
      body_text: None,
      date: None,
      attachments: vec![],
      protection: Protection::default(),
    }
  }

  /// A recipient in Bcc was hidden from everyone else, and a recipient in Cc
  /// was not addressed directly. Neither is a To, so neither is folded into
  /// it.
  fn addresses(&self, list: Option<InternetAddressList>) -> String {
    match list {
      Some(list) => self.internet_list(&list),
      None => String::new(),
    }
  }

  fn internet_list(&self, list: &InternetAddressList) -> String {
    let mut addresses: Vec<String> = Vec::new(); // Crée un vecteur vide de String

    for i in 0..list.length() {
      if let Some(addr) = list.address(i) {
        if let Some(address) = InternetAddressExt::to_string(&addr, None, false) {
          addresses.push(address.to_string());
        }
      }
    }
    addresses.join(", ")
  }

  /// What the structure of the message says about itself, without touching a
  /// key: the wrapper of RFC 1847 for PGP/MIME and S/MIME, and the armor of
  /// RFC 4880 for the older style that puts the block in the body.
  fn part_protection(content_type: &gmime::ContentType) -> Option<Protection> {
    if content_type.is_type("multipart", "encrypted") {
      return Some(Protection::Encrypted);
    }
    if content_type.is_type("multipart", "signed") {
      return Some(Protection::Signed);
    }
    // S/MIME puts everything in one part and says which in a parameter.
    if content_type.is_type("application", "pkcs7-mime")
      || content_type.is_type("application", "x-pkcs7-mime")
    {
      return match content_type.parameter("smime-type").as_deref() {
        Some("signed-data") => Some(Protection::Signed),
        // enveloped-data, authenveloped-data, or nothing said.
        _ => Some(Protection::Encrypted),
      };
    }
    None
  }

  fn inline_protection(body: &str) -> Option<Protection> {
    let body = body.trim_start();
    if body.starts_with("-----BEGIN PGP MESSAGE-----") {
      return Some(Protection::Encrypted);
    }
    if body.starts_with("-----BEGIN PGP SIGNED MESSAGE-----") {
      return Some(Protection::Signed);
    }
    None
  }

  fn parse_body(&mut self, message: &Message) {
    let mut html: Option<String> = None;
    message.foreach(|_, current| {
      log::debug!("part() => {:?}", current.content_id());
      if let Some(content_type) = current.content_type() {
        if let Some(found) = Self::part_protection(&content_type) {
          // Encrypted wins: what is inside is not readable from here anyway.
          if found == Protection::Encrypted || self.protection == Protection::None {
            self.protection = found;
          }
        }
      }
      if let Some(part) = current.dynamic_cast_ref::<Part>() {
        if part.is_attachment() {
          self.add_attachment(part);
        } else {
          // Note is_attachment() is false for inline (cid)
          if let Some(content_type) = part.content_type() {
            if content_type.is_type("text", "html") {
              html = Some(self.get_content(part));
            } else if content_type.is_type("text", "plain") {
              self.body_text = Some(self.get_content(part));
            } else {
              self.add_attachment(part);
            }
          }
        }
      }
    });
    if self.protection == Protection::None {
      if let Some(found) = self.body_text.as_deref().and_then(Self::inline_protection) {
        self.protection = found;
      }
    }
    if let Some(html) = html {
      self.body_html = Some(html);
      // for debugging parsed html
      // self.write_debug_html();
    }
  }

  #[allow(dead_code)]
  #[cfg(debug_assertions)]
  fn write_debug_html(&self) {
    if let Some(body) = &self.body_html {
      std::fs::write("body.html", body).unwrap_or_else(|err| {
        log::error!("Failed to write body.html : {}", err);
      });
    }
  }

  fn get_attachment(&self, part: &Part) -> Option<Attachment> {
    let mut content_id: String = "none".to_string();
    let mut mime_type: Option<String> = None;
    if let Some(id) = part.content_id() {
      content_id = id.to_string();
    }
    if let Some(file) = part.filename() {
      let filename = file.to_string();
      if let Some(content_type) = part.content_type() {
        if let Some(parameter) = content_type.mime_type() {
          mime_type = Some(parameter.to_string());
        }
        if let Some(content) = part.content() {
          let stream = StreamMem::new();
          content.write_to_stream(&stream);
          let body = stream.byte_array().unwrap().to_vec();
          stream.close();

          return Some(Attachment {
            content_id,
            filename,
            mime_type,
            body,
          });
        }
      }
    }
    None
  }

  // It seems that gmime-rs has a memory free bug with g_mime_message_get_date()
  fn my_mime_message_get_date(e: &Message) -> Option<glib::DateTime> {
    unsafe {
      glib::translate::from_glib_none(gmime::ffi::g_mime_message_get_date(
        glib::translate::ToGlibPtr::to_glib_none(&e).0,
      ))
    }
  }

  fn get_content(&self, part: &Part) -> String {
    log::debug!(
      "get_content() => part.content_type() {:?}",
      part.content_type()
    );
    log::debug!(
      "get_content() => part.content_encoding() {:?}",
      part.content_encoding()
    );
    log::debug!(
      "get_content() => part.content_disposition() {:?}",
      part.content_disposition()
    );

    if let Some(content) = part.content() {
      let stream = StreamMem::new();
      let size = content.write_to_stream(&stream) as u32;

      if size > 0 {
        let charset = match part.content_type() {
          Some(content_type) => &content_type.parameter("charset").unwrap_or("utf-8".into()),
          None => "utf-8",
        };

        let array: Vec<u8> = stream.byte_array().unwrap().to_vec();
        let encoding = Encoding::for_label(charset.as_bytes());

        if let Some(encoding) = encoding {
          log::debug!("get_content() encoding: {}", encoding.name());
          let (decoded, _, _) = encoding.decode(&array);
          return decoded.to_string();
        } else {
          log::error!("get_content() => to convert {} to string", charset);
        }
      } else {
        log::error!("get_content() => size");
      }
    } else {
      log::error!("get_content() => part.content()");
    }
    String::new()
  }

  fn add_attachment(&mut self, part: &Part) {
    if let Some(attachment) = self.get_attachment(part) {
      log::debug!(
        "add_attachment() => added attachment => {}",
        attachment.filename
      );
      self.attachments.push(attachment);
    } else {
      log::error!(
        "add_attachment() => no attachment => {:?}",
        part.content_id()
      );
    }
  }
}

impl super::message::Message for ElectronicMail {
  fn parse(&mut self, cancellable: Option<&gio::Cancellable>) -> Result<(), Box<dyn Error>> {
    let stream = StreamMem::with_buffer(&self.data);
    let parser = Parser::with_stream(&stream);
    let message = parser.construct_message(None);
    let mut isok = false;

    if let Some(cancellable) = cancellable {
      if let Err(e) = cancellable.set_error_if_cancelled() {
        stream.close();
        return Err(Box::new(e));
      }
    }

    if let Some(eml) = &message {
      isok = true;
      if let Some(from) = &eml.from() {
        self.from = self.internet_list(from);
      }
      self.to = self.addresses(eml.to());
      self.cc = self.addresses(eml.cc());
      self.bcc = self.addresses(eml.bcc());
      if let Some(subject) = &eml.subject() {
        self.subject = subject.to_string();
      }
      self.date = ElectronicMail::my_mime_message_get_date(eml);
      self.parse_body(eml);
    }
    stream.close();

    if !isok {
      log::error!("parse(None) => no message");
      return Err("No message found".into());
    }
    Ok(())
  }

  fn from(&self) -> String {
    self.from.clone()
  }

  fn to(&self) -> String {
    self.to.clone()
  }

  fn subject(&self) -> String {
    self.subject.clone()
  }

  fn date(&self) -> String {
    MessageParser::to_local_date(&self.date)
  }

  fn attachments(&self) -> Vec<Attachment> {
    self.attachments.clone()
  }

  fn cc(&self) -> String {
    self.cc.clone()
  }

  fn bcc(&self) -> String {
    self.bcc.clone()
  }

  fn body_html(&self) -> Option<String> {
    self.body_html.clone()
  }

  fn protection(&self) -> Protection {
    self.protection
  }

  fn body_text(&self) -> Option<String> {
    self.body_text.clone()
  }
}

#[cfg(test)]
mod tests {
  use std::error::Error;
  use std::fs;

  use gio::prelude::*;

  use crate::message::electronicmail::ElectronicMail;
  use crate::message::message::{Message, Protection};
  use crate::utils;

  fn assert_local_date(date: &str) {
    assert_eq!(date.len(), 25, "Unexpected local date format: {date}");
    assert_eq!(
      date.as_bytes()[19],
      b' ',
      "Unexpected local date format: {date}"
    );
    assert!(
      matches!(date.as_bytes()[20], b'+' | b'-'),
      "Missing date offset: {date}"
    );
  }

  #[test]
  fn test_sample() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("sample.eml").unwrap());
    parser.parse(None)?;
    assert_eq!(parser.from, "John Doe <john@moon.space>");
    assert_eq!(parser.to, "Lucas <lucas@mercure.space>");
    assert_eq!(parser.subject, "Lorem ipsum");
    assert_local_date(&parser.date());
    assert_eq!(parser.attachments.len(), 1);
    let attachment = &parser.attachments[0];
    assert_eq!(attachment.filename, "Deus_Gnome.png");
    assert_eq!(attachment.content_id, "ii_m2lqbrhv0");
    assert_eq!(attachment.mime_type.as_ref().unwrap(), "image/png");

    let attachment = attachment.clone();
    utils::spawn_and_wait_new_ctx(async move {
      let _file = attachment
        .write_to_tmp()
        .await
        .unwrap()
        .peek_path()
        .unwrap();
      println!("file => {:?}", _file);
      assert!(_file.is_file());
    });

    Ok(())
  }

  #[test]
  fn test_sample_google() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("tests/test-google.eml").unwrap());
    parser.parse(None)?;
    assert_eq!(parser.from, "Bill Jncjkq <jncjkq@gmail.com>");
    assert_eq!(parser.to, "bookmarks@jncjkq.net");
    assert_eq!(parser.subject, "Test");
    assert_local_date(&parser.date());
    assert_eq!(parser.attachments.len(), 1);
    let attachment = &parser.attachments[0];
    assert_eq!(attachment.filename, "bookmarks-really-short.html");
    assert_eq!(attachment.content_id, "none");
    assert_eq!(attachment.mime_type.as_ref().unwrap(), "text/html");

    Ok(())
  }

  #[test]
  fn test_sample_text() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("tests/text.eml").unwrap());
    parser.parse(None)?;
    assert_eq!(parser.from, "John Doe <john@moon.space>");
    assert_eq!(parser.to, "Lucas <lucas@mercure.space>");
    assert_eq!(parser.subject, "Lorem ipsum");
    assert_local_date(&parser.date());
    assert_ne!(parser.body_text, None);
    assert_eq!(parser.body_html, None);
    assert_eq!(parser.attachments.len(), 0);

    Ok(())
  }
  fn protection_of(path: &str) -> Result<Protection, Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read(path).unwrap());
    parser.parse(None)?;
    Ok(parser.protection())
  }

  #[test]
  fn keeps_the_recipients_apart() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("tests/recipients.eml").unwrap());
    parser.parse(None)?;

    assert_eq!(parser.to(), "Lucas <lucas@mercure.space>");
    assert_eq!(parser.cc(), "Marie <marie@venus.space>");
    assert_eq!(parser.bcc(), "Quiet One <quiet@pluto.space>");
    // The one that matters: someone in blind copy does not turn up as a
    // recipient everyone could see.
    assert!(!parser.to().contains("quiet@pluto.space"));

    Ok(())
  }

  #[test]
  fn reports_pgp_mime_messages() -> Result<(), Box<dyn Error>> {
    assert_eq!(
      protection_of("tests/pgp-encrypted.eml")?,
      Protection::Encrypted
    );
    assert_eq!(protection_of("tests/pgp-signed.eml")?, Protection::Signed);

    Ok(())
  }

  #[test]
  fn reports_pgp_written_into_the_body() -> Result<(), Box<dyn Error>> {
    assert_eq!(
      protection_of("tests/pgp-inline-encrypted.eml")?,
      Protection::Encrypted
    );
    assert_eq!(
      protection_of("tests/pgp-inline-signed.eml")?,
      Protection::Signed
    );

    Ok(())
  }

  #[test]
  fn says_nothing_about_a_plain_message() -> Result<(), Box<dyn Error>> {
    assert_eq!(protection_of("tests/html.eml")?, Protection::None);
    assert_eq!(protection_of("tests/text.eml")?, Protection::None);
    assert_eq!(protection_of("tests/test-google.eml")?, Protection::None);

    Ok(())
  }

  #[test]
  fn test_sample_html() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("tests/html.eml").unwrap());
    parser.parse(None)?;
    assert_eq!(parser.from, "John Doe <john@moon.space>");
    assert_eq!(parser.to, "Lucas <lucas@mercure.space>");
    assert_eq!(parser.subject, "Lorem ipsum");
    assert_local_date(&parser.date());
    assert_eq!(parser.body_text, None);
    assert_ne!(parser.body_html, None);
    assert_eq!(parser.attachments.len(), 0);

    Ok(())
  }

  #[test]
  fn test_sample_php() -> Result<(), Box<dyn Error>> {
    let mut parser = ElectronicMail::new(fs::read("tests/test-php.eml").unwrap());
    parser.parse(None)?;
    assert_eq!(parser.from, "mlemos <mlemos@acm.org>");
    assert_eq!(parser.to, "Manuel Lemos <mlemos@linux.local>");
    assert_eq!(
      parser.subject,
      "Testing Manuel Lemos' MIME E-mail composing and sending PHP class: HTML message"
    );
    assert_local_date(&parser.date());
    assert_ne!(parser.body_text, None);
    assert_ne!(parser.body_html, None);
    assert_eq!(parser.attachments.len(), 3);
    assert_eq!(parser.attachments[0].filename, "logo.gif");
    assert_eq!(
      parser.attachments[0].mime_type.as_ref().unwrap(),
      "image/gif"
    );
    assert_eq!(
      parser.attachments[0].content_id,
      "ae0357e57f04b8347f7621662cb63855.gif"
    );
    assert_eq!(parser.attachments[0].body.len(), 1195);
    assert_eq!(parser.attachments[1].filename, "background.gif");
    assert_eq!(
      parser.attachments[1].mime_type.as_ref().unwrap(),
      "image/gif"
    );
    assert_eq!(
      parser.attachments[1].content_id,
      "4c837ed463ad29c820668e835a270e8a.gif"
    );
    assert_eq!(parser.attachments[1].body.len(), 3265);
    assert_eq!(parser.attachments[2].filename, "attachment.txt");
    assert_eq!(
      parser.attachments[2].mime_type.as_ref().unwrap(),
      "text/plain"
    );
    assert_eq!(parser.attachments[2].content_id, "none");
    assert_eq!(parser.attachments[2].body.len(), 64);
    Ok(())
  }

  #[test]
  fn test_date_timezone() -> Result<(), Box<dyn Error>> {
    let fixtures = [
      ("tests/date-utc.eml", "2026-07-30 16:06:08 +0000"),
      (
        "tests/date-positive-offset.eml",
        "2026-07-30 18:06:08 +0200",
      ),
      (
        "tests/date-negative-offset.eml",
        "2026-07-30 11:06:08 -0500",
      ),
      ("tests/date-bug-report-bst.eml", "2026-07-30 16:06:08 +0000"),
    ];

    let mut local_dates = Vec::with_capacity(fixtures.len());

    for (path, expected_original) in fixtures {
      let mut parser = ElectronicMail::new(fs::read(path).unwrap());
      parser.parse(None)?;

      let date = parser.date.as_ref().expect("date fixture must have a date");
      let original: String = date.format("%Y-%m-%d %H:%M:%S %z")?.into();
      assert_eq!(original, expected_original);
      local_dates.push(parser.date());
    }

    assert_eq!(local_dates[0], local_dates[1]);
    assert_eq!(local_dates[1], local_dates[2]);
    assert_eq!(local_dates[2], local_dates[3]);

    Ok(())
  }
}
