#![allow(dead_code)]

use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
pub struct LogBuffer {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        let bytes = self.buf.lock().unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

pub struct BufferWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for BufferWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter {
            buf: Arc::clone(&self.buf),
        }
    }
}

/// Installs a thread-local `tracing` subscriber capturing `WARN` and above into a [`LogBuffer`].
pub fn capture_warns() -> (LogBuffer, DefaultGuard) {
    let buffer = LogBuffer::new();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(buffer.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buffer, guard)
}
