/* message.rs
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
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use gio::prelude::*;
use lazy_static::lazy_static;
use uuid::Uuid;

use super::attachment::Attachment;
use crate::message::electronicmail::ElectronicMail;
use crate::message::outlook::OutlookMessage;

const EML_MIME_TYPES: [&str; 1] = ["message/rfc822"];

const MSG_MIME_TYPES: [&str; 2] = ["application/vnd.ms-outlook", "application/x-ole-storage"];

/// The folder attachments are extracted to, under the runtime directory of the
/// user. Not taken from the application, so that a frontend does not have to
/// provide it.
const TEMP_FOLDER_NAME: &str = "mailviewer";

lazy_static! {
  pub static ref TEMP_FOLDER: PathBuf = {
    // XDG_RUNTIME_DIR is not always set (ssh sessions, containers), glib then
    // falls back to the user cache directory, which is user owned too.
    let mut path = glib::user_runtime_dir();
    let uuid = Uuid::new_v4().simple().to_string();
    path.push(TEMP_FOLDER_NAME);
    if !path.exists() {
      if let Err(e) = fs::create_dir(path.clone()) {
        log::error!("Error while creating {:?} : {}", path.to_str(), e);
      }
    }
    path.push(uuid);
    path
  };
}

/// Attachment folders that have not been touched for this long are leftovers.
/// A more recent one may belong to another instance running at the same time.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

fn clean_stale_folders(root: &Path, keep: &Path, older_than: Duration, now: SystemTime) {
  let entries = match fs::read_dir(root) {
    Ok(entries) => entries,
    Err(e) => {
      log::debug!("read_dir({:?}) : {}", root, e);
      return;
    }
  };

  for entry in entries.flatten() {
    let path = entry.path();
    if path == keep || !is_session_folder(&entry) || !is_older_than(&entry, older_than, now) {
      continue;
    }
    match fs::remove_dir_all(&path) {
      Ok(()) => log::debug!("removed stale folder {:?}", path),
      Err(e) => log::warn!("remove_dir_all({:?}) : {}", path, e),
    }
  }
}

/// Session folders are named after a UUID in its simple form, so anything that
/// is not 32 hexadecimal digits was put there by someone else.
fn is_session_folder(entry: &fs::DirEntry) -> bool {
  if !entry.file_type().is_ok_and(|t| t.is_dir()) {
    return false;
  }
  entry
    .file_name()
    .to_str()
    .is_some_and(|name| name.len() == 32 && name.chars().all(|c| c.is_ascii_hexdigit()))
}

fn is_older_than(entry: &fs::DirEntry, older_than: Duration, now: SystemTime) -> bool {
  entry
    .metadata()
    .and_then(|m| m.modified())
    .ok()
    .and_then(|modified| now.duration_since(modified).ok())
    .is_some_and(|age| age >= older_than)
}

pub trait Message {
  fn parse(&mut self, cancellable: Option<&gio::Cancellable>) -> Result<(), Box<dyn Error>>;
  fn from(&self) -> String;
  fn to(&self) -> String;
  fn subject(&self) -> String;
  fn date(&self) -> String;
  fn attachments(&self) -> Vec<Attachment>;
  fn body_html(&self) -> Option<String>;
  fn body_text(&self) -> Option<String>;
}

#[derive(PartialEq, Debug)]
#[repr(u8)]
pub enum MessageType {
  Eml = 0,
  Msg = 1,
}

pub struct MessageParser {
  parser: Box<dyn Message + Send>,
  #[allow(dead_code)]
  message_type: MessageType,
}

impl MessageParser {
  pub async fn new(
    file: &gio::File,
    cancellable: Option<&gio::Cancellable>,
  ) -> Result<Self, Box<dyn Error>> {
    let content = Self::message_content(file, cancellable).await?;
    let (content_type, _) = gio::content_type_guess(file.path(), Some(content.as_slice()));

    log::debug!(
      "MessageParser::new() message_content {:?}: {:?}",
      file.path(),
      content_type
    );

    // gio::content_type_guess() detects EML as text/plain if file == /dev/stdin,
    // so we assume != MSG => EML when file
    let message_type = if MSG_MIME_TYPES.contains(&content_type.as_str()) {
      MessageType::Msg
    } else {
      MessageType::Eml
    };

    Ok(Self {
      parser: if message_type == MessageType::Msg {
        Box::new(OutlookMessage::new(content))
      } else {
        Box::new(ElectronicMail::new(content))
      },
      message_type,
    })
  }

  pub fn supported_mime_types() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::with_capacity(EML_MIME_TYPES.len() + MSG_MIME_TYPES.len());
    v.extend(EML_MIME_TYPES.iter().copied());
    v.extend(MSG_MIME_TYPES.iter().copied());
    v
  }

  #[allow(dead_code)]
  async fn message_type(file: &gio::File) -> Result<MessageType, Box<dyn Error>> {
    let file_info = file
      .query_info_future(
        gio::FILE_ATTRIBUTE_STANDARD_CONTENT_TYPE.as_str(),
        gio::FileQueryInfoFlags::NONE,
        glib::Priority::DEFAULT,
      )
      .await?;

    let content_type = file_info.content_type().unwrap_or_default();
    let content_type = content_type.as_str();
    log::debug!(
      "MessageParser::message_type({}) content type: {}",
      file.peek_path().unwrap().display(),
      content_type
    );

    if EML_MIME_TYPES.contains(&content_type) {
      return Ok(MessageType::Eml);
    }

    if MSG_MIME_TYPES.contains(&content_type) {
      return Ok(MessageType::Msg);
    }

    Err(
      format!(
        "File {} has an unsupported content type: {}",
        file.peek_path().unwrap().display(),
        content_type
      )
      .into(),
    )
  }

  async fn message_content(
    file: &gio::File,
    cancellable: Option<&gio::Cancellable>,
  ) -> Result<Vec<u8>, Box<dyn Error>> {
    let input_stream = file.read_future(glib::Priority::DEFAULT).await?;

    let read_input_stream = async || -> Result<Vec<u8>, Box<dyn Error>> {
      let mut out: Vec<u8> = Vec::new();
      loop {
        if let Some(cancellable) = cancellable {
          cancellable.set_error_if_cancelled()?;
        }
        let buf = input_stream
          .read_bytes_future(8192, glib::Priority::DEFAULT)
          .await?;
        if buf.is_empty() {
          break;
        }
        out.extend_from_slice(&buf);
      }
      Ok(out)
    };

    let input_stream_result = read_input_stream().await;
    input_stream.close_future(glib::Priority::DEFAULT).await?;

    input_stream_result
  }

  /// Removes the attachment folders left behind by previous runs.
  ///
  /// [`Self::cleanup`] only runs when the application exits normally, so a
  /// crash or a logout leaves the extracted attachments on disk. Call this
  /// once at startup.
  pub fn cleanup_stale() {
    let Some(root) = TEMP_FOLDER.parent() else {
      return;
    };
    clean_stale_folders(root, &TEMP_FOLDER, STALE_AFTER, SystemTime::now());
  }

  pub fn cleanup() {
    log::debug!("MessageParser::cleanup()");
    if TEMP_FOLDER.exists() {
      log::debug!("remove_dir_all({:?})", TEMP_FOLDER.to_str());
      fs::remove_dir_all(TEMP_FOLDER.to_path_buf()).unwrap_or_else(|err| {
        log::error!("Error while removing {:?} : {}", TEMP_FOLDER.to_str(), err);
      });
    }
  }

  pub fn to_local_date(date: &Option<gmime::DateTime>) -> String {
    if let Some(date) = date {
      match date.to_local() {
        Ok(local_date) => match local_date.format("%Y-%m-%d %H:%M:%S %z") {
          Ok(formatted) => formatted.into(),
          Err(_) => String::new(),
        },
        Err(_) => String::new(),
      }
    } else {
      String::new()
    }
  }
}

impl Message for MessageParser {
  fn parse(&mut self, cancellable: Option<&gio::Cancellable>) -> Result<(), Box<dyn Error>> {
    self.parser.parse(cancellable)
  }

  fn from(&self) -> String {
    self.parser.from()
  }

  fn to(&self) -> String {
    self.parser.to()
  }

  fn subject(&self) -> String {
    self.parser.subject()
  }

  fn date(&self) -> String {
    self.parser.date()
  }

  fn attachments(&self) -> Vec<Attachment> {
    self.parser.attachments()
  }

  fn body_html(&self) -> Option<String> {
    self.parser.body_html()
  }

  fn body_text(&self) -> Option<String> {
    self.parser.body_text()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::utils;

  const DAY: Duration = Duration::from_secs(24 * 60 * 60);

  fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("mailviewer-test-{}-{}", name, Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    root
  }

  #[test]
  fn clean_stale_folders_removes_previous_sessions() {
    let root = temp_root("stale");
    let stale = root.join("0123456789abcdef0123456789abcdef");
    let current = root.join("fedcba9876543210fedcba9876543210");
    fs::create_dir(&stale).unwrap();
    fs::create_dir(&current).unwrap();
    fs::write(stale.join("invoice.pdf"), b"attachment").unwrap();

    // Nothing in the folder was touched, so pretend two days went by.
    clean_stale_folders(&root, &current, DAY, SystemTime::now() + 2 * DAY);

    assert!(!stale.exists(), "the previous session was left behind");
    assert!(current.exists(), "the current session was removed");
    fs::remove_dir_all(&root).unwrap();
  }

  #[test]
  fn clean_stale_folders_keeps_recent_sessions() {
    let root = temp_root("recent");
    let other = root.join("0123456789abcdef0123456789abcdef");
    fs::create_dir(&other).unwrap();

    // A folder this recent may belong to a second instance still running.
    clean_stale_folders(&root, &root.join("none"), DAY, SystemTime::now());

    assert!(other.exists(), "a folder in use was removed");
    fs::remove_dir_all(&root).unwrap();
  }

  #[test]
  fn clean_stale_folders_only_touches_its_own_folders() {
    let root = temp_root("foreign");
    let folder = root.join("not-a-uuid");
    let file = root.join("0123456789abcdef0123456789abcdef");
    fs::create_dir(&folder).unwrap();
    fs::write(&file, b"same name, but a file").unwrap();

    clean_stale_folders(&root, &root.join("none"), DAY, SystemTime::now() + 2 * DAY);

    assert!(folder.exists(), "a folder we did not create was removed");
    assert!(file.exists(), "a file was removed");
    fs::remove_dir_all(&root).unwrap();
  }

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
  fn test_content_type_eml() {
    utils::spawn_and_wait_new_ctx(async move {
      let message_type = MessageParser::message_type(&gio::File::for_path("sample.eml")).await;
      assert_eq!(message_type.unwrap(), MessageType::Eml);
    });
  }

  #[test]
  fn test_content_type_msg() {
    utils::spawn_and_wait_new_ctx(async move {
      let message_type = MessageParser::message_type(&gio::File::for_path("sample.msg")).await;
      assert_eq!(message_type.unwrap(), MessageType::Msg);
    });
  }

  #[test]
  fn test_content_guess_eml() {
    let file = gio::File::for_path("sample.eml");
    utils::spawn_and_wait_new_ctx(async move {
      let content = MessageParser::message_content(&file, None).await.unwrap();
      let (content_type, _) = gio::content_type_guess(file.path(), Some(content.as_slice()));
      println!("ContentType: {}", content_type);
      assert_eq!(content_type, "message/rfc822")
    });
  }

  #[test]
  fn test_content_guess_msg() {
    let file = gio::File::for_path("sample.msg");
    utils::spawn_and_wait_new_ctx(async move {
      let content = MessageParser::message_content(&file, None).await.unwrap();
      let (content_type, _) = gio::content_type_guess(file.path(), Some(content.as_slice()));
      println!("ContentType: {}", content_type);
      assert_eq!(content_type, "application/x-ole-storage")
    });
  }
  #[test]
  fn test_sample_eml() {
    let file = gio::File::for_path("sample.eml");

    utils::spawn_and_wait_new_ctx(async move {
      let mut message = MessageParser::new(&file, None).await.expect("File opened");
      message.parse(None).unwrap();
      assert_eq!(message.from(), "John Doe <john@moon.space>");
      assert_eq!(message.to(), "Lucas <lucas@mercure.space>");
      assert_eq!(message.subject(), "Lorem ipsum");
      assert_local_date(&message.date());
      assert_eq!(message.attachments().len(), 1);
      let attachment = &message.attachments()[0];
      assert_eq!(attachment.filename, "Deus_Gnome.png");
      assert_eq!(attachment.content_id, "ii_m2lqbrhv0");
      assert_eq!(attachment.mime_type.as_ref().unwrap(), "image/png");
    });
  }

  #[test]
  fn test_sample_msg() {
    let file = gio::File::for_path("sample.msg");

    utils::spawn_and_wait_new_ctx(async move {
      let mut message = MessageParser::new(&file, None).await.expect("File opened");

      message.parse(None).unwrap();
      assert_eq!(message.from(), "John Doe <john@moon.space>");
      assert_eq!(message.to(), "Lucas <lucas@mercure.space>");
      assert_eq!(message.subject(), "Lorem ipsum");
      assert_eq!(message.date(), "");
      assert_eq!(message.attachments().len(), 3);
      let attachment = &message.attachments()[0];
      assert_eq!(attachment.filename, "image001.png");
      assert_eq!(attachment.content_id, "image001.png"); // same as filename
      assert_eq!(attachment.mime_type.as_ref().unwrap(), "image/png");
    });
  }
}
