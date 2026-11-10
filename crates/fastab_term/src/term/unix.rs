use std::fs::OpenOptions;
use std::io::{Error as IoError, Write, stdin, stdout};
use std::mem;
use std::os::fd::BorrowedFd;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::BytesMut;
use filedescriptor::FileDescriptor;
use flume::{Receiver, Sender, bounded};
use nix::libc::{self, winsize};
use nix::sys::termios::{FlushArg, SetArg, Termios, cfmakeraw, tcdrain, tcflush, tcgetattr, tcsetattr};
use tokio::io::{self, AsyncRead, AsyncReadExt};
use tokio::select;
use tokio::signal::unix::SignalKind;
use tokio::time::Instant;

use super::InputEventResult;
use crate::input::{InputEvent, InputParser};
use crate::term::istty::IsTty;
use crate::term::{ScreenSize, Terminal, cast};

const BUF_SIZE: usize = 4096;

pub enum Purge {
    InputQueue,
    OutputQueue,
    InputAndOutputQueue,
}

pub enum SetAttributeWhen {
    /// changes are applied immediately
    Now,
    /// Apply once the current output queue has drained
    AfterDrainOutputQueue,
    /// Wait for the current output queue to drain, then
    /// discard any unread input
    AfterDrainOutputQueuePurgeInputQueue,
}

pub trait UnixTty {
    fn get_size(&mut self) -> Result<winsize>;
    fn set_size(&mut self, size: winsize) -> Result<()>;
    fn get_termios(&mut self) -> Result<Termios>;
    fn set_termios(&mut self, termios: &Termios, when: SetAttributeWhen) -> Result<()>;
    /// Waits until all written data has been transmitted.
    fn drain(&mut self) -> Result<()>;
    fn purge(&mut self, purge: Purge) -> Result<()>;
}

pub struct TtyWriteHandle {
    fd: FileDescriptor,
    write_buffer: Vec<u8>,
}

impl TtyWriteHandle {
    fn new(fd: FileDescriptor) -> Self {
        Self {
            fd,
            write_buffer: Vec::with_capacity(BUF_SIZE),
        }
    }

    fn flush_local_buffer(&mut self) -> std::result::Result<(), IoError> {
        if !self.write_buffer.is_empty() {
            self.fd.write_all(&self.write_buffer)?;
            self.write_buffer.clear();
        }
        Ok(())
    }
}

impl Write for TtyWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::result::Result<usize, IoError> {
        if self.write_buffer.len() + buf.len() > self.write_buffer.capacity() {
            self.flush()?;
        }
        if buf.len() >= self.write_buffer.capacity() {
            self.fd.write(buf)
        } else {
            self.write_buffer.write(buf)
        }
    }

    fn flush(&mut self) -> std::result::Result<(), IoError> {
        self.flush_local_buffer()?;
        self.drain().map_err(|e| IoError::other(format!("{e}")))?;
        Ok(())
    }
}

/// SAFETY: the [FileDescriptor] is already guaranteed to have an owned file descriptor by
/// its contract, so we just borrow it with a lifetime which make the borrow safe.
fn borrow_fd(fd: &FileDescriptor) -> BorrowedFd<'_> {
    unsafe { BorrowedFd::borrow_raw(fd.as_raw_fd()) }
}

impl UnixTty for TtyWriteHandle {
    fn get_size(&mut self) -> Result<winsize> {
        let mut size: winsize = unsafe { mem::zeroed() };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut size) } != 0 {
            bail!("failed to ioctl(TIOCGWINSZ): {}", IoError::last_os_error());
        }
        Ok(size)
    }

    fn set_size(&mut self, size: winsize) -> Result<()> {
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &size as *const _) } != 0 {
            bail!("failed to ioctl(TIOCSWINSZ): {:?}", IoError::last_os_error());
        }

        Ok(())
    }

    fn get_termios(&mut self) -> Result<Termios> {
        tcgetattr(borrow_fd(&self.fd)).context("get_termios failed")
    }

    fn set_termios(&mut self, termios: &Termios, when: SetAttributeWhen) -> Result<()> {
        let when = match when {
            SetAttributeWhen::Now => SetArg::TCSANOW,
            SetAttributeWhen::AfterDrainOutputQueue => SetArg::TCSADRAIN,
            SetAttributeWhen::AfterDrainOutputQueuePurgeInputQueue => SetArg::TCSAFLUSH,
        };
        tcsetattr(borrow_fd(&self.fd), when, termios).context("set_termios failed")
    }

    fn drain(&mut self) -> Result<()> {
        tcdrain(borrow_fd(&self.fd)).context("tcdrain failed")
    }

    fn purge(&mut self, purge: Purge) -> Result<()> {
        let param = match purge {
            Purge::InputQueue => FlushArg::TCIFLUSH,
            Purge::OutputQueue => FlushArg::TCOFLUSH,
            Purge::InputAndOutputQueue => FlushArg::TCIOFLUSH,
        };
        tcflush(borrow_fd(&self.fd), param).context("tcflush failed")
    }
}

/// A unix style terminal
pub struct UnixTerminal {
    write: TtyWriteHandle,
    saved_termios: Termios,
}

impl UnixTerminal {
    /// Attempt to create an instance from the stdin and stdout of the
    /// process.  This will fail unless both are associated with a tty.
    /// Note that this will duplicate the underlying file descriptors
    /// and will no longer participate in the stdin/stdout locking
    /// provided by the rust standard library.
    pub fn new_from_stdio() -> Result<UnixTerminal> {
        Self::new_with(&stdin(), &stdout())
    }

    pub fn new_with<A: AsRawFd, B: AsRawFd>(read: &A, write: &B) -> Result<UnixTerminal> {
        if !read.is_tty() || !write.is_tty() {
            anyhow::bail!("stdin and stdout must both be tty handles");
        }

        let mut write = TtyWriteHandle::new(FileDescriptor::dup(write)?);
        let saved_termios = write.get_termios()?;

        Ok(UnixTerminal { write, saved_termios })
    }

    /// Attempt to explicitly open a handle to the terminal device
    /// (/dev/tty) and build a `UnixTerminal` from there.  This will
    /// yield a terminal even if the stdio streams have been redirected,
    /// provided that the process has an associated controlling terminal.
    pub fn new() -> Result<UnixTerminal> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        Self::new_with(&file, &file)
    }
}

static IMMEDIATE_MODE: AtomicBool = AtomicBool::new(true);

impl Terminal for UnixTerminal {
    fn set_raw_mode(&mut self) -> Result<()> {
        let mut raw = self.write.get_termios()?;
        cfmakeraw(&mut raw);
        self.write
            .set_termios(&raw, SetAttributeWhen::AfterDrainOutputQueuePurgeInputQueue)
            .context("failed to set raw mode")?;
        self.write.flush()?;

        Ok(())
    }

    fn set_cooked_mode(&mut self) -> Result<()> {
        self.write.set_termios(&self.saved_termios, SetAttributeWhen::Now)
    }

    fn get_screen_size(&mut self) -> Result<ScreenSize> {
        let size = self.write.get_size()?;
        Ok(ScreenSize {
            rows: cast(size.ws_row)?,
            cols: cast(size.ws_col)?,
            xpixel: cast(size.ws_xpixel)?,
            ypixel: cast(size.ws_ypixel)?,
        })
    }

    fn set_screen_size(&mut self, size: ScreenSize) -> Result<()> {
        let size = winsize {
            ws_row: cast(size.rows)?,
            ws_col: cast(size.cols)?,
            ws_xpixel: cast(size.xpixel)?,
            ws_ypixel: cast(size.ypixel)?,
        };

        self.write.set_size(size)
    }

    fn flush(&mut self) -> Result<()> {
        self.write.flush().context("flush failed")
    }

    fn set_immediate_mode(&mut self, immediate: bool) -> Result<()> {
        IMMEDIATE_MODE.store(immediate, Ordering::SeqCst);
        Ok(())
    }

    fn read_input(&mut self) -> Result<Receiver<InputEventResult>> {
        let window_change_signal = tokio::signal::unix::signal(SignalKind::window_change())?;
        let (input_tx, input_rx) = bounded::<InputEventResult>(1);

        tokio::spawn(read_input_stream(
            io::stdin(),
            input_tx,
            Some(window_change_signal),
            &IMMEDIATE_MODE,
        ));

        Ok(input_rx)
    }
}

const MAX_INPUT_BATCH_BYTES: usize = 64 * 1024;

struct InputOuterResources;

impl Drop for InputOuterResources {
    fn drop(&mut self) {
        crate::resource_diagnostics::input_outer(0, 0);
    }
}

async fn flush_input_batch(
    parser: &mut InputParser,
    buf: &mut BytesMut,
    input_tx: &Sender<InputEventResult>,
    maybe_more: bool,
    finishing: bool,
) -> bool {
    flush_input_batch_emitting(parser, buf, input_tx, maybe_more, finishing)
        .await
        .is_some()
}

/// `None` when the receiver is gone, otherwise whether any event was emitted.
async fn flush_input_batch_emitting(
    parser: &mut InputParser,
    buf: &mut BytesMut,
    input_tx: &Sender<InputEventResult>,
    maybe_more: bool,
    finishing: bool,
) -> Option<bool> {
    let mut events = Vec::new();
    parser.parse(buf, |raw, event| events.push(Ok((raw, event))), maybe_more);
    buf.clear();
    if buf.capacity() > MAX_INPUT_BATCH_BYTES {
        *buf = BytesMut::with_capacity(crate::BUFFER_SIZE);
    }
    crate::resource_diagnostics::input_outer(buf.len(), buf.capacity());
    if finishing {
        parser.finish(|raw, event| events.push(Ok((raw, event))));
    }
    if events.is_empty() {
        return Some(false);
    }
    input_tx.send_async(events).await.ok().map(|()| true)
}

async fn read_input_stream(
    mut stdin: impl AsyncRead + Unpin,
    input_tx: Sender<InputEventResult>,
    mut window_change_signal: Option<tokio::signal::unix::Signal>,
    immediate_mode: &AtomicBool,
) {
    let _resources = InputOuterResources;
    let mut parser = InputParser::new_raw_passthrough();
    let mut buf = BytesMut::with_capacity(crate::BUFFER_SIZE);
    crate::resource_diagnostics::input_outer(buf.len(), buf.capacity());
    // A fixed request size also prevents Tokio stdin's internal blocking Buf
    // from following an arbitrarily large outer allocation.
    let mut chunk = [0u8; crate::BUFFER_SIZE];
    // Outer aggregation, timed from the batch's first byte.
    let mut batch_deadline = None;
    // A prefix the parser holds, timed from when it was first held, so one
    // that arrives late in a batch still gets the full window to complete.
    // While it is set the outer batch is empty: every read flushes at once.
    let mut held_deadline: Option<Instant> = None;
    loop {
        let read_limit = (MAX_INPUT_BATCH_BYTES - buf.len()).min(chunk.len());
        select! {
            biased;
            res = stdin.read(&mut chunk[..read_limit]) => {
                match res {
                    Ok(0) => {
                        flush_input_batch(&mut parser, &mut buf, &input_tx, false, true).await;
                        return;
                    }
                    Ok(n) => {
                        // Reads win the biased select, so steady input would starve the
                        // hold timer. Settle an expired prefix alone, before the new bytes
                        // can join it (ESC + `x` would read as Alt+x).
                        if held_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                            held_deadline = None;
                            if !flush_input_batch(&mut parser, &mut buf, &input_tx, false, false).await { return; }
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        crate::resource_diagnostics::input_outer(buf.len(), buf.capacity());
                        let deadline = *batch_deadline.get_or_insert_with(|| Instant::now() + Duration::from_millis(1));
                        if held_deadline.is_some()
                            || immediate_mode.load(Ordering::SeqCst)
                            || buf.len() >= MAX_INPUT_BATCH_BYTES
                            || Instant::now() >= deadline
                        {
                            // A read boundary does not settle a prefix that is still in its window.
                            let Some(emitted) = flush_input_batch_emitting(&mut parser, &mut buf, &input_tx, true, false).await else { return; };
                            batch_deadline = None;
                            // Emitting consumed the old prefix, so a held one is new.
                            let inherited = held_deadline.filter(|_| !emitted);
                            held_deadline = parser
                                .has_held_prefix()
                                .then(|| inherited.unwrap_or_else(|| Instant::now() + Duration::from_millis(1)));
                        }
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        // Deliver already-read bytes before the permanent error.
                        if flush_input_batch(&mut parser, &mut buf, &input_tx, false, true).await {
                            let _ = input_tx.send_async(vec![Err(anyhow::anyhow!(err))]).await;
                        }
                        return;
                    }
                }
            }
            _ = tokio::time::sleep_until(held_deadline.or(batch_deadline).unwrap_or_else(Instant::now)),
                if held_deadline.is_some() || batch_deadline.is_some() =>
            {
                let settle = held_deadline.take().is_some();
                if !flush_input_batch(&mut parser, &mut buf, &input_tx, !settle, false).await { return; }
                batch_deadline = None;
                if !settle && parser.has_held_prefix() {
                    held_deadline = Some(Instant::now() + Duration::from_millis(1));
                }
            }
            _ = async {
                match window_change_signal.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending().await,
                }
            }, if window_change_signal.is_some() => {
                if input_tx.send_async(vec![Ok((None, InputEvent::Resized))]).await.is_err() { return; }
            }
        }
    }
}

impl Drop for UnixTerminal {
    fn drop(&mut self) {
        // Best effort. The terminal can be gone by now (its window closed
        // while this process was not the one to get SIGHUP), and then
        // `tcdrain` and `tcsetattr` fail with EIO. There is nothing left to
        // restore in that case, and a panic here aborts the shipped build on
        // its way out, leaving a crash report for an ordinary exit.
        if let Err(err) = self.write.flush() {
            tracing::debug!(%err, "Could not flush the terminal before restoring it");
        }
        if let Err(err) = self.write.set_termios(&self.saved_termios, SetAttributeWhen::Now) {
            tracing::debug!(%err, "Could not restore the terminal's original mode");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll};

    use tokio::io::{AsyncWriteExt, ReadBuf};
    use tokio::net::UnixStream;

    use super::*;

    // A real socket supplies the finite bytes. Convert its EOF to an injected
    // permanent device error to cover the reader's distinct error exit path.
    struct ErrorAtEof(UnixStream);

    impl AsyncRead for ErrorAtEof {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut TaskContext<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let before = buf.filled().len();
            match Pin::new(&mut self.0).poll_read(cx, buf) {
                Poll::Ready(Ok(())) if buf.filled().len() == before => {
                    Poll::Ready(Err(io::Error::other("injected input device failure")))
                },
                result => result,
            }
        }
    }

    async fn finite_stream(input: &[u8], permanent_error: bool, immediate: bool) -> (Vec<u8>, usize) {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let (tx, rx) = bounded(1);
        let task = tokio::spawn(async move {
            let mode = AtomicBool::new(immediate);
            if permanent_error {
                read_input_stream(ErrorAtEof(reader), tx, None, &mode).await;
            } else {
                read_input_stream(reader, tx, None, &mode).await;
            }
        });
        writer.write_all(input).await.unwrap();
        writer.shutdown().await.unwrap();
        let mut raw = Vec::new();
        let mut errors = 0;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Ok(events) = rx.recv_async().await {
                for event in events {
                    match event {
                        Ok((bytes, event)) => {
                            assert!(
                                !matches!(event, InputEvent::Paste(_)),
                                "Unix raw paste must not decode unused text"
                            );
                            assert_eq!(errors, 0, "buffered input must precede the terminal error");
                            raw.extend_from_slice(bytes.as_deref().unwrap_or_default());
                        },
                        Err(_) => errors += 1,
                    }
                }
            }
            task.await.unwrap();
        })
        .await
        .expect("finite input must close its channel and task");
        (raw, errors)
    }

    #[tokio::test]
    async fn finite_input_flushes_partial_raw_before_eof_or_permanent_error() {
        for immediate in [false, true] {
            for permanent_error in [false, true] {
                for input in [b"".as_slice(), b"abc\xe2\x82", b"abc\x1b[", b"\x1b[200~unfinished\xff"] {
                    let (raw, errors) = finite_stream(input, permanent_error, immediate).await;
                    assert_eq!(raw, input);
                    assert_eq!(errors, usize::from(permanent_error));
                }
            }
        }
    }

    #[tokio::test]
    async fn read_boundary_flush_preserves_split_paste_markers_and_idle_flush_settles_escape() {
        let mut input = crate::input::PASTE_START.as_bytes().to_vec();
        input.extend(std::iter::repeat_n(b'x', 128 * 1024));
        input.extend_from_slice(b"\xe4\xb8\xad\xff\x1b[31m");
        input.extend_from_slice(crate::input::PASTE_END.as_bytes());
        let paste_len = input.len();
        input.extend_from_slice(b"xy");
        for split in 1..crate::input::PASTE_START.len() {
            let mut parser = InputParser::new_raw_passthrough();
            let mut buf = BytesMut::from(&input[..split]);
            let (tx, rx) = bounded(4);
            // A size/immediate read boundary must not commit this prefix.
            assert!(flush_input_batch(&mut parser, &mut buf, &tx, true, false).await);
            assert!(rx.try_recv().is_err());
            buf.extend_from_slice(&input[split..paste_len - 3]);
            assert!(flush_input_batch(&mut parser, &mut buf, &tx, true, false).await);
            assert!(rx.try_recv().is_err());
            buf.extend_from_slice(&input[paste_len - 3..]);
            assert!(flush_input_batch(&mut parser, &mut buf, &tx, true, false).await);
            let events: Vec<_> = rx.recv().unwrap().into_iter().map(|event| event.unwrap()).collect();
            assert_eq!(events.len(), 3);
            assert_eq!(events[0].1, InputEvent::RawString);
            let raw = events[0].0.as_ref().unwrap();
            assert_eq!(raw.as_ref(), &input[..paste_len]);
            assert!(raw.is_unique());
            for (index, byte) in b"xy".iter().enumerate() {
                assert_eq!(events[index + 1].0.as_deref(), Some(std::slice::from_ref(byte)));
                assert_eq!(
                    events[index + 1].1,
                    InputEvent::Key(crate::input::KeyEvent {
                        key: crate::input::KeyCode::Char(char::from(*byte)),
                        modifiers: crate::input::Modifiers::NONE,
                    })
                );
            }
            assert!(buf.is_empty());
            assert!(buf.capacity() <= MAX_INPUT_BATCH_BYTES);
        }

        let mut parser = InputParser::new_raw_passthrough();
        let mut buf = BytesMut::from(b"\x1b".as_slice());
        let (tx, rx) = bounded(1);
        assert!(flush_input_batch(&mut parser, &mut buf, &tx, true, false).await);
        assert!(rx.try_recv().is_err());
        // The original first-byte timer must flush even with an empty batch.
        assert!(buf.is_empty());
        assert!(flush_input_batch(&mut parser, &mut buf, &tx, false, false).await);
        let event = rx.recv().unwrap().pop().unwrap().unwrap();
        assert_eq!(event.0.as_deref(), Some(b"\x1b".as_slice()));
        assert!(matches!(
            event.1,
            InputEvent::Key(crate::input::KeyEvent {
                key: crate::input::KeyCode::Escape,
                modifiers: crate::input::Modifiers::NONE,
            })
        ));
    }

    #[tokio::test]
    async fn immediate_input_settles_isolated_escape_without_eof_or_another_read() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let (tx, rx) = bounded(1);
        let task = tokio::spawn(async move {
            read_input_stream(reader, tx, None, &AtomicBool::new(true)).await;
        });
        writer.write_all(b"\x1b").await.unwrap();
        let events = tokio::time::timeout(Duration::from_secs(2), rx.recv_async())
            .await
            .expect("the empty-batch timer must settle ESC while the writer stays open")
            .unwrap();
        assert_eq!(events.len(), 1);
        let event = events.into_iter().next().unwrap().unwrap();
        assert_eq!(event.0.as_deref(), Some(b"\x1b".as_slice()));
        assert!(matches!(
            event.1,
            InputEvent::Key(crate::input::KeyEvent {
                key: crate::input::KeyCode::Escape,
                modifiers: crate::input::Modifiers::NONE,
            })
        ));
        writer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(rx.recv_async().await.is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn escape_late_in_a_batch_still_gets_its_own_window() {
        // Step the paused clock explicitly: the timer has millisecond
        // granularity, so sleeping would land writes on timer ticks.
        async fn settle() {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        for immediate in [false, true] {
            let (mut writer, reader) = tokio::io::duplex(64);
            let (tx, rx) = bounded(8);
            let task = tokio::spawn(async move {
                read_input_stream(reader, tx, None, &AtomicBool::new(immediate)).await;
            });
            writer.write_all(b"a").await.unwrap();
            settle().await;
            tokio::time::advance(Duration::from_micros(500)).await;
            writer.write_all(b"\x1b").await.unwrap();
            settle().await;
            // Past the first byte's 1 ms, inside the escape's own.
            tokio::time::advance(Duration::from_micros(600)).await;
            settle().await;
            writer.write_all(b"[A").await.unwrap();
            settle().await;
            drop(writer);
            task.await.unwrap();
            let mut events = Vec::new();
            while let Ok(batch) = rx.recv_async().await {
                events.extend(batch.into_iter().map(|event| event.unwrap()));
            }
            assert_eq!(events.len(), 2, "immediate={immediate}: {events:?}");
            assert_eq!(events[0].0.as_deref(), Some(b"a".as_slice()));
            assert_eq!(events[1].0.as_deref(), Some(b"\x1b[A".as_slice()));
            assert!(matches!(
                events[1].1,
                InputEvent::Key(crate::input::KeyEvent {
                    key: crate::input::KeyCode::UpArrow,
                    ..
                })
            ));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn steady_input_cannot_keep_an_expired_escape_held() {
        async fn settle() {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
        for immediate in [false, true] {
            let (mut writer, reader) = tokio::io::duplex(64);
            let (tx, rx) = bounded(8);
            let task = tokio::spawn(async move {
                read_input_stream(reader, tx, None, &AtomicBool::new(immediate)).await;
            });
            writer.write_all(b"\x1b").await.unwrap();
            settle().await;
            if !immediate {
                // The batch timer hands ESC to the parser, which holds it.
                tokio::time::advance(Duration::from_micros(1100)).await;
                settle().await;
            }
            // The next read is ready by the time the hold expires, and reads
            // win the biased select over the hold timer.
            writer.write_all(b"x").await.unwrap();
            tokio::time::advance(Duration::from_micros(1500)).await;
            settle().await;
            drop(writer);
            task.await.unwrap();
            let mut events = Vec::new();
            while let Ok(batch) = rx.recv_async().await {
                events.extend(batch.into_iter().map(|event| event.unwrap()));
            }
            assert_eq!(
                events,
                vec![
                    (
                        Some(bytes::Bytes::from_static(b"\x1b")),
                        InputEvent::Key(crate::input::KeyEvent {
                            key: crate::input::KeyCode::Escape,
                            modifiers: crate::input::Modifiers::NONE,
                        })
                    ),
                    (
                        Some(bytes::Bytes::from_static(b"x")),
                        InputEvent::Key(crate::input::KeyEvent {
                            key: crate::input::KeyCode::Char('x'),
                            modifiers: crate::input::Modifiers::NONE,
                        })
                    ),
                ],
                "immediate={immediate}"
            );
        }
    }

    #[tokio::test]
    async fn continuous_input_produces_bounded_batches_before_writer_exits() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        let producer = tokio::spawn(async move {
            loop {
                if writer.write_all(&[b'x'; crate::BUFFER_SIZE]).await.is_err() {
                    break;
                }
            }
        });
        let (tx, rx) = bounded(1);
        let reader_task = tokio::spawn(async move {
            read_input_stream(reader, tx, None, &AtomicBool::new(false)).await;
        });
        let events = tokio::time::timeout(Duration::from_secs(2), rx.recv_async())
            .await
            .unwrap()
            .unwrap();
        let bytes: usize = events.into_iter().map(|event| event.unwrap().0.unwrap().len()).sum();
        assert!(bytes > 0 && bytes <= MAX_INPUT_BATCH_BYTES);
        assert!(
            !producer.is_finished(),
            "a continuously ready reader must still flush batches"
        );
        producer.abort();
        let _ = producer.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while rx.recv_async().await.is_ok() {}
            reader_task.await.unwrap();
        })
        .await
        .expect("closing the producer must terminate the reader");
    }

    #[tokio::test]
    async fn flushed_large_batch_releases_outer_backing_while_raw_stays_alive() {
        let mut buf = BytesMut::from(b"\x1b[200~".as_slice());
        buf.extend(std::iter::repeat_n(b'x', 128 * 1024));
        buf.extend_from_slice(b"\x1b[201~");
        let expected = buf.to_vec();
        let (tx, rx) = bounded(1);
        let mut parser = InputParser::new();
        assert!(flush_input_batch(&mut parser, &mut buf, &tx, false, false).await);
        let raw = rx.recv().unwrap().pop().unwrap().unwrap().0.unwrap();
        assert_eq!(raw.as_ref(), expected);
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), crate::BUFFER_SIZE);
    }
}
