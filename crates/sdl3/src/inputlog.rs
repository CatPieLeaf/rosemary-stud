//! `STUD_INPUT_LOG=<file>`: every input step, with a millisecond timestamp,
//! for a report of keys that stall or stay held. Off unless set; Stud sets it
//! to a file in its logs when started with `STUD_INPUT_LOG=1`. It records
//! every key pressed, typing included, so it is for a test session only.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

struct Log {
    start: Instant,
    file: Mutex<std::fs::File>,
}

fn log() -> Option<&'static Log> {
    static LOG: OnceLock<Option<Log>> = OnceLock::new();
    LOG.get_or_init(|| {
        let path = std::env::var_os("STUD_INPUT_LOG")?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path).ok()?;
        Some(Log { start: Instant::now(), file: Mutex::new(file) })
    })
    .as_ref()
}

pub fn on() -> bool {
    log().is_some()
}

pub fn line(text: impl FnOnce() -> String) {
    let Some(log) = log() else { return };
    let ms = log.start.elapsed().as_secs_f64() * 1000.0;
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("?").to_string();
    let text = text();
    if let Ok(mut file) = log.file.lock() {
        let _ = writeln!(file, "{ms:10.1} [{name}] {text}");
    }
}
