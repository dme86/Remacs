//! Resource-conscious, async LSP 3.17 core for the Lua-configured language
//! servers. One child per (workspace, language), never a child per buffer.
//! Reader/writer threads own the blocking pipes, so the UI never blocks on
//! server startup, indexing or IO. Calls are queued until initialized.
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
};
use serde_json::{json, Value};
use url::Url;

use crate::lsp_plugin::ServerConfig;

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_EVENTS_PER_TICK: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding { Utf8, Utf16, Utf32 }

impl Encoding {
    fn from_server(capabilities: &Value) -> Self {
        match capabilities["positionEncoding"].as_str() {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16, // LSP 3.17 default and our advertised encoding
        }
    }
}

pub fn character_offset(line: &[char], col: usize, encoding: Encoding) -> usize {
    line.iter().take(col).map(|ch| match encoding {
        Encoding::Utf8 => ch.len_utf8(),
        Encoding::Utf16 => ch.len_utf16(),
        Encoding::Utf32 => 1
    }).sum()
}

pub fn char_index(line: &[char], offset: usize, encoding: Encoding) -> usize {
    let mut position = 0;
    for (index, ch) in line.iter().enumerate() {
        let len = match encoding {
            Encoding::Utf8 => ch.len_utf8(),
            Encoding::Utf16 => ch.len_utf16(),
            Encoding::Utf32 => 1,
        };
        if position + len > offset { return index; }
        position += len;
    }
    line.len()
}

pub fn absolute(path: &Path) -> PathBuf {
    if let Ok(found) = path.canonicalize() { return found; }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(path)
    };
    path.parent().and_then(|parent| parent.canonicalize().ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or(path)
}

fn uri(path: &Path) -> io::Result<String> {
    Url::from_file_path(absolute(path)).map(|url| url.into())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid file URI"))
}

fn path_from_uri(uri: &str) -> Option<PathBuf> {
    Url::parse(uri).ok()?.to_file_path().ok()
}

fn position_from_byte_offset(text: &str, byte_offset: usize, encoding: Encoding) -> Value {
    let mut row = 0u32;
    let mut column = 0u32;
    for ch in text[..byte_offset].chars() {
        if ch == '\n' {
            row += 1;
            column = 0;
        } else {
            column += match encoding {
                Encoding::Utf8 => ch.len_utf8() as u32,
                Encoding::Utf16 => ch.len_utf16() as u32,
                Encoding::Utf32 => 1,
            }
        }
    }
    json!({ "line": row, "character": column })
}

/// Minimal valid incremental edit. Diff at Unicode character boundaries,
/// keeping matching prefix and suffix so only actual edits cross the pipe.
pub fn incremental_change(old: &str, new: &str, encoding: Encoding) -> Value {
    let mut prefix = 0usize;
    for (before, after) in old.chars().zip(new.chars()) {
        if before != after { break; }
        prefix += before.len_utf8();
    }
    let mut suffix = 0usize;
    for (before, after) in old[prefix..].chars().rev().zip(new[prefix..].chars().rev()) {
        if before != after { break; }
        suffix += before.len_utf8();
    }
    let end_old = old.len() - suffix;
    let end_new = new.len() - suffix;
    json!({
        "range": {
            "start": position_from_byte_offset(old, prefix, encoding),
            "end": position_from_byte_offset(old, end_old, encoding)
        },
        "text": &new[prefix..end_new],
    })
}

pub fn read_frame(reader: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut size: Option<usize> = None;
    let mut total_header = 0usize;
    let mut saw_header = false;
    for _ in 0..40 {
        let mut line = String::new();
        let count = reader.read_line(&mut line)?;
        if count == 0 {
            if !saw_header { return Ok(None); }
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "LSP header truncated"));
        }
        saw_header = true;
        total_header += count;
        if total_header > 16 * 1024 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "LSP header too large"));
        }
        if line.trim().is_empty() { break; }
        if let Some((header, value)) = line.trim().split_once(':') {
            if header.eq_ignore_ascii_case("Content-Length") {
                size = Some(value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid LSP Content-Length")
                })?);
            }
        }
    }
    let size = size.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing LSP Content-Length")
    })?;
    if size > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "LSP frame exceeds 8 MiB"));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(io::Error::other)
}

pub fn write_frame(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind { Definition, Hover }

#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub row: usize,
    pub character: usize,
    pub encoding: Encoding,
}

pub enum Event {
    Definition {
        origin: PathBuf, revision: u64, target: Option<Target>
    },
    Hover {
        origin: PathBuf, revision: u64, text: Option<String>
    },
    Status(String),
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct ServerKey {
    root: PathBuf,
    language: String,
}

struct Synced {
    revision: u64,
    version: i64,
    text: String,
}

#[derive(Clone)]
struct Requested {
    origin: PathBuf,
    revision: u64,
    kind: RequestKind,
    row: usize,
    character: usize, // character offset in the editor's Vec<char>
}

struct Server {
    child: Child,
    writer: SyncSender<Value>,
    reader: Receiver<io::Result<Value>>,
    ready: bool,
    sync_kind: u8,
    encoding: Encoding,
    documents: HashMap<PathBuf, Synced>,
    deferred: Vec<Requested>,
    waiting: HashMap<i64, Requested>,
    next_id: i64,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn start(config: &ServerConfig) -> io::Result<Self> {
        let mut command = Command::new(&config.command[0]);
        command.args(&config.command[1..])
            .current_dir(&config.root)
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or_else(|| io::Error::other("LSP stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("LSP stdout unavailable"))?;
        let (writer, outgoing) = mpsc::sync_channel::<Value>(256);
        let (incoming, reader) = mpsc::sync_channel::<io::Result<Value>>(256);
        thread::Builder::new().name("remacs-lsp-writer".into()).spawn(move || {
            let mut stdin = stdin;
            while let Ok(value) = outgoing.recv() {
                if write_frame(&mut stdin, &value).is_err() { break; }
            }
        })?;
        thread::Builder::new().name("remacs-lsp-reader".into()).spawn(move || {
            let mut stdout = BufReader::new(stdout);
            loop {
                match read_frame(&mut stdout) {
                    Ok(Some(value)) => {
                        if incoming.send(Ok(value)).is_err() { break; }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = incoming.send(Err(error));
                        break;
                    }
                }
            }
        })?;
        let this = Self {
            child, writer, reader, ready: false,
            sync_kind: 2, encoding: Encoding::Utf16,
            documents: HashMap::new(), deferred: Vec::new(),
            waiting: HashMap::new(), next_id: 1,
        };
        let root = uri(&config.root)?;
        this.send(json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": { "name": "remacs", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root,
                "workspaceFolders": [{ "uri": root, "name": config.root.file_name()
                    .map(|name| name.to_string_lossy().into_owned()).unwrap_or_default() }],
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "workspace": {"configuration": true, "workspaceFolders": true},
                    "textDocument": {
                        "synchronization": {"didSave": true},
                        "definition": {"linkSupport": true},
                        "hover": {"contentFormat": ["markdown", "plaintext"]}
                    }
                }
            }
        }))?;
        Ok(this)
    }

    fn send(&self, payload: Value) -> io::Result<()> {
        self.writer.try_send(payload).map_err(|error| {
            io::Error::new(io::ErrorKind::WouldBlock, format!("LSP queue full/disconnected: {error}"))
        })
    }

    fn respond_to_server(&self, request: &Value) {
        let Some(id) = request.get("id") else { return };
        let method = request["method"].as_str().unwrap_or("");
        let result = match method {
            "workspace/configuration" => {
                let length = request["params"]["items"].as_array().map_or(0, Vec::len);
                Value::Array(vec![json!({}); length])
            }
            "workspace/workspaceFolders" => Value::Array(vec![]),
            _ => Value::Null,
        };
        let _ = self.send(json!({"jsonrpc":"2.0", "id":id, "result":result}));
    }

    fn handle_message(&mut self, message: Value) -> Vec<Event> {
        let mut events = Vec::new();
        if message.get("method").is_some() {
            if message.get("id").is_some() { self.respond_to_server(&message); }
            // Diagnostics rendering comes later. Never block on unsolicited messages.
            return events;
        }
        let Some(id) = message["id"].as_i64() else { return events };
        if id == 1 && !self.ready {
            if let Some(error) = message.get("error") {
                events.push(Event::Status(format!("LSP initialization failed: {error}")));
                return events;
            }
            let cap = &message["result"]["capabilities"];
            self.encoding = Encoding::from_server(cap);
            self.sync_kind = if let Some(n) = cap["textDocumentSync"].as_u64() {
                n as u8
            } else {
                cap["textDocumentSync"]["change"].as_u64().unwrap_or(2) as u8
            };
            self.ready = true;
            let _ = self.send(json!({
                "jsonrpc": "2.0", "method": "initialized", "params": {}
            }));
            events.push(Event::Status("rust-analyzer connected".into()));
            return events;
        }
        let Some(request) = self.waiting.remove(&id) else { return events };
        let result = &message["result"];
        if message.get("error").is_some() {
            events.push(Event::Status(format!("LSP request failed: {}", message["error"])));
            return events;
        }
        match request.kind {
            RequestKind::Definition => events.push(Event::Definition {
                origin: request.origin, revision: request.revision,
                target: parse_definition(result, self.encoding),
            }),
            RequestKind::Hover => events.push(Event::Hover {
                origin: request.origin, revision: request.revision,
                text: parse_hover(result),
            }),
        }
        events
    }

    fn sync_document(&mut self, path: &Path, revision: u64, text: impl FnOnce() -> String,
        language: &str) -> io::Result<()> {
        if !self.ready { return Ok(()) }
        let path = absolute(path);
        let uri = uri(&path)?;
        if let Some(previous) = self.documents.get(&path) {
            if previous.revision == revision { return Ok(()); }
        }
        let new_text = text();
        let (version, msg) = match self.documents.get(&path) {
            Some(old) if self.sync_kind == 2 => (
                old.version + 1,
                json!({
                    "jsonrpc":"2.0", "method":"textDocument/didChange",
                    "params":{"textDocument":{"uri":uri,"version":old.version + 1},
                        "contentChanges":[incremental_change(&old.text, &new_text, self.encoding)]}
                }),
            ),
            Some(old) => (
                old.version + 1,
                json!({
                    "jsonrpc":"2.0","method":"textDocument/didChange",
                    "params":{"textDocument":{"uri":uri,"version":old.version + 1},
                        "contentChanges":[{"text":new_text}]}
                }),
            ),
            None => (
                1,
                json!({
                    "jsonrpc":"2.0","method":"textDocument/didOpen",
                    "params":{"textDocument":{"uri":uri,"languageId":language,
                        "version":1,"text":new_text}}
                }),
            )
        };
        self.send(msg)?;
        self.documents.insert(path, Synced { revision, version, text: new_text });
        Ok(())
    }

    fn flush_requests(&mut self) -> io::Result<()> {
        if !self.ready { return Ok(()) }
        let mut pending = std::mem::take(&mut self.deferred);
        for request in pending.drain(..) {
            // Keep the operation pending until the document is opened.
            let Some(doc) = self.documents.get(&request.origin) else {
                self.deferred.push(request);
                continue;
            };
            // Ignore an outdated request if the user has edited meanwhile.
            if doc.revision != request.revision { continue; }
            self.next_id += 1;
            let id = self.next_id;
            let line: Vec<char> = doc.text.split('\n').nth(request.row)
                .unwrap_or("").chars().collect();
            let character = character_offset(&line, request.character, self.encoding);
            let method = match request.kind {
                RequestKind::Definition => "textDocument/definition",
                RequestKind::Hover => "textDocument/hover"
            };
            let payload = json!({
                "jsonrpc":"2.0", "id":id, "method":method,
                "params":{
                    "textDocument":{"uri":uri(&request.origin)?},
                    "position":{"line":request.row,"character":character}
                }
            });
            match self.send(payload) {
                Ok(()) => { self.waiting.insert(id, request); }
                Err(error) => {
                    self.deferred.push(request);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

pub struct Manager {
    servers: HashMap<ServerKey, Server>,
    failed: HashSet<ServerKey>,
}

impl Default for Manager {
    fn default() -> Self {
        Self { servers: HashMap::new(), failed: HashSet::new() }
    }
}

impl Manager {
    fn ensure(&mut self, config: &ServerConfig) -> io::Result<Option<&mut Server>> {
        let key = ServerKey {
            root: absolute(&config.root),
            language: config.language.clone(),
        };
        if self.failed.contains(&key) { return Ok(None); }
        if !self.servers.contains_key(&key) {
            match Server::start(config) {
                Ok(server) => { self.servers.insert(key.clone(), server); }
                Err(error) => {
                    self.failed.insert(key);
                    return Err(io::Error::new(error.kind(),
                        format!("{}: {error} (install the language server)",
                            config.command[0])));
                }
            }
        }
        Ok(self.servers.get_mut(&key))
    }

    pub fn sync(&mut self, config: &ServerConfig, path: &Path, revision: u64,
        text: impl FnOnce() -> String) -> io::Result<()> {
        if let Some(server) = self.ensure(config)? {
            server.sync_document(path, revision, text, &config.language)?;
        }
        Ok(())
    }

    pub fn request(&mut self, config: &ServerConfig, path: &Path, revision: u64,
        row: usize, col: usize, kind: RequestKind) -> io::Result<bool> {
        let Some(server) = self.ensure(config)? else { return Ok(false); };
        server.deferred.push(Requested {
            origin: absolute(path), revision, row, character: col, kind
        });
        Ok(true)
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut result = Vec::new();
        let mut expired = Vec::new();
        for (key, server) in &mut self.servers {
            for _ in 0..MAX_EVENTS_PER_TICK {
                match server.reader.try_recv() {
                    Ok(Ok(value)) => result.extend(server.handle_message(value)),
                    Ok(Err(error)) => result.push(Event::Status(format!(
                        "LSP protocol error: {error}"
                    ))),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    result.push(Event::Status(format!(
                        "{} LSP exited ({status})", key.language
                    )));
                    expired.push(key.clone());
                }
                Err(error) => {
                    result.push(Event::Status(format!("LSP process error: {error}")));
                    expired.push(key.clone());
                }
                Ok(None) => {}
            }
        }
        for key in expired {
            self.servers.remove(&key);
            self.failed.insert(key);
        }
        result
    }

    pub fn flush(&mut self, present: &HashSet<PathBuf>) {
        for server in self.servers.values_mut() {
            if server.ready {
                let missing: Vec<_> = server.documents.keys().filter(|path| !present.contains(*path))
                    .cloned().collect();
                for path in missing {
                    if let Ok(uri) = uri(&path) {
                        if server.send(json!({
                            "jsonrpc":"2.0","method":"textDocument/didClose",
                            "params":{"textDocument":{"uri":uri}}
                        })).is_ok() {
                            server.documents.remove(&path);
                        }
                    }
                }
                let _ = server.flush_requests();
            }
        }
    }

    pub fn state(&self, config: &ServerConfig) -> &'static str {
        let key = ServerKey { root: absolute(&config.root), language: config.language.clone() };
        if self.failed.contains(&key) { return "error"; }
        match self.servers.get(&key) {
            Some(server) if server.ready => "ready",
            Some(_) => "starting",
            None => "idle"
        }
    }
}

fn parse_definition(result: &Value, encoding: Encoding) -> Option<Target> {
    let item = if let Some(items) = result.as_array() { items.first()? } else { result };
    if item.is_null() { return None; }
    let (uri, range) = if item.get("targetUri").is_some() {
        (item["targetUri"].as_str()?, &item["targetSelectionRange"])
    } else {
        (item["uri"].as_str()?, &item["range"])
    };
    Some(Target {
        path: path_from_uri(uri)?,
        row: range["start"]["line"].as_u64()? as usize,
        character: range["start"]["character"].as_u64()? as usize,
        encoding,
    })
}

fn parse_hover(result: &Value) -> Option<String> {
    if result.is_null() { return None; }
    let contents = &result["contents"];
    let text = if let Some(value) = contents.as_str() {
        value.to_owned()
    } else if let Some(value) = contents["value"].as_str() {
        value.to_owned()
    } else if let Some(items) = contents.as_array() {
        items.iter().filter_map(|item| item.as_str()
            .or_else(|| item["value"].as_str()))
            .collect::<Vec<_>>().join("\n")
    } else { return None };
    let text: String = text.chars().take(5000).collect();
    if text.trim().is_empty() { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn lsp_framing_accepts_multibyte_json_and_rejects_oversize() {
        let val = json!({"jsonrpc":"2.0","method":"window/logMessage",
            "params":{"message":"é 🦀"}});
        let mut output = Vec::new();
        write_frame(&mut output, &val).unwrap();
        let mut input = BufReader::new(Cursor::new(output));
        assert_eq!(read_frame(&mut input).unwrap(), Some(val));
        assert_eq!(read_frame(&mut input).unwrap(), None);
        assert!(read_frame(&mut BufReader::new(Cursor::new(
            b"Content-Length: 99999999\r\n\r\n".to_vec()
        ))).is_err());
    }

    #[test]
    fn precise_incremental_edits_use_unicode_utf16_offsets() {
        let patch = incremental_change("fn 🦀() {}\n", "fn 🦀(x) {}\n", Encoding::Utf16);
        assert_eq!(patch["range"]["start"], json!({"line":0,"character":7}));
        assert_eq!(patch["range"]["end"], json!({"line":0,"character":7}));
        assert_eq!(patch["text"], "x");
        let chars: Vec<char> = "🦀hi".chars().collect();
        assert_eq!(character_offset(&chars, 1, Encoding::Utf16), 2);
        assert_eq!(char_index(&chars, 2, Encoding::Utf16), 1);
        assert_eq!(char_index(&chars, 1, Encoding::Utf16), 0);
        assert_eq!(character_offset(&chars, 1, Encoding::Utf8), 4);
    }

    #[test]
    fn parses_definition_location_and_location_link() {
        let first = json!({"uri":"file:///tmp/lib.rs",
            "range":{"start":{"line":3,"character":6}}});
        let result = parse_definition(&json!([first]), Encoding::Utf16).unwrap();
        assert!(result.path.ends_with("lib.rs"));
        assert_eq!((result.row,result.character),(3,6));
        let link = json!({"targetUri":"file:///tmp/main.rs",
            "targetSelectionRange":{"start":{"line":5,"character":2}}});
        assert_eq!(parse_definition(&link,Encoding::Utf16).unwrap().row,5);
        assert!(parse_definition(&Value::Null, Encoding::Utf16).is_none());
    }

    #[test]
    fn hover_parses_markdown_and_plaintext() {
        assert_eq!(parse_hover(&json!({
            "contents":{"kind":"markdown","value":"**struct** Example"}
        })),Some("**struct** Example".into()));
        assert!(parse_hover(&Value::Null).is_none());
    }
}
