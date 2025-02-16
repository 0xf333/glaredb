use crate::highlighter::SQLHighlighter;
use crate::prompt::SQLPrompt;
use crate::util::MetastoreClientMode;
use anyhow::{anyhow, Result};
use clap::{Parser, ValueEnum};
use colored::Colorize;
use datafusion::arrow::csv::writer::WriterBuilder as CsvWriterBuilder;
use datafusion::arrow::error::ArrowError;
use datafusion::arrow::json::writer::{
    JsonFormat, LineDelimited as JsonLineDelimted, Writer as JsonWriter,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::display::FormatOptions;
use datafusion::arrow::util::pretty;
use datafusion::physical_plan::SendableRecordBatchStream;
use futures::StreamExt;
use once_cell::sync::Lazy;
use pgrepr::format::Format;
use reedline::{FileBackedHistory, Reedline, Signal};

use sqlexec::engine::EngineStorageConfig;
use sqlexec::engine::{Engine, SessionStorageConfig, TrackedSession};
use sqlexec::parser;
use sqlexec::session::ExecutionResult;
use sqlexec::vars::SessionVars;
use std::env;
use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::Tracker;
use tracing::error;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum OutputMode {
    Table,
    Json,
    Ndjson,
    Csv,
}

#[derive(Debug, Clone, Parser)]
pub struct LocalClientOpts {
    /// Address to the Metastore.
    ///
    /// If not provided, an in-process metastore will be started.
    #[clap(short, long, value_parser)]
    pub metastore_addr: Option<String>,

    /// Path to spill temporary files to.
    #[clap(long, value_parser)]
    pub spill_path: Option<PathBuf>,

    /// Optional file path for persisting data.
    ///
    /// Catalog data and user data will be stored in this directory.
    #[clap(short = 'f', long, value_parser)]
    pub data_dir: Option<PathBuf>,

    /// Display output mode.
    #[arg(long, value_enum, default_value_t=OutputMode::Table)]
    pub mode: OutputMode,
}

impl LocalClientOpts {
    fn help_string() -> Result<String> {
        let pairs = [
            ("\\help", "Show this help text"),
            (
                "\\mode MODE",
                "Set the output mode [table, json, ndjson, csv]",
            ),
            ("\\open PATH", "Open a database at the given path"),
            ("\\quit", "Quit this session"),
        ];

        let mut buf = String::new();
        for (cmd, help) in pairs {
            writeln!(&mut buf, "{cmd: <15} {help}")?;
        }

        Ok(buf)
    }
}

pub struct LocalSession {
    sess: TrackedSession,
    engine: Engine,
    opts: LocalClientOpts,
}

impl LocalSession {
    pub async fn connect(opts: LocalClientOpts) -> Result<Self> {
        if opts.metastore_addr.is_some() && opts.data_dir.is_some() {
            return Err(anyhow!(
                "Cannot specify both a metastore address and a local file path"
            ));
        }

        // Connect to metastore.
        let mode = MetastoreClientMode::new_from_options(
            opts.metastore_addr.clone(),
            opts.data_dir.clone(),
        )?;
        let metastore_client = mode.into_client().await?;
        let tracker = Arc::new(Tracker::Nop);

        let storage_conf = match &opts.data_dir {
            Some(path) => EngineStorageConfig::Local { path: path.clone() },
            None => EngineStorageConfig::Memory,
        };

        let engine = Engine::new(
            metastore_client,
            storage_conf,
            tracker,
            opts.spill_path.clone(),
        )
        .await?;

        Ok(LocalSession {
            sess: engine
                .new_session(SessionVars::default(), SessionStorageConfig::default())
                .await?,
            engine,
            opts,
        })
    }

    pub async fn run(mut self, query: Option<String>) -> Result<()> {
        let result = if let Some(query) = query {
            self.execute_one(&query).await
        } else {
            self.run_interactive().await
        };

        // Try to shutdown the engine gracefully.
        if let Err(err) = self.engine.shutdown().await {
            error!(%err, "unable to shutdown the engine gracefully");
        }

        result
    }

    async fn run_interactive(&mut self) -> Result<()> {
        let history = Box::new(
            FileBackedHistory::with_file(100, get_history_path())
                .expect("Error configuring history with file"),
        );

        let mut line_editor = Reedline::create().with_history(history);

        let sql_highlighter = SQLHighlighter {};
        line_editor = line_editor.with_highlighter(Box::new(sql_highlighter));

        println!("GlareDB (v{})", env!("CARGO_PKG_VERSION"));
        println!("Type \\help for help.");

        let info = match (&self.opts.metastore_addr, &self.opts.data_dir) {
            (Some(addr), None) => format!("Persisting catalog on remote metastore: {addr}"), // TODO: Should we continue to allow this?
            (None, Some(path)) => format!("Persisting database at path: {}", path.display()),
            (None, None) => "Using in-memory catalog".to_string(),
            _ => unreachable!(),
        };
        println!("{}", info.bold());
        let prompt = SQLPrompt {};
        let mut scratch = String::with_capacity(1024);

        loop {
            let sig = line_editor.read_line(&prompt);
            match sig {
                Ok(Signal::Success(buffer)) => match buffer.as_str() {
                    cmd if is_client_cmd(cmd) => {
                        self.handle_client_cmd(cmd).await?;
                    }
                    _ => {
                        let mut parts = buffer.splitn(2, ';');
                        let first = parts.next().unwrap();
                        scratch.push_str(first);

                        let second = parts.next();
                        if second.is_some() {
                            match self.execute(&scratch).await {
                                Ok(_) => {}
                                Err(e) => println!("Error: {e}"),
                            };
                            scratch.clear();
                        } else {
                            scratch.push(' ');
                        }
                    }
                },
                Ok(Signal::CtrlD) => break,
                Ok(Signal::CtrlC) => {
                    if scratch.is_empty() {
                        break;
                    } else {
                        scratch.clear();
                    }
                }
                Err(e) => {
                    return Err(anyhow!("Unable to read from prompt: {e}"));
                }
            }
        }
        Ok(())
    }

    async fn execute_one(&mut self, query: &str) -> Result<()> {
        self.execute(query).await?;
        Ok(())
    }

    async fn execute(&mut self, text: &str) -> Result<()> {
        if is_client_cmd(text) {
            self.handle_client_cmd(text).await?;
            return Ok(());
        }

        const UNNAMED: String = String::new();

        let statements = parser::parse_sql(text)?;
        for stmt in statements {
            self.sess
                .prepare_statement(UNNAMED, Some(stmt), Vec::new())
                .await?;
            let prepared = self.sess.get_prepared_statement(&UNNAMED)?;
            let num_fields = prepared.output_fields().map(|f| f.len()).unwrap_or(0);
            self.sess.bind_statement(
                UNNAMED,
                &UNNAMED,
                Vec::new(),
                vec![Format::Text; num_fields],
            )?;
            let result = self.sess.execute_portal(&UNNAMED, 0).await?;

            match result {
                ExecutionResult::Query { stream, .. }
                | ExecutionResult::ShowVariable { stream } => {
                    print_stream(stream, self.opts.mode).await?
                }
                other => println!("{:?}", other),
            }
        }
        Ok(())
    }

    async fn handle_client_cmd(&mut self, text: &str) -> Result<()> {
        let mut ss = text.split_whitespace();
        let cmd = ss.next().unwrap();
        let val = ss.next();

        match (cmd, val) {
            ("\\help", None) => {
                print!("{}", LocalClientOpts::help_string()?);
            }
            ("\\mode", Some(val)) => {
                self.opts.mode = OutputMode::from_str(val, true)
                    .map_err(|s| anyhow!("Unable to set output mode: {s}"))?;
            }
            ("\\open", Some(path)) => {
                let new_opts = LocalClientOpts {
                    data_dir: Some(PathBuf::from(path)),
                    metastore_addr: None,
                    ..self.opts.clone()
                };
                let new_sess = LocalSession::connect(new_opts).await?;
                println!("Created new session. New database path: {path}");
                *self = new_sess;
            }
            ("\\quit", None) => std::process::exit(0),
            (cmd, _) => return Err(anyhow!("Unable to handle client command: {cmd}")),
        }

        Ok(())
    }
}

static TABLE_FORMAT_OPTS: Lazy<FormatOptions> = Lazy::new(|| {
    FormatOptions::default()
        .with_display_error(false)
        .with_null("NULL")
});

async fn process_stream(stream: SendableRecordBatchStream) -> Result<Vec<RecordBatch>> {
    let batches = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;
    Ok(batches)
}

async fn print_stream(stream: SendableRecordBatchStream, mode: OutputMode) -> Result<()> {
    let batches = process_stream(stream).await?;

    fn write_json<F: JsonFormat>(batches: &[RecordBatch]) -> Result<()> {
        let stdout = std::io::stdout();
        let buf = std::io::BufWriter::new(stdout);
        let mut writer = JsonWriter::<_, F>::new(buf);
        for batch in batches {
            writer.write(batch)?;
        }
        writer.finish()?;
        let mut buf = writer.into_inner();
        buf.flush()?;
        Ok(())
    }

    match mode {
        OutputMode::Table => {
            let disp = pretty::pretty_format_batches_with_options(&batches, &TABLE_FORMAT_OPTS)?;
            println!("{disp}");
        }
        OutputMode::Csv => {
            let stdout = std::io::stdout();
            let buf = std::io::BufWriter::new(stdout);
            let mut writer = CsvWriterBuilder::new().has_headers(true).build(buf);
            for batch in batches {
                writer.write(&batch)?; // CSV writer flushes per write.
            }
        }
        OutputMode::Json => write_json::<JsonArrayNewLines>(&batches)?,
        OutputMode::Ndjson => write_json::<JsonLineDelimted>(&batches)?,
    }

    Ok(())
}

fn is_client_cmd(s: &str) -> bool {
    s.starts_with('\\')
}

/// Produces JSON output as a single JSON array with new lines between objects.
///
/// ```json
/// [{"foo":1},
/// {"bar":1}]
/// ```
#[derive(Debug, Default)]
pub struct JsonArrayNewLines {}

impl JsonFormat for JsonArrayNewLines {
    fn start_stream<W: Write>(&self, writer: &mut W) -> Result<(), ArrowError> {
        writer.write_all(b"[")?;
        Ok(())
    }

    fn start_row<W: Write>(&self, writer: &mut W, is_first_row: bool) -> Result<(), ArrowError> {
        if !is_first_row {
            writer.write_all(b",\n")?;
        }
        Ok(())
    }

    fn end_stream<W: Write>(&self, writer: &mut W) -> Result<(), ArrowError> {
        writer.write_all(b"]\n")?;
        Ok(())
    }
}

fn get_home_dir() -> PathBuf {
    match env::var("HOME") {
        Ok(path) => PathBuf::from(path),
        Err(_) => match env::var("USERPROFILE") {
            Ok(path) => PathBuf::from(path),
            Err(_) => panic!("Failed to get home directory"),
        },
    }
}

fn get_history_path() -> PathBuf {
    let mut home_dir = get_home_dir();
    home_dir.push(".glaredb");
    home_dir.push("history.txt");
    home_dir
}
