use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    sync::Arc,
};
use tracing_subscriber::{
    Registry, filter::EnvFilter, fmt, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

#[derive(Clone)]
pub struct LogWriter {
    inner: Arc<Mutex<LogSink>>,
}

enum LogSink {
    Stdout,
    File(File),
}

impl LogWriter {
    fn new(output: &str) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(open_sink(output)?)),
        })
    }

    fn reload(&self, output: &str) -> Result<()> {
        let sink = open_sink(output)?;
        *self.inner.lock() = sink;
        Ok(())
    }
}

fn open_sink(output: &str) -> Result<LogSink> {
    if output.trim().is_empty() {
        return Ok(LogSink::Stdout);
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(output)
        .with_context(|| format!("open log file {output}"))?;
    Ok(LogSink::File(file))
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut sink = self.inner.lock();
        match &mut *sink {
            LogSink::Stdout => io::stdout().write(buf),
            LogSink::File(file) => file.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut sink = self.inner.lock();
        match &mut *sink {
            LogSink::Stdout => io::stdout().flush(),
            LogSink::File(file) => file.flush(),
        }
    }
}

impl<'a> fmt::MakeWriter<'a> for LogWriter {
    type Writer = LogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

pub struct LoggingHandle {
    writer: LogWriter,
    filter: reload::Handle<EnvFilter, Registry>,
}

impl LoggingHandle {
    pub fn reload(&self, config: &crate::config::LogConfig) -> Result<()> {
        self.writer.reload(&config.output)?;
        let filter = env_filter(&config.level);
        self.filter
            .reload(filter)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(())
    }
}

fn env_filter(level: &str) -> EnvFilter {
    EnvFilter::try_new(level.trim()).unwrap_or_else(|_| EnvFilter::new("info"))
}

pub fn init(config: &crate::config::LogConfig) -> Result<LoggingHandle> {
    let writer = LogWriter::new(&config.output)?;
    let (filter_layer, filter) = reload::Layer::new(env_filter(&config.level));
    let fmt_layer = fmt::layer()
        .with_writer(writer.clone())
        .with_ansi(config.ansi)
        .with_target(true)
        .with_level(true);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .try_init()?;
    Ok(LoggingHandle { writer, filter })
}
