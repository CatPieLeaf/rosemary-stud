//! Web views shown by another process.
//!
//! With `STUD_WEBVIEW_SOCKET` set, every `WKWebView` is a window of the
//! QtWebEngine host listening on that socket, and WebKitGTK is never started.
//! This crate's Objective-C side stays what the guest talks to; only what is
//! underneath it moves to the host.
//!
//! Each message is a frame: a little-endian `u32` count of fields, then each
//! field as a little-endian `u32` byte length and its UTF-8 bytes. The first
//! field names the message. Views are named by the id this crate gives them.
//!
//! To the host:
//! - `create view agent developer` and `destroy view`
//! - `scripts view count (source at_start main_only)...`, `handlers view name...`
//! - `agent view agent`
//! - `show view width height`, `hide view`
//! - `load view url method headers_json`, `html view html base_url`
//! - `back view`, `forward view`, `reload view`, `stop view`, `edit view command`
//! - `eval view call script`
//! - `decide decision policy` (1 allows), `dialog_reply call ok text`
//! - `cookies_get call`, `cookie_set call name value domain path secure expires`,
//!   `cookie_delete call name domain path`
//!
//! From the host:
//! - `policy view decision url type main new_window`, which the page waits on
//! - `started view url`, `redirect view url`, `committed view url`,
//!   `finished view url`, `failed view provisional url domain code message`
//! - `response view url status main`
//! - `state view url title loading progress back forward`
//! - `message view name json url`, `eval_result call json`
//! - `dialog view call kind message default url`, which the page waits on
//! - `new_window view url`, `closed view`, `terminated view`
//! - `cookies call json`, `cookie_done call`

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use objc::object::Id;

use crate::send;

/// The variable naming the host's socket.
pub const SOCKET: &str = "STUD_WEBVIEW_SOCKET";

/// Whether web views belong to the host: decided by the environment alone, so
/// that WebKitGTK is never started beside it, even before the first view.
pub fn requested() -> bool {
    static REQUESTED: OnceLock<bool> = OnceLock::new();
    *REQUESTED.get_or_init(|| std::env::var_os(SOCKET).is_some_and(|value| !value.is_empty()))
}

pub struct Remote {
    writer: Mutex<UnixStream>,
}

static REMOTE: OnceLock<Option<Remote>> = OnceLock::new();
static INBOX: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());
static NEXT_CALL: AtomicUsize = AtomicUsize::new(1);

/// The connection to the host, made on first use; `None` without one.
pub fn get() -> Option<&'static Remote> {
    if !requested() {
        return None;
    }
    REMOTE.get_or_init(connect).as_ref()
}

fn connect() -> Option<Remote> {
    let path = std::env::var_os(SOCKET)?;
    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("rosemary: webkit: the web view host did not answer: {error}");
            return None;
        }
    };
    let reader = stream.try_clone().ok()?;
    std::thread::Builder::new()
        .name("web view host".into())
        .spawn(move || read_loop(reader))
        .ok()?;
    appkit::event::add_pump_hook(pump);
    Some(Remote {
        writer: Mutex::new(stream),
    })
}

impl Remote {
    pub fn send(&self, fields: &[&str]) {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for field in fields {
            frame.extend_from_slice(&(field.len() as u32).to_le_bytes());
            frame.extend_from_slice(field.as_bytes());
        }
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(error) = writer.write_all(&frame) {
            crate::engine::warn_once(&format!("the web view host stopped listening: {error}"));
        }
    }
}

/// A number for a call whose answer comes back later.
pub fn next_call() -> usize {
    NEXT_CALL.fetch_add(1, Ordering::Relaxed)
}

fn read_u32(stream: &mut UnixStream) -> Option<u32> {
    let mut bytes = [0u8; 4];
    stream.read_exact(&mut bytes).ok()?;
    Some(u32::from_le_bytes(bytes))
}

fn read_frame(stream: &mut UnixStream) -> Option<Vec<String>> {
    let count = read_u32(stream)?;
    let mut fields = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let length = read_u32(stream)? as usize;
        let mut bytes = vec![0u8; length];
        stream.read_exact(&mut bytes).ok()?;
        fields.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    Some(fields)
}

fn read_loop(mut stream: UnixStream) {
    while let Some(fields) = read_frame(&mut stream) {
        if fields.is_empty() {
            continue;
        }
        INBOX.lock().unwrap_or_else(|e| e.into_inner()).push(fields);
        corefoundation::runloop::main().wake();
    }
    eprintln!("rosemary: webkit: the web view host closed the connection");
}

/// Deliver what the host sent, on the thread the guest's views live on.
fn pump() {
    let messages = std::mem::take(&mut *INBOX.lock().unwrap_or_else(|e| e.into_inner()));
    let pool = objc::arc::pool_push();
    for message in messages {
        match message[0].as_str() {
            "cookies" | "cookie_done" => crate::data_store::remote_event(&message),
            "eval_result" => crate::web_view::remote_eval_result(&message),
            _ => crate::web_view::remote_event(&message),
        }
    }
    crate::engine::run_deferred();
    crate::web_view::remote_tick();
    objc::arc::pool_pop(pool);
}

/// A field of a message, or empty when the host sent fewer.
pub fn field(message: &[String], index: usize) -> &str {
    message.get(index).map_or("", String::as_str)
}

/// A numeric field, or zero.
pub fn number(message: &[String], index: usize) -> i64 {
    field(message, index).parse().unwrap_or(0)
}

/// The Foundation object `json` describes, nil where it is not JSON.
pub fn json_object(json: &str) -> Id {
    if json.is_empty() {
        return std::ptr::null_mut();
    }
    let data = objc::arc::autorelease(
        corefoundation::collections::new_data(json.as_bytes().to_vec()) as Id,
    );
    let class = objc::class::lookup(c"NSJSONSerialization").expect("registered by Foundation");
    // `NSJSONReadingFragmentsAllowed`: a lone string or number is a value too.
    const FRAGMENTS: usize = 1 << 2;
    // SAFETY: `+JSONObjectWithData:options:error:` is `id (Class, SEL, id, NSUInteger, id *)`.
    unsafe {
        send!(objc::class::ptr(class) as Id, "JSONObjectWithData:options:error:",
            data => Id, FRAGMENTS => usize, std::ptr::null_mut::<Id>() => *mut Id; Id)
    }
}

/// `dictionary[key]`, nil for a missing key or a non-dictionary.
pub fn value_for(dictionary: Id, key: &str) -> Id {
    if dictionary.is_null() || !crate::engine::responds(dictionary, "objectForKey:") {
        return std::ptr::null_mut();
    }
    // SAFETY: a dictionary; `-objectForKey:` is `id (id, SEL, id)`.
    unsafe { send!(dictionary, "objectForKey:", crate::engine::string(key) => Id; Id) }
}

/// JSON text for headers, as `{"name": "value", ...}`.
pub fn headers_json(headers: &[(String, String)]) -> String {
    let fields: Vec<String> = headers
        .iter()
        .map(|(name, value)| format!("{}:{}", crate::js::quote(name), crate::js::quote(value)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_through_a_socket() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let remote = Remote {
            writer: Mutex::new(a.try_clone().unwrap()),
        };
        remote.send(&["load", "7", "https://example.com/é", ""]);
        let fields = read_frame(&mut b).unwrap();
        assert_eq!(fields, ["load", "7", "https://example.com/é", ""]);
        drop(remote);
        a.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(read_frame(&mut b).is_none());
    }

    #[test]
    fn headers_become_a_json_object() {
        let headers = vec![("A".to_string(), "b\"c".to_string())];
        assert_eq!(headers_json(&headers), "{\"A\":\"b\\\"c\"}");
        assert_eq!(headers_json(&[]), "{}");
    }
}
