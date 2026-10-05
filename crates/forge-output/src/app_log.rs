//! Forge's own log, kept in memory for the Output panel. Installed in place of a plain
//! `env_logger`: every record still goes to stderr exactly as before, and the ones that
//! pass the filter are also appended to a bounded in-memory buffer.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Lines kept for the Output panel; older ones are dropped.
const CAPACITY: usize = 10_000;

struct Buffer {
    lines: VecDeque<String>,
    /// Lines ever written, so readers can ask for what is new since they last looked.
    total: u64,
}

static BUFFER: Mutex<Buffer> = Mutex::new(Buffer { lines: VecDeque::new(), total: 0 });

struct Tee {
    inner: env_logger::Logger,
}

impl log::Log for Tee {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.inner.matches(record) {
            return;
        }
        self.inner.log(record);
        let time = chrono::Local::now().format("%H:%M:%S");
        push(format!("{time} {:<5} {}: {}", record.level(), record.target(), record.args()));
    }

    fn flush(&self) {
        self.inner.flush();
    }
}

/// Installs the logger. Call once, early in `main`, instead of `builder.init()`.
pub fn install(mut builder: env_logger::Builder) {
    let inner = builder.build();
    let max_level = inner.filter();
    if log::set_boxed_logger(Box::new(Tee { inner })).is_ok() {
        log::set_max_level(max_level);
    }
}

fn push(line: String) {
    let Ok(mut buffer) = BUFFER.lock() else { return };
    for line in line.lines() {
        if buffer.lines.len() == CAPACITY {
            buffer.lines.pop_front();
        }
        buffer.lines.push_back(line.to_string());
        buffer.total += 1;
    }
}

/// The lines written after the first `seen` ones, and the new count to pass next time.
/// Lines that already fell out of the buffer are skipped.
pub fn lines_since(seen: u64) -> (Vec<String>, u64) {
    let Ok(buffer) = BUFFER.lock() else { return (Vec::new(), seen) };
    let first_kept = buffer.total - buffer.lines.len() as u64;
    let skip = seen.saturating_sub(first_kept) as usize;
    (buffer.lines.iter().skip(skip).cloned().collect(), buffer.total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_out_only_new_lines() {
        let (_, start) = lines_since(0);
        push("first".into());
        push("second\nthird".into());
        let (lines, seen) = lines_since(start);
        assert_eq!(lines, ["first", "second", "third"]);
        assert_eq!(seen, start + 3);
        assert!(lines_since(seen).0.is_empty());
    }
}
