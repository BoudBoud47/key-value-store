//! Simple in-memory key/value storee responds to command line arguments
use crate::engine::KvsEngine;
use crate::{MyError, Result};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::BTreeMap;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{prelude::*, BufReader, BufWriter, SeekFrom, Write};
use std::ops::Range;
use std::path::PathBuf;

/// The size of the log file needed before compaction occurs
const COMPACT_BYTES: u64 = 1024;

/// The `KvStore` stores string key/value pairs.
///
/// Key/value pairs are stored in a `HashMap` in memory and not persisted to disk.
///
/// Example:
///
/// ```rust
/// # use kvs::{MyError, Result, KvStore};
/// # use kvs::KvsEngine;
/// # use std::env::current_dir;
/// # fn try_main() -> Result<()> {
///
/// let mut store = KvStore::new()?;
/// store.set("key".to_owned(), "value".to_owned());
/// let val = store.get("key".to_owned())?;
/// assert_eq!(val, Some("value".to_owned()));
///
/// # Ok(())
/// # }
/// ```
pub struct KvStore {
    writer: BufWriter<File>,
    reader: BufReader<File>,
    index: BTreeMap<String, Pointer>,
    path: PathBuf,
    uncompacted: u64,
}

impl KvsEngine for KvStore {
    /// Sets the value of a string key to a string.
    ///
    /// If the key already exists, the previous value will be overwritten.
    fn set(&mut self, key: String, value: String) -> Result<()> {
        let command = Command::set(key.clone(), value.clone());
        let initial_offset = self.writer.seek(SeekFrom::End(0))?;
        self.writer.write_all(b"\r\n")?;
        serde_json::to_writer(&mut self.writer, &command)?;
        self.writer.flush()?;
        let new_offset = self.writer.seek(SeekFrom::End(0))?;
        if let Some(pointer) = self
            .index
            .insert(key.clone(), (initial_offset..new_offset).into())
        {
            self.uncompacted += pointer.len;
            //println!("Uncompacted {:?}", self.uncompacted);
        }
        if new_offset > COMPACT_BYTES {
            self.compact()?;
        }

        Ok(())
    }

    /// Gets the string value of a given string key.
    ///
    /// Returns `None` if the given key does not exist.
    fn get(&mut self, key: String) -> Result<Option<String>> {
        self.reader.seek(SeekFrom::Start(0))?;
        if let Some(pointer) = self.index.get(&key) {
            self.reader.seek(SeekFrom::Start(pointer.pos))?;
            let cmd_reader = (&mut self.reader).take(pointer.len);
            if let Command::Set { value, .. } = serde_json::from_reader(cmd_reader)? {
                Ok(Some(value))
            } else {
                Err(MyError::KeyNotFound)
            }
        } else {
            Ok(None)
        }
    }

    /// Remove a given key.
    fn remove(&mut self, key: String) -> Result<()> {
        self.writer.seek(SeekFrom::End(0))?;
        let command = Command::remove(key.clone());
        match self.index.remove(&key) {
            Some(_x) => {
                serde_json::to_writer(&mut self.writer, &command)?;
                self.writer.write_all(b"\r\n")?;
                self.writer.flush()?;
                return Ok(());
            }
            None => return Err(MyError::KeyNotFound),
        }
    }
}

impl KvStore {
    /// Creates a `KvStore`.
    pub fn new() -> Result<Self> {
        let cwd = std::env::current_dir()?;
        KvStore::open(cwd.as_path())
    }

    /// Open the KvStore at a given path. Return the KvStore.
    pub fn open(path: impl Into<PathBuf>) -> Result<KvStore> {
        let mut path = path.into();
        std::fs::create_dir_all(&path)?;

        path.push("log");
        path.set_extension("json");

        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(false)
            .open(&path)?;

        let mut kv = KvStore {
            writer: BufWriter::new(file),
            reader: BufReader::new(OpenOptions::new().read(true).open(&path)?),
            index: BTreeMap::new(),
            path,
            uncompacted: 0,
        };

        kv.read_file()?;
        Ok(kv)
    }

    /// Read file and load history of command from the log
    fn read_file(&mut self) -> Result<()> {
        let mut buf_reader = BufReader::new(OpenOptions::new().read(true).open(&self.path)?);
        let mut initial_offset = buf_reader.seek(SeekFrom::Start(0))?;

        let mut stream = serde_json::Deserializer::from_reader(buf_reader).into_iter::<Command>();

        while let Some(command) = stream.next() {
            let new_offset = stream.byte_offset() as u64;
            match command? {
                Command::Set { key, .. } => {
                    if let Some(pointer) = self
                        .index
                        .insert(key.to_string(), (initial_offset..new_offset).into())
                    {
                        self.uncompacted += pointer.len;
                    }
                }
                Command::Remove { key } => {
                    if let Some(_pointer) = self.index.remove(key.as_str()) {
                        // the "remove" command itself can be deleted in the next compaction.
                        // so we add its length to `uncompacted`.
                        self.uncompacted += new_offset - initial_offset;
                    }
                }
            };
            initial_offset = new_offset;
        }
        //println!("Uncompacted {:?}", self.uncompacted);
        Ok(())
    }

    /// Compact file when when the size exceeds the configured one. Compact == remove remove the entries for identical keys
    fn compact(&mut self) -> Result<()> {
        let mut path = std::env::current_dir()?;
        path.push("compacted_log");
        path.set_extension("json");

        let temp_file = OpenOptions::new().write(true).create(true).open(&path)?;

        let mut writer_temp_file = BufWriter::new(temp_file);
        self.reader.seek(SeekFrom::Start(0))?;
        for (_key, pointer) in &mut self.index {
            self.reader.seek(SeekFrom::Start(pointer.pos))?;
            let mut cmd_reader = (&mut self.reader).take(pointer.len);
            let _len = std::io::copy(&mut cmd_reader, &mut writer_temp_file)?;
        }
        writer_temp_file.flush()?;

        //self.reader = BufReader::new(OpenOptions::new().read(true).open(&path)?);
        //self.writer = writer_temp_file;

        std::fs::remove_file(&self.path)?;
        std::fs::rename(&path, &self.path)?;
        self.uncompacted = 0;
        //self.path = path;
        Ok(())
    }
}

/// Command is an enum with each possible command of the database. Each enum
/// command will be serialized to a log file and used as the basis for populating/
/// updating an in-memory key/value store.
#[derive(Serialize, Deserialize, Debug)]
pub enum Command {
    Set { key: String, value: String },
    Remove { key: String },
}

impl Command {
    fn set(key: String, value: String) -> Command {
        Command::Set { key, value }
    }

    // fn get(key: String) -> Command {
    //     Command::Get { key }
    // }

    fn remove(key: String) -> Command {
        Command::Remove { key }
    }
}

/// Represents the position and length of a json-serialized command in the log.
#[derive(Clone, Debug)]
struct Pointer {
    pos: u64,
    len: u64,
}

impl From<Range<u64>> for Pointer {
    fn from(range: Range<u64>) -> Self {
        Pointer {
            pos: range.start,
            len: range.end - range.start,
        }
    }
}
