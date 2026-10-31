//! Log priorities for journald.
//!
//! journald files each line a service writes at the unit's default priority,
//! info, unless the line opens with a syslog `<N>` prefix. Without one every
//! WARN and ERROR the daemon logs is stored as info, and the
//! `journalctl -p warning` that the troubleshooting guide asks for shows
//! nothing (`docs/BUGS.md` B258).

use std::fmt;
use std::os::unix::fs::MetadataExt;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// Whether standard output is the journal stream systemd connected this
/// process to.
///
/// `JOURNAL_STREAM` alone does not say so: a terminal started from a user unit
/// inherits it, and every `pdfs` run in that terminal would print the
/// prefixes. systemd sets it to the stream's `device:inode`, so it is compared
/// against what fd 1 actually is.
pub(crate) fn stdout_is_journal() -> bool {
    let Some(stream) = std::env::var_os("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(stdout) = std::fs::metadata("/proc/self/fd/1") else {
        return false;
    };
    stream
        .to_str()
        .is_some_and(|value| names_stream(value, stdout.dev(), stdout.ino()))
}

/// Whether a `JOURNAL_STREAM` value names the file `dev`:`ino`.
fn names_stream(value: &str, dev: u64, ino: u64) -> bool {
    value
        .split_once(':')
        .is_some_and(|(d, i)| d.parse() == Ok(dev) && i.parse() == Ok(ino))
}

/// The syslog priority journald should file a `level` line at.
fn priority(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 3,
        Level::WARN => 4,
        Level::INFO => 6,
        Level::DEBUG | Level::TRACE => 7,
    }
}

/// An event format that opens each line with its `<N>` priority, then writes
/// the line as `F` would.
pub(crate) struct Prioritized<F>(pub(crate) F);

impl<S, N, F> FormatEvent<S, N> for Prioritized<F>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        write!(writer, "<{}>", priority(event.metadata().level()))?;
        self.0.format_event(ctx, writer, event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buf {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn each_line_opens_with_the_priority_of_its_level() {
        let buf = Buf(Arc::new(Mutex::new(Vec::new())));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .event_format(Prioritized(tracing_subscriber::fmt::format()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!("e");
            tracing::warn!("w");
            tracing::info!("i");
            tracing::debug!("d");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let prefixes: Vec<&str> = out.lines().map(|line| &line[..3]).collect();
        assert_eq!(prefixes, ["<3>", "<4>", "<6>", "<7>"]);
    }

    #[test]
    fn journal_stream_must_name_the_same_file() {
        assert!(names_stream("8:12345", 8, 12345));
        assert!(!names_stream("8:12345", 8, 54321));
        assert!(!names_stream("9:12345", 8, 12345));
        assert!(!names_stream("8", 8, 12345));
        assert!(!names_stream("", 8, 12345));
    }
}
