//! The log's writer: bounded, lossy, never blocking the caller.
//!
//! Log lines are written by the runtime's worker threads, four on a Raspberry
//! Pi. Writing them to stdout directly blocks a worker whenever the reader of
//! that pipe falls behind; openIVR records an unread 64 KiB pipe wedging go2rtc
//! within half an hour. Here each line goes into a queue of [`QUEUE_LINES`],
//! which one thread of its own writes out. A full queue drops the line and
//! counts it, and the next line written says how many were lost.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;

use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

/// Lines held while the reader is slow: about 1 MiB of typical lines.
pub const QUEUE_LINES: usize = 4096;

enum Message {
    Line(Vec<u8>),
    Stop,
}

/// What the formatter writes into; cheap to clone.
#[derive(Clone)]
pub struct LogWriter {
    queue: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

/// Writes out what is queued, then stops the thread, when dropped.
pub struct LogFlush {
    queue: SyncSender<Message>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for LogFlush {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LogFlush")
    }
}

/// A writer to `sink` through a queue of `capacity` lines, and its flush.
pub fn spawn(sink: Box<dyn Write + Send>, capacity: usize) -> std::io::Result<(LogWriter, LogFlush)> {
    let (queue, lines) = sync_channel(capacity.max(1));
    let dropped = Arc::new(AtomicU64::new(0));
    let thread = std::thread::Builder::new().name("oxrtc-log".into()).spawn({
        let dropped = dropped.clone();
        move || write_out(sink, &lines, &dropped)
    })?;
    Ok((LogWriter { queue: queue.clone(), dropped }, LogFlush { queue, thread: Some(thread) }))
}

fn write_out(mut sink: Box<dyn Write + Send>, lines: &Receiver<Message>, dropped: &AtomicU64) {
    while let Ok(Message::Line(line)) = lines.recv() {
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost > 0 {
            let time = crate::clock::now();
            let notice = crate::logging::line(
                &time,
                Level::WARN,
                "oxrtc",
                &format!("{lost} log lines dropped: their reader did not keep up"),
                &[],
            );
            // Nowhere to report a failure to write the log.
            let _ = writeln!(sink, "{notice}");
        }
        let _ = sink.write_all(&line);
        let _ = sink.flush();
    }
}

impl LogWriter {
    /// Lines lost since the last line written out.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl Write for LogWriter {
    /// The formatter writes one whole line per call; it is queued, or
    /// dropped and counted if the queue is full. Never blocks, never fails.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.queue.try_send(Message::Line(buf.to_vec())) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Drop for LogFlush {
    fn drop(&mut self) {
        // Blocking here is fine: the process is ending, and what is queued
        // (the last error before exit 1, say) must reach the reader.
        let _ = self.queue.send(Message::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier, Mutex};

    use super::*;

    /// A sink that holds its first write until the test lets it go.
    /// A sink whose first write says it has begun (`entered`), then waits
    /// for `gate`: the writer thread is then known to be holding a line.
    struct Stuck {
        entered: Arc<Barrier>,
        gate: Arc<Barrier>,
        opened: bool,
        out: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Stuck {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if !self.opened {
                self.entered.wait();
                self.gate.wait();
                self.opened = true;
            }
            self.out.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_slow_reader_loses_lines_but_never_blocks_the_caller() {
        let entered = Arc::new(Barrier::new(2));
        let gate = Arc::new(Barrier::new(2));
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = Stuck { entered: entered.clone(), gate: gate.clone(), opened: false, out: out.clone() };
        let (writer, flush) = spawn(Box::new(sink), 4).unwrap();
        writer.clone().write_all(b"line 0\n").unwrap();
        // The writer thread now holds line 0, stuck in the sink: the queue's
        // four places are all there is. Waiting for that, not guessing it,
        // keeps the count exact (a thread that started late took the drops'
        // count into its notice before the assertion read it, and the test
        // then hung on the full queue).
        entered.wait();
        let started = std::time::Instant::now();
        for n in 1..20 {
            let mut w = writer.clone();
            w.write_all(format!("line {n}\n").as_bytes()).unwrap();
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "the caller waited on the reader");
        assert_eq!(writer.dropped(), 15, "19 lines, 4 places");
        gate.wait();
        // Until the thread makes room, a line is dropped like any other.
        let mut w = writer.clone();
        loop {
            let before = writer.dropped();
            w.write_all(b"after\n").unwrap();
            if writer.dropped() == before {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        drop(flush);
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        // The notice goes out with the next line written, wherever the gap was.
        assert!(text.contains("line 0\n"), "{text}");
        assert!(text.contains("log lines dropped: their reader did not keep up"), "{text}");
        assert!(text.ends_with("after\n"), "the flush writes what was queued: {text}");
    }

    #[test]
    fn the_flush_writes_every_queued_line() {
        let out = Arc::new(Mutex::new(Vec::new()));
        let open = Arc::new(Barrier::new(1));
        let sink = Stuck { entered: open.clone(), gate: open, opened: false, out: out.clone() };
        let (writer, flush) = spawn(Box::new(sink), 64).unwrap();
        for n in 0..50 {
            let mut w = writer.clone();
            w.write_all(format!("{n}\n").as_bytes()).unwrap();
        }
        drop(flush);
        assert_eq!(String::from_utf8(out.lock().unwrap().clone()).unwrap().lines().count(), 50);
    }
}
