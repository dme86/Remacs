//! Resource-conscious, async LSP 3.17 core for the Lua-configured language
//! servers. One child per (workspace, language), never a child per buffer.
//! Reader/writer threads own the blocking pipes, so the UI never blocks on
//! server startup, indexing or IO. Calls are queued until initialized.
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
    time::{Duration, Instant},
};
use url::Url;

use crate::lsp_plugin::ServerConfig;

const MAX_FRAME: usize = 8 * 1024 * 1024;
const MAX_HEADER: usize = 16 * 1024;
const MAX_EVENTS_PER_TICK: usize = 128;
const MAX_INITIAL_EMPTY_RETRIES: u8 = 6; // bounded async retries while rust-analyzer indexes
const MAX_PENDING_REQUESTS: usize = 32;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
    Utf32,
}

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
    line.iter()
        .take(col)
        .map(|ch| match encoding {
            Encoding::Utf8 => ch.len_utf8(),
            Encoding::Utf16 => ch.len_utf16(),
            Encoding::Utf32 => 1,
        })
        .sum()
}

pub fn char_index(line: &[char], offset: usize, encoding: Encoding) -> usize {
    let mut position = 0;
    for (index, ch) in line.iter().enumerate() {
        let len = match encoding {
            Encoding::Utf8 => ch.len_utf8(),
            Encoding::Utf16 => ch.len_utf16(),
            Encoding::Utf32 => 1,
        };
        if position + len > offset {
            return index;
        }
        position += len;
    }
    line.len()
}

pub fn absolute(path: &Path) -> PathBuf {
    if let Ok(found) = path.canonicalize() {
        return found;
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .and_then(|parent| path.file_name().map(|name| parent.join(name)))
        .unwrap_or(path)
}

fn uri(path: &Path) -> io::Result<String> {
    Url::from_file_path(absolute(path))
        .map(|url| url.into())
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
        if before != after {
            break;
        }
        prefix += before.len_utf8();
    }
    let mut suffix = 0usize;
    for (before, after) in old[prefix..].chars().rev().zip(new[prefix..].chars().rev()) {
        if before != after {
            break;
        }
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
    let mut complete_header = false;
    for _ in 0..40 {
        let mut line = String::new();
        // Bound the allocation itself, including servers that never send a newline.
        let count = (&mut *reader)
            .take((MAX_HEADER - total_header + 1) as u64)
            .read_line(&mut line)?;
        if count == 0 {
            if !saw_header {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "LSP header truncated",
            ));
        }
        saw_header = true;
        total_header += count;
        if total_header > MAX_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP header too large",
            ));
        }
        if line.trim().is_empty() {
            complete_header = true;
            break;
        }
        if let Some((header, value)) = line.trim().split_once(':') {
            if header.eq_ignore_ascii_case("Content-Length") {
                size = Some(value.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid LSP Content-Length")
                })?);
            }
        }
    }
    if !complete_header {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP header missing separator",
        ));
    }
    let size = size
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing LSP Content-Length"))?;
    if size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LSP frame exceeds 8 MiB",
        ));
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

fn trace_message(direction: &str, payload: &Value) {
    if cfg!(test) && std::env::var_os("REMACS_LSP_TRACE").is_some() {
        let message = payload.to_string();
        let preview = message.chars().take(1200).collect::<String>();
        eprintln!("LSP {direction}: {preview}");
    }
}

pub fn write_frame(writer: &mut impl Write, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestKind {
    Definition,
    Hover,
    Completion,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub row: usize,
    pub character: usize,
    pub encoding: Encoding,
}

pub enum Event {
    Definition {
        origin: PathBuf,
        revision: u64,
        cursor: (usize, usize),
        target: Option<Target>,
    },
    Hover {
        origin: PathBuf,
        revision: u64,
        cursor: (usize, usize),
        text: Option<String>,
    },
    Completion {
        origin: PathBuf,
        revision: u64,
        cursor: (usize, usize),
        encoding: Encoding,
        list: crate::completion::List,
    },
    Status(String),
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct ServerKey {
    root: PathBuf,
    language: String,
    // A reload can replace a language's executable without changing its
    // workspace. Process and failure identity must include the full argv.
    command: Vec<String>,
}

impl ServerKey {
    fn for_config(config: &ServerConfig) -> Self {
        Self {
            root: absolute(&config.root),
            language: config.language.clone(),
            command: config.command.clone(),
        }
    }
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
    attempts: u8,
    retry_after: Option<Instant>,
    sent_at: Option<Instant>,
}

struct Server {
    child: Child,
    writer: SyncSender<Value>,
    reader: Receiver<io::Result<Value>>,
    ready: bool,
    started_at: Instant,
    initialization_failed: bool,
    sync_kind: u8,
    completion_supported: bool,
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
        command
            .args(&config.command[1..])
            .current_dir(&config.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("LSP stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("LSP stdout unavailable"))?;
        let (writer, outgoing) = mpsc::sync_channel::<Value>(256);
        let (incoming, reader) = mpsc::sync_channel::<io::Result<Value>>(256);
        thread::Builder::new()
            .name("remacs-lsp-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                while let Ok(value) = outgoing.recv() {
                    trace_message(">>>", &value);
                    if let Err(error) = write_frame(&mut stdin, &value) {
                        eprintln!("remacs LSP writer stopped: {error}");
                        break;
                    }
                }
            })?;
        thread::Builder::new()
            .name("remacs-lsp-reader".into())
            .spawn(move || {
                let mut stdout = BufReader::new(stdout);
                loop {
                    match read_frame(&mut stdout) {
                        Ok(Some(value)) => {
                            trace_message("<<<", &value);
                            if incoming.send(Ok(value)).is_err() {
                                break;
                            }
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
            child,
            writer,
            reader,
            ready: false,
            started_at: Instant::now(),
            initialization_failed: false,
            sync_kind: 2,
            completion_supported: false,
            encoding: Encoding::Utf16,
            documents: HashMap::new(),
            deferred: Vec::new(),
            waiting: HashMap::new(),
            next_id: 1,
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
                        "hover": {"contentFormat": ["markdown", "plaintext"]},
                        "completion": {
                            "contextSupport": true,
                            "completionItem": {
                                "snippetSupport": false,
                                "insertReplaceSupport": true
                            },
                            "completionList": {
                                "itemDefaults": ["editRange", "insertTextFormat"]
                            }
                        }
                    }
                }
            }
        }))?;
        Ok(this)
    }

    fn send(&self, payload: Value) -> io::Result<()> {
        self.writer.try_send(payload).map_err(|error| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                format!("LSP queue full/disconnected: {error}"),
            )
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

    fn cancel_requests(&mut self, mut discard: impl FnMut(&Requested) -> bool) {
        self.deferred.retain(|request| !discard(request));
        let mut cancelled = Vec::new();
        self.waiting.retain(|id, request| {
            if discard(request) {
                cancelled.push(*id);
                false
            } else {
                true
            }
        });
        for id in cancelled {
            // Cancellation is best effort; removed request IDs also prevent
            // late responses from triggering stale navigation or retries.
            let _ = self.send(json!({
                "jsonrpc":"2.0", "method":"$/cancelRequest", "params":{"id":id}
            }));
        }
    }

    fn close_document(&mut self, path: &Path) -> io::Result<()> {
        self.cancel_requests(|request| request.origin == path);
        if !self.documents.contains_key(path) {
            return Ok(());
        }
        if self.ready {
            self.send(json!({
                "jsonrpc":"2.0", "method":"textDocument/didClose",
                "params":{"textDocument":{"uri":uri(path)?}}
            }))?;
        }
        // Keep the document if notification backpressure requires a retry.
        self.documents.remove(path);
        Ok(())
    }

    fn queue_request(&mut self, request: Requested) {
        let identical = |pending: &Requested| {
            pending.origin == request.origin
                && pending.kind == request.kind
                && pending.revision == request.revision
                && pending.row == request.row
                && pending.character == request.character
        };
        if self
            .deferred
            .iter()
            .chain(self.waiting.values())
            .any(identical)
        {
            return;
        }
        self.cancel_requests(|pending| {
            pending.origin == request.origin && pending.kind == request.kind
        });
        if self.deferred.len() >= MAX_PENDING_REQUESTS {
            self.deferred.remove(0);
        }
        self.deferred.push(request);
    }

    fn expire_requests(&mut self) -> bool {
        let now = Instant::now();
        let expired = self.waiting.values().any(|request| {
            request.kind != RequestKind::Completion
                && request
                    .sent_at
                    .is_some_and(|sent| now.duration_since(sent) >= REQUEST_TIMEOUT)
        });
        self.cancel_requests(|request| {
            request
                .sent_at
                .is_some_and(|sent| now.duration_since(sent) >= REQUEST_TIMEOUT)
        });
        expired
    }

    fn defer_retry(&mut self, mut request: Requested) {
        request.sent_at = None;
        request.retry_after =
            Some(Instant::now() + Duration::from_millis(300 * request.attempts as u64));
        if self.deferred.len() >= MAX_PENDING_REQUESTS {
            self.deferred.remove(0);
        }
        self.deferred.push(request);
    }

    fn handle_message(&mut self, message: Value) -> Vec<Event> {
        let mut events = Vec::new();
        if message.get("method").is_some() {
            if message.get("id").is_some() {
                self.respond_to_server(&message);
            }
            // Diagnostics rendering comes later. Never block on unsolicited messages.
            return events;
        }
        let Some(id) = message["id"].as_i64() else {
            return events;
        };
        if id == 1 && !self.ready {
            if let Some(error) = message.get("error") {
                self.initialization_failed = true;
                events.push(Event::Status(format!("LSP initialization failed: {error}")));
                return events;
            }
            let cap = &message["result"]["capabilities"];
            self.encoding = Encoding::from_server(cap);
            self.completion_supported = cap["completionProvider"].is_object();
            self.sync_kind = match cap["textDocumentSync"]
                .as_u64()
                .or_else(|| cap["textDocumentSync"]["change"].as_u64())
            {
                Some(1) => 1,
                Some(2) => 2,
                _ => 0, // None is also the default when change support is omitted.
            };
            if let Err(error) = self.send(json!({
                "jsonrpc": "2.0", "method": "initialized", "params": {}
            })) {
                self.initialization_failed = true;
                events.push(Event::Status(format!("LSP initialization failed: {error}")));
                return events;
            }
            self.ready = true;
            if !self.completion_supported {
                // Requests made during startup still complete once, even
                // when this server does not implement completion.
                for request in &self.deferred {
                    if request.kind == RequestKind::Completion {
                        events.push(self.completion_event(request, &Value::Null));
                    }
                }
                self.deferred
                    .retain(|request| request.kind != RequestKind::Completion);
            }
            events.push(Event::Status("Language server connected".into()));
            return events;
        }
        let Some(request) = self.waiting.remove(&id) else {
            return events;
        };
        let result = &message["result"];
        if request.kind == RequestKind::Completion {
            // Completion is invoked often while typing. Empty results and
            // transient cancellation/content-modified errors are final for
            // this snapshot; the next edit can request a fresh list.
            let result = if message.get("error").is_some() {
                &Value::Null
            } else {
                result
            };
            events.push(self.completion_event(&request, result));
            return events;
        }
        if let Some(error) = message.get("error") {
            // -32801 (ContentModified) means the server's semantic snapshot
            // changed while it was processing the request. rust-analyzer can
            // emit this during initial indexing, even without user edits.
            // Retry only this transient error with the same bounded
            // nonblocking backoff as early empty results.
            if error["code"].as_i64() == Some(-32801)
                && request.attempts < MAX_INITIAL_EMPTY_RETRIES
            {
                self.defer_retry(request);
                return events;
            }
            events.push(Event::Status(format!("LSP request failed: {error}")));
            return events;
        }
        let target = if request.kind == RequestKind::Definition {
            parse_definition(result, self.encoding)
        } else {
            None
        };
        let hover = if request.kind == RequestKind::Hover {
            parse_hover(result)
        } else {
            None
        };
        // rust-analyzer returns []/null if definition/hover is queried in
        // the milliseconds between didOpen and indexing the workspace.
        // A bounded, asynchronous retry avoids falsely claiming there is
        // no definition, without making the editor UI wait. Ordinary misses
        // eventually terminate; stale revisions are dropped in flush_requests.
        if target.is_none() && hover.is_none() && request.attempts < MAX_INITIAL_EMPTY_RETRIES {
            self.defer_retry(request);
            return events;
        }
        let cursor = (request.row, request.character);
        match request.kind {
            RequestKind::Definition => events.push(Event::Definition {
                origin: request.origin,
                revision: request.revision,
                cursor,
                target,
            }),
            RequestKind::Hover => events.push(Event::Hover {
                origin: request.origin,
                revision: request.revision,
                cursor,
                text: hover,
            }),
            RequestKind::Completion => unreachable!("completion responses handled above"),
        }
        events
    }

    fn completion_event(&self, request: &Requested, result: &Value) -> Event {
        Event::Completion {
            origin: request.origin.clone(),
            revision: request.revision,
            cursor: (request.row, request.character),
            encoding: self.encoding,
            list: crate::completion::parse(result),
        }
    }

    fn sync_document(
        &mut self,
        path: &Path,
        revision: u64,
        text: impl FnOnce() -> String,
        language: &str,
    ) -> io::Result<()> {
        if !self.ready {
            return Ok(());
        }
        let path = absolute(path);
        let uri = uri(&path)?;
        if let Some(previous) = self.documents.get(&path) {
            if previous.revision == revision {
                return Ok(());
            }
        }
        let new_text = text();
        if self.sync_kind == 0 {
            if let Some(previous) = self.documents.get_mut(&path) {
                // Servers can opt out of changes. Keep the local snapshot for
                // request coordinates and stale-response checks without
                // inventing a server version or sending an unwanted change.
                previous.revision = revision;
                previous.text = new_text;
                self.cancel_requests(|request| {
                    request.origin == path && request.revision != revision
                });
                return Ok(());
            }
        }
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
            ),
        };
        self.send(msg)?;
        self.cancel_requests(|request| request.origin == path && request.revision != revision);
        self.documents.insert(
            path,
            Synced {
                revision,
                version,
                text: new_text,
            },
        );
        Ok(())
    }

    fn flush_requests(&mut self) -> io::Result<()> {
        if !self.ready {
            return Ok(());
        }
        let mut pending = std::mem::take(&mut self.deferred).into_iter();
        while let Some(mut request) = pending.next() {
            if self.waiting.len() >= MAX_PENDING_REQUESTS {
                self.deferred.push(request);
                self.deferred.extend(pending);
                break;
            }
            if request
                .retry_after
                .is_some_and(|when| Instant::now() < when)
            {
                self.deferred.push(request);
                continue;
            }
            // Keep the operation pending until the document is opened.
            let Some(doc) = self.documents.get(&request.origin) else {
                self.deferred.push(request);
                continue;
            };
            // Ignore an outdated request if the user has edited meanwhile.
            if doc.revision != request.revision {
                continue;
            }
            if request.kind == RequestKind::Completion && !self.completion_supported {
                continue;
            }
            self.next_id += 1;
            let id = self.next_id;
            let line: Vec<char> = doc
                .text
                .split('\n')
                .nth(request.row)
                .unwrap_or("")
                .chars()
                .collect();
            let character = character_offset(&line, request.character, self.encoding);
            let method = match request.kind {
                RequestKind::Definition => "textDocument/definition",
                RequestKind::Hover => "textDocument/hover",
                RequestKind::Completion => "textDocument/completion",
            };
            let mut payload = json!({
                "jsonrpc":"2.0", "id":id, "method":method,
                "params":{
                    "textDocument":{"uri":uri(&request.origin)?},
                    "position":{"line":request.row,"character":character}
                }
            });
            if request.kind == RequestKind::Completion {
                payload["params"]["context"] = json!({"triggerKind":1});
            }
            match self.send(payload) {
                Ok(()) => {
                    request.attempts += 1;
                    request.retry_after = None;
                    request.sent_at = Some(Instant::now());
                    self.waiting.insert(id, request);
                }
                Err(error) => {
                    self.deferred.push(request);
                    self.deferred.extend(pending);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Manager {
    servers: HashMap<ServerKey, Server>,
    failed: HashSet<ServerKey>,
}

impl Manager {
    /// Dismiss completion without disturbing navigation/hover operations.
    pub fn cancel_completion(&mut self) {
        for server in self.servers.values_mut() {
            server.cancel_requests(|request| request.kind == RequestKind::Completion);
        }
    }
    /// Initialization is asynchronous; documents must be retried until ready.
    pub fn has_starting_servers(&self) -> bool {
        self.servers
            .values()
            .any(|server| !server.ready && !server.initialization_failed)
    }

    /// Called after a Mason installer finishes: retry formerly missing
    /// executables and replace old language-server processes on next sync.
    pub fn reset_language(&mut self, language: &str) {
        self.failed.retain(|key| key.language != language);
        self.servers.retain(|key, _| key.language != language);
    }

    /// Reconcile a live Lua configuration without restarting unaffected
    /// workspaces. Dropping an obsolete server also cancels its pending work.
    pub fn reconfigure(&mut self, configs: &[ServerConfig]) {
        let retained: HashSet<_> = configs.iter().map(ServerKey::for_config).collect();
        self.servers.retain(|key, _| retained.contains(key));
        self.failed.retain(|key| retained.contains(key));
    }

    fn ensure(&mut self, config: &ServerConfig) -> io::Result<Option<&mut Server>> {
        let key = ServerKey::for_config(config);
        if self.failed.contains(&key) {
            return Ok(None);
        }
        if !self.servers.contains_key(&key) {
            match Server::start(config) {
                Ok(server) => {
                    self.servers.insert(key.clone(), server);
                }
                Err(error) => {
                    self.failed.insert(key);
                    return Err(io::Error::new(
                        error.kind(),
                        format!(
                            "{}: {error} (install the language server)",
                            config.command[0]
                        ),
                    ));
                }
            }
        }
        Ok(self.servers.get_mut(&key))
    }

    pub fn sync(
        &mut self,
        config: &ServerConfig,
        path: &Path,
        revision: u64,
        text: impl FnOnce() -> String,
    ) -> io::Result<()> {
        let key = ServerKey::for_config(config);
        let path = absolute(path);
        // A live configuration can move a path to another language/workspace
        // while its previous server still serves other buffers. Close only the
        // reassigned document and cancel its stale responses there.
        for (previous, server) in &mut self.servers {
            if previous != &key {
                server.close_document(&path)?;
            }
        }
        if let Some(server) = self.ensure(config)? {
            server.sync_document(&path, revision, text, &config.language)?;
        }
        Ok(())
    }

    pub fn request(
        &mut self,
        config: &ServerConfig,
        path: &Path,
        revision: u64,
        row: usize,
        col: usize,
        kind: RequestKind,
    ) -> io::Result<bool> {
        let Some(server) = self.ensure(config)? else {
            return Ok(false);
        };
        if kind == RequestKind::Completion && server.ready && !server.completion_supported {
            return Ok(false);
        }
        server.queue_request(Requested {
            origin: absolute(path),
            revision,
            row,
            character: col,
            kind,
            attempts: 0,
            retry_after: None,
            sent_at: None,
        });
        Ok(true)
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let mut result = Vec::new();
        let mut expired = Vec::new();
        for (key, server) in &mut self.servers {
            if server.expire_requests() {
                result.push(Event::Status(format!(
                    "{} LSP request timed out",
                    key.language
                )));
            }
            for _ in 0..MAX_EVENTS_PER_TICK {
                match server.reader.try_recv() {
                    Ok(Ok(value)) => result.extend(server.handle_message(value)),
                    Ok(Err(error)) => {
                        result.push(Event::Status(format!("LSP protocol error: {error}")))
                    }
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
            if server.initialization_failed {
                expired.push(key.clone());
                continue;
            }
            if !server.ready && server.started_at.elapsed() >= STARTUP_TIMEOUT {
                result.push(Event::Status(format!(
                    "{} LSP initialization timed out",
                    key.language
                )));
                expired.push(key.clone());
                continue;
            }
            match server.child.try_wait() {
                Ok(Some(status)) => {
                    result.push(Event::Status(format!(
                        "{} LSP exited ({status})",
                        key.language
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
            server.cancel_requests(|request| !present.contains(&request.origin));
            if server.ready {
                let missing: Vec<_> = server
                    .documents
                    .keys()
                    .filter(|path| !present.contains(*path))
                    .cloned()
                    .collect();
                for path in missing {
                    let _ = server.close_document(&path);
                }
                let _ = server.flush_requests();
            }
        }
    }
}

fn parse_definition(result: &Value, encoding: Encoding) -> Option<Target> {
    let item = if let Some(items) = result.as_array() {
        items.first()?
    } else {
        result
    };
    if item.is_null() {
        return None;
    }
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
    if result.is_null() {
        return None;
    }
    let contents = &result["contents"];
    let text = if let Some(value) = contents.as_str() {
        value.to_owned()
    } else if let Some(value) = contents["value"].as_str() {
        value.to_owned()
    } else {
        contents
            .as_array()?
            .iter()
            .filter_map(|item| item.as_str().or_else(|| item["value"].as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text: String = text.chars().take(5000).collect();
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn test_server(capacity: usize) -> (Server, Receiver<Value>) {
        // A quiet child keeps process lifecycle real without an installed LSP.
        let child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let (writer, outgoing) = mpsc::sync_channel(capacity);
        let (_, reader) = mpsc::channel();
        (
            Server {
                child,
                writer,
                reader,
                ready: true,
                started_at: Instant::now(),
                initialization_failed: false,
                sync_kind: 2,
                completion_supported: true,
                encoding: Encoding::Utf16,
                documents: HashMap::new(),
                deferred: Vec::new(),
                waiting: HashMap::new(),
                next_id: 1,
            },
            outgoing,
        )
    }

    fn requested(number: usize) -> Requested {
        Requested {
            origin: absolute(&std::env::temp_dir().join(format!("remacs-request-{number}.rs"))),
            revision: 1,
            kind: RequestKind::Definition,
            row: 0,
            character: 0,
            attempts: 0,
            retry_after: None,
            sent_at: None,
        }
    }

    fn open_request_document(server: &mut Server, request: &Requested) {
        server.documents.insert(
            request.origin.clone(),
            Synced {
                revision: request.revision,
                version: 1,
                text: "fn example() {}".into(),
            },
        );
    }

    fn manager_with_server(server: Server) -> (Manager, ServerConfig) {
        let config = ServerConfig {
            language: "rust".into(),
            command: vec!["unused-test-server".into()],
            root: absolute(&std::env::temp_dir()),
        };
        let mut manager = Manager::default();
        manager.servers.insert(
            ServerKey {
                root: config.root.clone(),
                language: config.language.clone(),
                command: config.command.clone(),
            },
            server,
        );
        (manager, config)
    }

    #[test]
    fn completion_requests_use_negotiated_positions_and_invocation_context() {
        for (encoding, character) in [
            (Encoding::Utf16, 3),
            (Encoding::Utf8, 5),
            (Encoding::Utf32, 2),
        ] {
            let (mut server, outgoing) = test_server(16);
            server.encoding = encoding;
            let mut request = requested(0);
            request.kind = RequestKind::Completion;
            request.character = 2;
            open_request_document(&mut server, &request);
            server.documents.get_mut(&request.origin).unwrap().text = "🦀a".into();
            server.queue_request(request);
            server.flush_requests().unwrap();
            let payload = outgoing.try_recv().unwrap();
            assert_eq!(payload["method"], "textDocument/completion");
            assert_eq!(
                payload["params"]["position"],
                json!({"line":0,"character":character})
            );
            assert_eq!(payload["params"]["context"], json!({"triggerKind":1}));
        }
    }

    #[test]
    fn completion_responses_preserve_context_and_empty_results_do_not_retry() {
        for result in [
            Value::Null,
            json!([]),
            json!({"isIncomplete":true,"items":[{"label":"width"}]}),
        ] {
            let (mut server, outgoing) = test_server(16);
            let mut request = requested(0);
            request.kind = RequestKind::Completion;
            request.character = 3;
            open_request_document(&mut server, &request);
            server.queue_request(request.clone());
            server.flush_requests().unwrap();
            let payload = outgoing.try_recv().unwrap();
            let events = server.handle_message(json!({"id":payload["id"],"result":result}));
            assert_eq!(events.len(), 1);
            match &events[0] {
                Event::Completion {
                    origin,
                    revision,
                    cursor,
                    encoding,
                    list,
                } => {
                    assert_eq!(origin, &request.origin);
                    assert_eq!(*revision, request.revision);
                    assert_eq!(*cursor, (0, 3));
                    assert_eq!(*encoding, Encoding::Utf16);
                    assert_eq!(
                        list.incomplete,
                        result["isIncomplete"].as_bool().unwrap_or(false)
                    );
                    assert_eq!(list.items.len(), usize::from(!result["items"].is_null()));
                }
                _ => panic!("expected a completion event"),
            }
            assert!(server.deferred.is_empty());
            assert!(server.waiting.is_empty());
        }
    }

    #[test]
    fn unsupported_completion_is_rejected_and_startup_requests_finish_empty() {
        let (mut server, outgoing) = test_server(16);
        server.ready = false;
        let mut request = requested(0);
        request.kind = RequestKind::Completion;
        server.queue_request(request.clone());
        let events = server.handle_message(json!({"id":1,"result":{"capabilities":{}}}));
        assert!(!server.completion_supported);
        assert!(events.iter().any(|event| matches!(event,
            Event::Completion { list, .. } if list.items.is_empty())));
        assert!(server.deferred.is_empty());
        assert_eq!(outgoing.try_recv().unwrap()["method"], "initialized");
        let (mut manager, config) = manager_with_server(server);
        assert!(!manager
            .request(&config, &request.origin, 1, 0, 0, RequestKind::Completion)
            .unwrap());
        assert!(manager
            .servers
            .values()
            .all(|server| server.deferred.is_empty()));
    }

    #[test]
    fn completion_cancellation_keeps_definition_requests_and_discards_late_results() {
        let (mut server, outgoing) = test_server(16);
        let mut completion = requested(0);
        completion.kind = RequestKind::Completion;
        open_request_document(&mut server, &completion);
        server.queue_request(completion.clone());
        server.queue_request(requested(0));
        server.flush_requests().unwrap();
        let completion_payload = outgoing.try_recv().unwrap();
        let definition_payload = outgoing.try_recv().unwrap();
        let (mut manager, _) = manager_with_server(server);
        manager.cancel_completion();
        assert_eq!(outgoing.try_recv().unwrap()["method"], "$/cancelRequest");
        let server = manager.servers.values_mut().next().unwrap();
        assert_eq!(server.waiting.len(), 1);
        assert!(server
            .waiting
            .contains_key(&definition_payload["id"].as_i64().unwrap()));
        assert!(server
            .handle_message(json!({"id":completion_payload["id"],"result":[{"label":"stale"}]}))
            .is_empty());
    }

    #[test]
    fn completion_errors_are_quiet_and_do_not_retry_outdated_snapshots() {
        for code in [-32800, -32801, -32603] {
            let (mut server, outgoing) = test_server(16);
            let mut request = requested(0);
            request.kind = RequestKind::Completion;
            open_request_document(&mut server, &request);
            server.queue_request(request);
            server.flush_requests().unwrap();
            let payload = outgoing.try_recv().unwrap();
            let events = server.handle_message(
                json!({"id":payload["id"],"error":{"code":code,"message":"cancelled"}}),
            );
            assert_eq!(events.len(), 1);
            assert!(matches!(&events[0], Event::Completion { list, .. } if list.items.is_empty()));
            assert!(server.deferred.is_empty());
        }
    }

    #[test]
    fn silent_initialization_times_out_and_rejects_requests_until_reset() {
        let (mut server, _outgoing) = test_server(16);
        server.ready = false;
        server.started_at = Instant::now() - STARTUP_TIMEOUT;
        server.queue_request(requested(0));
        let (mut manager, config) = manager_with_server(server);
        assert!(manager.has_starting_servers());
        let events = manager.poll();
        assert!(events.iter().any(|event| matches!(event,
            Event::Status(message) if message.contains("initialization timed out"))));
        assert!(manager.servers.is_empty());
        assert!(!manager.has_starting_servers());
        let request = requested(0);
        assert!(!manager
            .request(&config, &request.origin, 1, 0, 0, RequestKind::Definition)
            .unwrap());
        manager.reset_language("rust");
        assert!(manager.failed.is_empty());
    }

    #[test]
    fn initialize_error_is_terminal_and_removes_pending_work() {
        let (mut server, _outgoing) = test_server(16);
        server.ready = false;
        server.queue_request(requested(0));
        let (incoming, reader) = mpsc::channel();
        server.reader = reader;
        incoming
            .send(Ok(json!({"id":1,
            "error":{"code":-32603,"message":"initialization rejected"}})))
            .unwrap();
        let (mut manager, _) = manager_with_server(server);
        let events = manager.poll();
        assert!(events.iter().any(|event| matches!(event,
            Event::Status(message) if message.contains("initialization rejected"))));
        assert!(manager.servers.is_empty());
        assert_eq!(manager.failed.len(), 1);
        assert!(!manager.has_starting_servers());
    }

    #[test]
    fn buffered_initialization_response_is_processed_before_startup_timeout() {
        let (mut server, _outgoing) = test_server(16);
        server.ready = false;
        server.started_at = Instant::now() - STARTUP_TIMEOUT;
        let (incoming, reader) = mpsc::channel();
        server.reader = reader;
        incoming
            .send(Ok(json!({"id":1,
            "result":{"capabilities":{"textDocumentSync":2}}})))
            .unwrap();
        let (mut manager, _) = manager_with_server(server);
        manager.poll();
        assert_eq!(manager.servers.len(), 1);
        assert!(manager.failed.is_empty());
        assert!(!manager.has_starting_servers());
    }

    #[test]
    fn failed_initialized_notification_does_not_report_a_ready_server() {
        let (mut server, _outgoing) = test_server(0);
        server.ready = false;
        server.handle_message(json!({"id":1,"result":{"capabilities":{}}}));
        assert!(server.initialization_failed);
        assert!(!server.ready);
    }

    #[test]
    fn synchronization_capabilities_default_to_none() {
        for (capabilities, expected) in [
            (json!({}), 0),
            (json!({"textDocumentSync":0}), 0),
            (json!({"textDocumentSync":{"openClose":true}}), 0),
            (json!({"textDocumentSync":{"change":1}}), 1),
            (json!({"textDocumentSync":2}), 2),
            (json!({"textDocumentSync":258}), 0),
        ] {
            let (mut server, _outgoing) = test_server(16);
            server.ready = false;
            server.handle_message(json!({"id":1,"result":{"capabilities":capabilities}}));
            assert!(server.ready);
            assert_eq!(server.sync_kind, expected);
        }
    }

    #[test]
    fn sync_none_keeps_current_request_coordinates_without_sending_changes() {
        let (mut server, outgoing) = test_server(16);
        server.sync_kind = 0;
        let mut request = requested(0);
        open_request_document(&mut server, &request);
        server
            .sync_document(&request.origin, 2, || "🦀target".into(), "rust")
            .unwrap();
        assert!(outgoing.try_recv().is_err(), "no didChange for sync None");
        let document = &server.documents[&request.origin];
        assert_eq!(document.revision, 2);
        assert_eq!(document.version, 1);
        assert_eq!(document.text, "🦀target");
        request.revision = 2;
        request.character = 1;
        server.queue_request(request);
        server.flush_requests().unwrap();
        let navigation = outgoing.try_recv().unwrap();
        assert_eq!(navigation["method"], "textDocument/definition");
        assert_eq!(navigation["params"]["position"]["character"], 2);
    }

    #[test]
    fn repeated_navigation_is_deduplicated_and_superseded_requests_are_cancelled() {
        let (mut server, outgoing) = test_server(16);
        let request = requested(0);
        open_request_document(&mut server, &request);
        server.queue_request(request.clone());
        server.queue_request(request.clone());
        assert_eq!(server.deferred.len(), 1);
        server.flush_requests().unwrap();
        let first = outgoing.try_recv().unwrap();
        server.queue_request(request.clone());
        assert!(
            server.deferred.is_empty(),
            "do not duplicate an in-flight gd"
        );
        let mut newer = request;
        newer.character = 3;
        server.queue_request(newer);
        assert!(server.waiting.is_empty());
        let cancellation = outgoing.try_recv().unwrap();
        assert_eq!(cancellation["method"], "$/cancelRequest");
        assert_eq!(cancellation["params"]["id"], first["id"]);
        // A late response cannot navigate back to the superseded target.
        assert!(server
            .handle_message(json!({"id":first["id"], "result":null}))
            .is_empty());
        assert_eq!(server.deferred.len(), 1);
    }

    #[test]
    fn unanswered_requests_are_bounded_and_timeout_releases_capacity() {
        let (mut server, _outgoing) = test_server(256);
        for number in 0..MAX_PENDING_REQUESTS * 2 {
            let request = requested(number);
            open_request_document(&mut server, &request);
            server.queue_request(request);
            server.flush_requests().unwrap();
        }
        assert_eq!(server.waiting.len(), MAX_PENDING_REQUESTS);
        assert_eq!(server.deferred.len(), MAX_PENDING_REQUESTS);
        for request in server.waiting.values_mut() {
            request.sent_at = Some(Instant::now() - REQUEST_TIMEOUT);
        }
        assert!(server.expire_requests());
        assert!(server.waiting.is_empty());
        server.flush_requests().unwrap();
        assert_eq!(server.waiting.len(), MAX_PENDING_REQUESTS);
        assert!(server.deferred.is_empty());
    }

    #[test]
    fn writer_backpressure_preserves_all_deferred_requests() {
        let (mut server, _outgoing) = test_server(0);
        for number in 0..3 {
            let request = requested(number);
            open_request_document(&mut server, &request);
            server.queue_request(request);
        }
        assert!(server.flush_requests().is_err());
        assert_eq!(server.deferred.len(), 3);
        for (number, request) in server.deferred.iter().enumerate() {
            assert_eq!(request.origin, requested(number).origin);
        }
    }

    #[test]
    fn edited_documents_cancel_requests_for_the_old_revision() {
        let (mut server, outgoing) = test_server(16);
        let request = requested(0);
        open_request_document(&mut server, &request);
        server.queue_request(request.clone());
        server.flush_requests().unwrap();
        outgoing.try_recv().unwrap();
        server
            .sync_document(&request.origin, 2, || "fn edited() {}".into(), "rust")
            .unwrap();
        assert!(server.waiting.is_empty());
        assert_eq!(
            outgoing.try_recv().unwrap()["method"],
            "textDocument/didChange"
        );
        assert_eq!(outgoing.try_recv().unwrap()["method"], "$/cancelRequest");
    }

    #[test]
    fn framing_bounds_header_allocation_and_requires_a_separator() {
        let mut oversized = Cursor::new(vec![b'x'; MAX_HEADER * 4]);
        assert!(read_frame(&mut oversized).is_err());
        assert_eq!(oversized.position(), (MAX_HEADER + 1) as u64);
        let header = format!(
            "Content-Length: 2\r\n{}{{}}",
            "X-Test: value\r\n".repeat(39)
        );
        assert!(read_frame(&mut Cursor::new(header)).is_err());
    }

    #[test]
    fn content_modified_error_code_matches_lsp_specification() {
        let error = json!({"code":-32801,"message":"content modified"});
        assert_eq!(error["code"].as_i64(), Some(-32801));
        assert_ne!(error["code"].as_i64(), Some(-32800)); // cancellation is not retryable
    }

    #[test]
    fn early_empty_definition_is_retried_with_backoff_and_limited() {
        // No external language server is needed to check the retry policy.
        let retry = Requested {
            origin: std::env::temp_dir().join("test.rs"),
            revision: 0,
            kind: RequestKind::Definition,
            row: 1,
            character: 3,
            attempts: 1,
            retry_after: Some(Instant::now() + Duration::from_millis(300)),
            sent_at: None,
        };
        assert_eq!(retry.attempts, 1);
        assert!(retry.retry_after.is_some_and(|when| when > Instant::now()));
        assert!(MAX_INITIAL_EMPTY_RETRIES <= MAX_PENDING_REQUESTS as u8);
    }

    #[test]
    fn reassigned_document_closes_on_previous_server_and_cancels_stale_requests() {
        let (mut previous, outgoing) = test_server(16);
        let request = requested(1);
        let path = request.origin.clone();
        open_request_document(&mut previous, &request);
        previous.waiting.insert(7, request);
        let unrelated = requested(2);
        open_request_document(&mut previous, &unrelated);
        previous.deferred.push(unrelated);
        let (mut manager, old_config) = manager_with_server(previous);
        let (replacement, replacement_outgoing) = test_server(16);
        let config = ServerConfig {
            language: "lua".into(),
            command: vec!["different-test-server".into()],
            root: old_config.root.clone(),
        };
        manager
            .servers
            .insert(ServerKey::for_config(&config), replacement);
        manager
            .sync(&config, &path, 2, || "new text".into())
            .unwrap();
        let previous = &manager.servers[&ServerKey::for_config(&old_config)];
        assert!(!previous.documents.contains_key(&path));
        assert!(previous.waiting.is_empty());
        assert_eq!(previous.documents.len(), 1);
        assert_eq!(
            previous.deferred.len(),
            1,
            "unrelated requests stay pending"
        );
        let messages: Vec<_> = outgoing.try_iter().collect();
        assert!(messages
            .iter()
            .any(|message| message["method"] == "$/cancelRequest"));
        assert!(messages
            .iter()
            .any(|message| message["method"] == "textDocument/didClose"));
        assert_eq!(
            replacement_outgoing.try_recv().unwrap()["method"],
            "textDocument/didOpen"
        );
    }

    #[test]
    fn document_reassignment_retries_after_notification_backpressure() {
        let (mut previous, outgoing) = test_server(1);
        let request = requested(3);
        let path = request.origin.clone();
        open_request_document(&mut previous, &request);
        previous.send(json!({"method":"test/blocker"})).unwrap();
        let (mut manager, old_config) = manager_with_server(previous);
        let (replacement, replacement_outgoing) = test_server(16);
        let config = ServerConfig {
            language: "lua".into(),
            command: vec!["different-test-server".into()],
            root: old_config.root.clone(),
        };
        manager
            .servers
            .insert(ServerKey::for_config(&config), replacement);
        assert!(manager
            .sync(&config, &path, 2, || "new text".into())
            .is_err());
        assert!(manager.servers[&ServerKey::for_config(&old_config)]
            .documents
            .contains_key(&path));
        assert!(replacement_outgoing.try_recv().is_err());
        outgoing.try_recv().unwrap();
        manager
            .sync(&config, &path, 2, || "new text".into())
            .unwrap();
        assert_eq!(
            outgoing.try_recv().unwrap()["method"],
            "textDocument/didClose"
        );
        assert_eq!(
            replacement_outgoing.try_recv().unwrap()["method"],
            "textDocument/didOpen"
        );
    }

    #[test]
    fn configuration_reload_keeps_unchanged_process_and_pending_requests() {
        let (mut server, _outgoing) = test_server(16);
        let request = requested(1);
        open_request_document(&mut server, &request);
        server.deferred.push(request);
        let pid = server.child.id();
        let (mut manager, config) = manager_with_server(server);
        manager.reconfigure(std::slice::from_ref(&config));
        let retained = manager
            .servers
            .get(&ServerKey::for_config(&config))
            .unwrap();
        assert_eq!(retained.child.id(), pid);
        assert_eq!(retained.deferred.len(), 1);
    }

    #[test]
    fn configuration_reload_removes_changed_and_disabled_processes() {
        let (server, _outgoing) = test_server(16);
        let (mut manager, config) = manager_with_server(server);
        let mut changed = config.clone();
        changed.command = vec!["replacement-server".into(), "--stdio".into()];
        manager.failed.insert(ServerKey::for_config(&config));
        manager.reconfigure(std::slice::from_ref(&changed));
        assert!(manager.servers.is_empty());
        assert!(
            manager.failed.is_empty(),
            "old command failure must not block new executable"
        );

        let (server, _outgoing) = test_server(16);
        let (mut manager, _) = manager_with_server(server);
        manager.reconfigure(&[]);
        assert!(
            manager.servers.is_empty(),
            "disabled language server must stop"
        );
    }

    #[test]
    fn changed_command_does_not_reuse_old_negative_startup_cache() {
        let (server, _outgoing) = test_server(16);
        let (mut manager, mut config) = manager_with_server(server);
        manager.failed.insert(ServerKey::for_config(&config));
        config.command = vec!["/remacs-test-no-such-executable".into()];
        assert!(
            manager.ensure(&config).is_err(),
            "new argv must attempt startup, not reuse old failure"
        );
        assert!(manager.failed.contains(&ServerKey::for_config(&config)));
    }

    #[test]
    fn successful_mason_install_releases_failed_server_state() {
        let mut manager = Manager::default();
        let rust = ServerKey {
            root: PathBuf::from("/tmp"),
            language: "rust".into(),
            command: vec!["rust-analyzer".into()],
        };
        let lua = ServerKey {
            root: PathBuf::from("/tmp"),
            language: "lua".into(),
            command: vec!["lua-language-server".into()],
        };
        manager.failed.insert(rust.clone());
        manager.failed.insert(lua.clone());
        manager.reset_language("rust");
        assert!(!manager.failed.contains(&rust));
        assert!(
            manager.failed.contains(&lua),
            "unrelated LSP stays unchanged"
        );
    }

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
        )))
        .is_err());
    }

    #[test]
    fn precise_incremental_edits_use_unicode_utf16_offsets() {
        let patch = incremental_change("fn 🦀() {}\n", "fn 🦀(x) {}\n", Encoding::Utf16);
        assert_eq!(patch["range"]["start"], json!({"line":0,"character":6}));
        assert_eq!(patch["range"]["end"], json!({"line":0,"character":6}));
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
        assert_eq!((result.row, result.character), (3, 6));
        let link = json!({"targetUri":"file:///tmp/main.rs",
            "targetSelectionRange":{"start":{"line":5,"character":2}}});
        assert_eq!(parse_definition(&link, Encoding::Utf16).unwrap().row, 5);
        assert!(parse_definition(&Value::Null, Encoding::Utf16).is_none());
    }

    #[test]
    #[ignore = "run in CI with rustup component add rust-analyzer rust-src"]
    fn actual_rust_analyzer_resolves_definition() {
        use std::time::{Duration, Instant};
        let root = std::env::temp_dir().join(format!(
            "remacs-ra-integration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"remacs-lsp-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let text = "pub fn target() -> u32 { 42 }\npub fn caller() -> u32 { target() }\n";
        let source = root.join("src/lib.rs");
        std::fs::write(&source, text).unwrap();
        let config = ServerConfig {
            language: "rust".into(),
            command: vec!["rust-analyzer".into()],
            root: root.canonicalize().unwrap(),
        };
        let reference = text.lines().nth(1).unwrap();
        let column = reference.find("target()").unwrap();
        let mut manager = Manager::default();
        assert!(manager
            .request(&config, &source, 0, 1, column, RequestKind::Definition)
            .unwrap());

        let deadline = Instant::now() + Duration::from_secs(30);
        let present = HashSet::from([absolute(&source)]);
        let mut found = false;
        let mut failure = None;
        while Instant::now() < deadline {
            for event in manager.poll() {
                if let Event::Status(ref status) = event {
                    eprintln!("LSP EVENT: {status}");
                }
                match event {
                    Event::Definition {
                        target: Some(target),
                        ..
                    } => {
                        assert_eq!(absolute(&target.path), absolute(&source));
                        assert_eq!(target.row, 0);
                        found = true;
                    }
                    Event::Status(status)
                        if status.contains("error")
                            || status.contains("exited")
                            || status.contains("failed") =>
                    {
                        failure = Some(status)
                    }
                    _ => {}
                }
            }
            if found || failure.is_some() {
                break;
            }
            manager.sync(&config, &source, 0, || text.into()).unwrap();
            manager.flush(&present);
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(manager);
        std::fs::remove_dir_all(&root).unwrap();
        assert!(failure.is_none(), "rust-analyzer failed: {failure:?}");
        assert!(
            found,
            "rust-analyzer did not resolve a real definition before timeout"
        );
    }

    #[test]
    fn hover_parses_markdown_and_plaintext() {
        assert_eq!(
            parse_hover(&json!({
                "contents":{"kind":"markdown","value":"**struct** Example"}
            })),
            Some("**struct** Example".into())
        );
        assert!(parse_hover(&Value::Null).is_none());
    }

    #[test]
    #[ignore = "run in CI with rustup component add rust-analyzer rust-src"]
    fn actual_rust_analyzer_completes_symbol() {
        let root = std::env::temp_dir().join(format!(
            "remacs-ra-completion-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"),
            "[package]\nname = \"remacs-completion-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
        let text = "pub struct Widget { pub width: u32 }\npub fn caller() { let widget = Widget { width: 42 }; widget.wid }\n";
        let source = root.join("src/lib.rs");
        std::fs::write(&source, text).unwrap();
        let config = ServerConfig {
            language: "rust".into(),
            command: vec!["rust-analyzer".into()],
            root: root.canonicalize().unwrap(),
        };
        let row = 1;
        let line = text.lines().nth(row).unwrap();
        let col = line.find("widget.wid").unwrap() + "widget.wid".len();
        let mut manager = Manager::default();
        let present = HashSet::from([absolute(&source)]);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut next_request = Instant::now();
        let mut found = false;
        let mut failure = None;
        while Instant::now() < deadline {
            for event in manager.poll() {
                match event {
                    Event::Completion { list, encoding, .. } => {
                        if let Some(item) = list.items.iter().find(|item| item.label == "width") {
                            let mut doc = crate::document::Document::from_text(text);
                            doc.row = row;
                            doc.col = col;
                            crate::completion::apply(&mut doc, item, encoding).unwrap();
                            assert!(doc.lsp_text().contains("widget.width"));
                            found = true;
                        }
                    }
                    Event::Status(status)
                        if status.contains("error")
                            || status.contains("exited")
                            || status.contains("failed") =>
                    {
                        failure = Some(status);
                    }
                    _ => {}
                }
            }
            if found || failure.is_some() {
                break;
            }
            manager.sync(&config, &source, 0, || text.into()).unwrap();
            // Indexing can initially produce an empty list. A new invocation
            // gets a fresh semantic snapshot, just as further typing does.
            if Instant::now() >= next_request {
                manager
                    .request(&config, &source, 0, row, col, RequestKind::Completion)
                    .unwrap();
                next_request = Instant::now() + Duration::from_millis(300);
            }
            manager.flush(&present);
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(manager);
        std::fs::remove_dir_all(root).unwrap();
        assert!(failure.is_none(), "rust-analyzer failed: {failure:?}");
        assert!(
            found,
            "rust-analyzer did not complete and insert a real field before timeout"
        );
    }
}
