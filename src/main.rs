use std::{
    io::{self, Write},
    path::PathBuf,
    process::{exit, Child, Command, Stdio},
    sync::{
        mpsc::{sync_channel, Receiver, RecvTimeoutError},
        Arc, Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use clap::{Parser, ValueEnum};
use fake_tty::make_script_command;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use itertools::Itertools;
use log::{debug, info, trace, warn};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use strum::{Display, EnumIter, EnumString, IntoEnumIterator};

fn main() {
    if let Err(e) = run() {
        eprintln!("failed to run cli: {e}");
        exit(1)
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    args.init_log()?;

    Cli::new(&args).and_then(|cli| cli.start())
}

/// A simple project to monitor file events and run commands
#[derive(Parser, Debug)]
#[command(author, version)]
struct Args {
    /// log level. default off
    #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 0)]
    verbose: u8,

    /// recursive for paths
    #[arg(short, long, default_value_t = false)]
    recurive: bool,

    /// Timeout for rechecking monitoring events
    #[arg(short = 'i', long, default_value = "30s")]
    poll_interval: humantime::Duration,

    /// Run a command in sh, pretending to be a tty. useful for example: docker exec
    #[arg(short, long, default_value_t = false)]
    tty: bool,

    /// run a command and send event to the stdin
    #[arg(short, long)]
    command: Option<String>,

    /// Listen for specific events
    #[arg(short, long, default_values_t = EventType::iter().collect::<Vec<_>>(), value_delimiter = ',')]
    events: Vec<EventType>,

    /// Exclude all events on files matching the globs <pattern>.
    /// higher priority than include
    #[arg(short = 'E', long)]
    excludes: Option<Vec<String>>,

    /// include all events on files matching the globs <pattern>.
    #[arg(short = 'I', long)]
    includes: Option<Vec<String>>,

    /// the monitoring paths
    #[clap(default_value = ".")]
    paths: Vec<PathBuf>,
}

impl Args {
    fn init_log(&self) -> Result<()> {
        let verbose = self.verbose;
        if verbose > 4 {
            return Err(anyhow!("invalid arg: 4 < {} number of verbose", verbose));
        }
        let level: log::LevelFilter = unsafe { std::mem::transmute((verbose + 1) as usize) };
        env_logger::builder()
            .filter_level(log::LevelFilter::Error)
            .filter_module(module_path!(), level)
            .init();
        Ok(())
    }

    fn build_glob(&self, s: &str) -> Result<Glob> {
        GlobBuilder::new(s)
            .case_insensitive(false)
            // .literal_separator(true)
            .build()
            .map_err(Into::into)
    }
}

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, EnumString, Display, EnumIter,
)]
// clap [clap::Command] attr: rename_all is default kebab-case
#[strum(serialize_all = "kebab-case")]
enum EventType {
    Access,
    Modify,
    // Attrib,
    // CloseWrite,
    // CloseNowrite,
    // Close,
    // Open,
    // MovedTo,
    // MovedFrom,
    Move,
    Create,
    Delete,
    // DeleteSelf,
    // Unmount,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EventInfo {
    event: EventType,
    paths: Vec<PathBuf>,
}

struct Cli<'a> {
    args: &'a Args,
    watcher: Arc<Mutex<RecommendedWatcher>>,
    rx: Receiver<EventInfo>,
    exclude_globs: Option<GlobSet>,
    include_globs: Option<GlobSet>,
}

impl<'a> Cli<'a> {
    pub fn new(args: &'a Args) -> Result<Self> {
        let exclude_globs = args
            .excludes
            .as_deref()
            .map(|globs| {
                globs
                    .iter()
                    .map(|s| args.build_glob(s))
                    .fold_ok(GlobSetBuilder::new(), |mut b, g| {
                        b.add(g);
                        b
                    })
                    .and_then(|b| b.build().map_err(Into::into))
                    .map(Some)
            })
            .unwrap_or(Ok(None))?;

        let include_globs = args
            .includes
            .as_deref()
            .map(|globs| {
                globs
                    .iter()
                    .map(|s| args.build_glob(s))
                    .fold_ok(GlobSetBuilder::new(), |mut b, g| {
                        b.add(g);
                        b
                    })
                    .and_then(|b| b.build().map_err(Into::into))
                    .map(Some)
            })
            .unwrap_or(Ok(None))?;

        let channel_size = 1000;
        let (tx, rx) = sync_channel(channel_size);
        let watcher = notify::recommended_watcher(
            move |res: Result<Event, notify::Error>| match res {
                Ok(e) => {
                    trace!(
                        "found {:?} event in {:?} has attrs {:?}",
                        e.kind,
                        e.paths,
                        e.attrs
                    );

                    use notify::EventKind;
                    let event_type = match e.kind {
                        EventKind::Access(_) => EventType::Access,
                        EventKind::Create(_) => EventType::Create,
                        EventKind::Modify(_) => EventType::Modify,
                        EventKind::Remove(_) => EventType::Delete,
                        _ => {
                            info!("Ignore unsupported event {:?} in {:?}", e.kind, e.paths);
                            return;
                        }
                    };
                    let info = EventInfo {
                        event: event_type,
                        paths: e.paths,
                    };

                    use std::sync::mpsc::{SendError, TrySendError};
                    tx.try_send(info).or_else(|e| match e {
                        TrySendError::Full(info) => {
                            // 在事件b到达时被通道阻塞，正在重试并等待通道可用
                            warn!("Blocked by channel on event {info:?} arrival, retrying and waiting for channel availability");
                            tx.send(info)
                        },
                        TrySendError::Disconnected(info) => Err(SendError(info))
                    }).unwrap();
                }
                Err(e) => warn!("ignore an error found during monitoring: {e}"),
            },
        )?;

        Ok(Self {
            args,
            rx,
            watcher: Arc::new(Mutex::new(watcher)),
            exclude_globs,
            include_globs,
        })
    }

    fn format(&self, e: &EventInfo, writer: &mut impl Write) -> Result<()> {
        // TODO: format
        let path_str = e.paths.iter().filter_map(|p| p.to_str()).join(",");
        writeln!(writer, "{} {}", e.event, path_str)?;
        Ok(())
    }

    fn is_interested(&self, info: &EventInfo) -> bool {
        self.args.events.iter().any(|e| *e == info.event)
            && self
                .exclude_globs
                .as_ref()
                // exclude for all matched
                .map(|gs| info.paths.iter().all(|path| !gs.is_match(path)))
                .unwrap_or(true)
            && self
                .include_globs
                .as_ref()
                .map(|gs| info.paths.iter().any(|path| gs.is_match(path)))
                .unwrap_or(true)
    }

    fn start(&self) -> Result<()> {
        // start watch paths
        let mut watcher = self.watcher.lock().map_err(|e| anyhow!("{e}"))?;
        let rec_mod = if self.args.recurive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let paths = &self.args.paths;
        for path in paths {
            watcher.watch(path, rec_mod)?;
        }

        let timeout: Duration = self.args.poll_interval.into();
        println!(
            "waiting {:?} events in interval {}s for {}{} paths: {:?}",
            self.args.events,
            timeout.as_secs(),
            if self.args.recurive { "recurive " } else { "" },
            paths.len(),
            paths
        );

        let mut cur_child = None::<Child>;
        let mut last_info = None::<EventInfo>;
        loop {
            trace!(
                "waiting timeout {}s for last info: {last_info:?}",
                timeout.as_secs()
            );
            match self.rx.recv_timeout(timeout) {
                Ok(info) => {
                    if !self.is_interested(&info) {
                        trace!("skipped received event {info:?}");
                        continue;
                    }

                    info!("found new event {} in path {:?}", info.event, info.paths);
                    if let Some(_cmd) = self.args.command.as_deref() {
                        if let Some(child) = cur_child.as_mut() {
                            // check child still live?
                            if let Some(status) = child.try_wait()? {
                                // terminated
                                debug!(
                                    "cleaning exited status {:?} for process {}",
                                    status.code(),
                                    child.id()
                                );

                                // rerun for next timeout
                                cur_child.take();
                            } else {
                                // alive
                                trace!("Writing info `{info:?}` to be formatted to the stdin of existing process {}", child.id());
                                // dont close stdin, only release lock to unblock
                                self.format(&info, child.stdin.as_mut().unwrap())?;

                                last_info = None;
                                continue;
                            }
                        }

                        last_info = Some(info);
                    } else {
                        trace!("Writing info `{info:?}` to be formatted to the stdout");
                        self.format(&info, &mut io::stdout())?;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Some(cmd) = self.args.command.as_deref() {
                        if let Some(info) = last_info.take() {
                            let mut child = if let Some(mut child) = cur_child.take() {
                                // check child still live?
                                let child = if let Some(status) = child.try_wait()? {
                                    // terminated
                                    info!(
                                        "Executing command again for exited status {:?} command process {}",
                                        status.code(),
                                        child.id()
                                    );
                                    self.spawn(cmd)?
                                } else {
                                    // alive
                                    debug!(
                                        "found running process {} for recv timeout {}",
                                        child.id(),
                                        timeout.as_secs()
                                    );
                                    child
                                };
                                child
                            } else {
                                info!("starting new process `{cmd}`");
                                self.spawn(cmd)?
                            };

                            // input to proc stdin
                            trace!("Writing info `{info:?}` to be formatted to the stdin of process {}", child.id());
                            self.format(&info, child.stdin.as_mut().unwrap())?;

                            cur_child = Some(child);
                        }

                        last_info = None;
                    }
                }
                Err(e) => bail!(e),
            }
        }
    }

    fn spawn(&self, cmd: &str) -> Result<Child> {
        if self.args.tty {
            let sh = "sh";
            trace!("executing command `{cmd}` in tty {sh}");
            make_script_command(cmd, Some(sh)).and_then(|mut c| c.stdin(Stdio::piped()).spawn())
        } else {
            let args =
                shlex::split(cmd).ok_or_else(|| anyhow!("Unable to parse the command: {cmd}"))?;

            trace!("executing command `{cmd}` with args: {args:?}");
            let name = &args[0];
            Command::new(name)
                .args(&args[1..])
                .stdin(Stdio::piped())
                // .stdout(Stdio::piped())
                // .stderr(Stdio::piped())
                .spawn()
        }
        .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::LevelFilter;
    use std::{
        env, fs,
        path::Path,
        sync::{mpsc::channel, Once},
    };
    use tempfile::tempdir;

    static CRATE_NAME: &str = env!("CARGO_CRATE_NAME");

    #[ctor::ctor]
    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            env_logger::builder()
                .is_test(true)
                .filter_level(LevelFilter::Info)
                .filter_module(CRATE_NAME, LevelFilter::Trace)
                .init();
        });
    }

    #[test]
    fn test_args() -> Result<()> {
        let args = Args::parse_from::<&[&str], _>(&[]);
        assert_eq!(args.events, EventType::iter().collect::<Vec<_>>());
        assert_eq!(args.paths, vec![Path::new(".").to_owned()]);

        Ok(())
    }

    #[test]
    fn test_interested() -> Result<()> {
        let tempdir = tempdir()?;
        env::set_current_dir(&tempdir)?;
        let cur_path = Path::new(".");

        let tmpfile_path = tempdir.path().join("a.txt");
        let (tx, rx) = channel();
        let mut watcher = notify::recommended_watcher(tx)?;
        trace!("watching path {}", cur_path.display());
        watcher.watch(cur_path, RecursiveMode::NonRecursive)?;

        {
            info!("writing {} for watching", tmpfile_path.display());
            fs::write(&tmpfile_path, "2323")?;
        }

        let timeout = Duration::from_secs(2);
        info!("waiting timeout {}s for watching", timeout.as_secs());
        let event = rx.recv_timeout(timeout)??;
        assert_eq!(event.paths.first(), Some(&tmpfile_path));
        // "/tmp/.tmpb9lPL7/./a.txt" = "/tmp/.tmpb9lPL7/a.txt"
        assert_ne!(event.paths.first().unwrap().to_str(), tmpfile_path.to_str());

        let _event_path = event.paths.first().unwrap();
        let info = EventInfo {
            event: EventType::Create,
            paths: vec![tmpfile_path],
        };

        let args =
            Args::parse_from(format!("{CRATE_NAME} -E *.txt {}", cur_path.display()).split(' '));
        assert!(!Cli::new(&args)?.is_interested(&info));

        let args =
            Args::parse_from(format!("{CRATE_NAME} -E **/* {}", cur_path.display()).split(' '));
        assert!(!Cli::new(&args)?.is_interested(&info));

        let args =
            Args::parse_from(format!("{CRATE_NAME} -E *.jpg {}", cur_path.display()).split(' '));
        assert!(Cli::new(&args)?.is_interested(&info));

        let args = Args::parse_from(format!("{CRATE_NAME} {}", cur_path.display()).split(' '));
        assert!(Cli::new(&args)?.is_interested(&info));

        Ok(())
    }
}
