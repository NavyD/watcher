use std::{
    io::{self, Write},
    path::PathBuf,
    process::{exit, Child, Command, Stdio},
    sync::mpsc::{sync_channel, Receiver, RecvTimeoutError},
    time::Duration,
};

use anyhow::{anyhow, bail, Result};
use clap::{CommandFactory, Parser, ValueEnum, ValueHint};
use clap_complete::{generate, Shell};
use fake_tty::make_script_command;
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use itertools::Itertools;
use log::{debug, error, info, trace, warn};
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

    if let Some(sh) = args.generator {
        let mut cmd = Args::command();
        let name = cmd.get_name().to_string();
        generate(sh, &mut cmd, name, &mut io::stdout());
        Ok(())
    } else {
        let mut watcher = EventWatcher::new(&args)?;
        watcher.start()?;

        if let Some(command) = args.command {
            match args.run_mode {
                RunMode::Single => {
                    let pargs = ProcessArgs {
                        command,
                        send_stdin: args.send_stdin,
                        tty: args.tty,
                    };
                    SingleProcessHandler::new(pargs).handle(watcher)?;
                }
            }
        } else {
            SimpleHandler.handle(watcher)?;
        }
        todo!()
    }
}

/// A simple project to monitor file events and run commands
#[derive(Parser, Debug)]
#[command(author, version)]
pub struct Args {
    /// outputs the completion file for given shell
    #[arg(long = "generate", value_enum)]
    generator: Option<Shell>,

    /// log level. default error
    #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 1)]
    verbose: u8,

    /// recursive for paths
    #[arg(short, long, default_value_t = false)]
    recurive: bool,

    /// Timeout for rechecking monitoring events
    #[arg(short = 'i', long, default_value = "1s")]
    poll_interval: humantime::Duration,

    /// Run a command in sh, pretending to be a tty. useful for example: docker exec
    #[arg(short, long, default_value_t = false)]
    tty: bool,

    /// send events to process stdin
    #[arg(long, default_value_t = false)]
    send_stdin: bool,

    #[arg(short = 'm', long, default_value_t = RunMode::Single)]
    run_mode: RunMode,

    /// run a command and send event to the stdin
    #[arg(short, long, value_hint = ValueHint::CommandString)]
    command: Option<String>,

    /// Listen for specific events
    #[arg(short, long, default_values_t = EventType::iter().collect::<Vec<_>>(), value_delimiter = ',')]
    events: Vec<EventType>,

    /// Exclude all events on files matching the globs <pattern>.
    /// higher priority than include
    #[arg(short = 'E', long, value_hint = ValueHint::Unknown)]
    excludes: Option<Vec<String>>,

    /// include all events on files matching the globs <pattern>.
    #[arg(short = 'I', long, value_hint = ValueHint::Unknown)]
    includes: Option<Vec<String>>,

    /// the monitoring paths
    #[clap(default_value = ".", value_hint = ValueHint::AnyPath)]
    paths: Vec<PathBuf>,
}

impl Args {
    fn init_log(&self) -> Result<()> {
        let verbose = self.verbose;
        if verbose > 5 {
            return Err(anyhow!("invalid log level {} number of verbose", verbose));
        }
        let level: log::LevelFilter = unsafe { std::mem::transmute(verbose as usize) };
        env_logger::builder()
            .filter_level(log::LevelFilter::Error)
            .filter_module(module_path!(), level)
            .init();
        Ok(())
    }

    fn build_glob(&self, s: &str) -> Result<Glob> {
        GlobBuilder::new(s)
            .case_insensitive(false)
            .literal_separator(true)
            .build()
            .map_err(Into::into)
    }

    fn build_include_globs(&self) -> Result<Option<GlobSet>> {
        self.includes
            .as_deref()
            .map(|globs| {
                globs
                    .iter()
                    .map(|s| self.build_glob(s))
                    .fold_ok(GlobSetBuilder::new(), |mut b, g| {
                        b.add(g);
                        b
                    })
                    .and_then(|b| b.build().map_err(Into::into))
                    .map(Some)
            })
            .unwrap_or(Ok(None))
    }

    fn build_exclude_globs(&self) -> Result<Option<GlobSet>> {
        self.excludes
            .as_deref()
            .map(|globs| {
                globs
                    .iter()
                    .map(|s| self.build_glob(s))
                    .fold_ok(GlobSetBuilder::new(), |mut b, g| {
                        b.add(g);
                        b
                    })
                    .and_then(|b| b.build().map_err(Into::into))
                    .map(Some)
            })
            .unwrap_or(Ok(None))
    }
}

#[derive(
    Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, EnumString, Display, EnumIter,
)]
// clap [clap::Command] attr: rename_all is default kebab-case
#[strum(serialize_all = "kebab-case")]
enum RunMode {
    Single,
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
pub struct EventInfo {
    event: EventType,
    paths: Vec<PathBuf>,
}

pub struct EventWatcher {
    watcher: Option<RecommendedWatcher>,
    rx: Receiver<EventInfo>,
    event_filter: EventFilter,
    timeout: Duration,
    recurive: bool,
    paths: Vec<PathBuf>,
}

impl EventWatcher {
    pub fn new(args: &Args) -> Result<Self> {
        let channel_size = 1000;
        let (tx, rx) = sync_channel(channel_size);
        let watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            match res {
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
            }
        })?;

        Ok(Self {
            rx,
            watcher: Some(watcher),
            event_filter: EventFilter::new(args)?,
            timeout: args.poll_interval.into(),
            paths: args.paths.clone(),
            recurive: args.recurive,
        })
    }

    pub fn iter(&self) -> Result<Iter> {
        todo!()
    }

    fn start(&mut self) -> Result<&mut Self> {
        let rec_mod = if self.recurive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let paths = &self.paths;
        for path in paths {
            self.watcher.as_mut().unwrap().watch(path, rec_mod)?;
        }

        println!(
            "waiting {:?} events in interval {}s for {}{} paths: {:?}",
            self.event_filter.events,
            self.timeout.as_secs(),
            if self.recurive { "recurive " } else { "" },
            paths.len(),
            paths
        );
        Ok(self)
    }

    fn wait_next(&self) -> Option<Option<EventInfo>> {
        loop {
            return match self.rx.recv_timeout(self.timeout) {
                Ok(info) => {
                    if self.event_filter.filter(&info) {
                        Some(Some(info))
                    } else {
                        trace!("skipped received event {info:?}");
                        continue;
                    }
                }
                Err(RecvTimeoutError::Timeout) => Some(None),
                Err(RecvTimeoutError::Disconnected) => {
                    info!("close the iter for disconnected channel");
                    None
                }
            };
        }
    }
}

pub struct Iter<'a> {
    e: &'a EventWatcher,
}

impl<'a> Iterator for Iter<'a> {
    type Item = Option<EventInfo>;

    fn next(&mut self) -> Option<Self::Item> {
        self.e.wait_next()
    }
}

pub struct IntoIter {
    e: EventWatcher,
}

impl Iterator for IntoIter {
    type Item = Option<EventInfo>;
    fn next(&mut self) -> Option<Self::Item> {
        self.e.wait_next()
    }
}

impl IntoIterator for EventWatcher {
    type Item = Option<EventInfo>;

    type IntoIter = IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        IntoIter { e: self }
    }
}

pub struct EventFilter {
    events: Vec<EventType>,
    exclude_globs: Option<GlobSet>,
    include_globs: Option<GlobSet>,
}

impl EventFilter {
    pub fn new(args: &Args) -> Result<Self> {
        let exclude_globs = args.build_exclude_globs()?;
        let include_globs = args.build_include_globs()?;
        Ok(Self {
            events: args.events.clone(),
            exclude_globs,
            include_globs,
        })
    }

    pub fn filter(&self, info: &EventInfo) -> bool {
        self.events.iter().any(|e| *e == info.event)
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
}

pub trait Handler {
    fn handle<I>(&self, events: I) -> Result<()>
    where
        I: IntoIterator<Item = Option<EventInfo>>;

    fn format(&self, e: &EventInfo, writer: &mut impl Write) -> Result<()> {
        // TODO: format
        let path_str = e.paths.iter().filter_map(|p| p.to_str()).join(",");
        writeln!(writer, "{} {}", e.event, path_str)?;
        Ok(())
    }
}

pub trait ProcessHandler: Handler {
    fn new(args: ProcessArgs) -> Self;

    fn process_args(&self) -> &ProcessArgs;

    fn spawn(&self) -> Result<Child> {
        let args = self.process_args();
        debug!("spawning new process with args: {args:?}");
        let cmd = &args.command;
        println!("starting new process by command: `{cmd}`");

        let stdin = if args.send_stdin {
            Stdio::piped()
        } else {
            Stdio::inherit()
        };
        if args.tty {
            let sh = "sh";
            trace!("executing command `{cmd}` in tty {sh}");
            make_script_command(cmd, Some(sh)).and_then(|mut c| c.stdin(stdin).spawn())
        } else {
            let cmd_args =
                shlex::split(cmd).ok_or_else(|| anyhow!("Unable to parse the command: {cmd}"))?;
            trace!("executing command `{cmd}` with args: {cmd_args:?}");
            let name = &cmd_args[0];
            Command::new(name).args(&cmd_args[1..]).stdin(stdin).spawn()
        }
        .map_err(Into::into)
    }
}

struct SimpleHandler;
impl Handler for SimpleHandler {
    fn handle<I>(&self, events: I) -> Result<()>
    where
        I: IntoIterator<Item = Option<EventInfo>>,
    {
        for e in events.into_iter().flatten() {
            self.format(&e, &mut io::stdout())?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct ProcessArgs {
    tty: bool,
    send_stdin: bool,
    command: String,
}

pub struct SingleProcessHandler {
    args: ProcessArgs,
}

impl Handler for SingleProcessHandler {
    /// 单进程处理事件。
    /// 开始的多个连续的事件将仅在None info出现后启动进程，后面的事件可能send_stdin=true发送给进程stdin。
    /// 如果进程启动后退出，则会重新开始启动
    fn handle<I>(&self, events: I) -> Result<()>
    where
        I: IntoIterator<Item = Option<EventInfo>>,
    {
        let mut cur_child = None::<Child>;
        let mut last_info = None::<EventInfo>;

        for e in events {
            if let Some(info) = e {
                info!("found new event {} in path {:?}", info.event, info.paths);
                if let Some(child) = cur_child.as_mut() {
                    // check child still live?
                    if let Some(status) = child.try_wait()? {
                        if !status.success() {
                            error!(
                                "failed to run process {} with command `{}` exit code {}",
                                child.id(),
                                self.args.command,
                                status.code().unwrap_or(-1)
                            );
                        }
                        // terminated
                        debug!(
                            "cleaning exited status {:?} for process {}",
                            status.code(),
                            child.id()
                        );

                        // rerun for next timeout
                        cur_child.take();
                    } else {
                        if self.args.send_stdin {
                            // alive
                            trace!("Writing info `{info:?}` to be formatted to the stdin of existing process {}", child.id());
                            // dont close stdin, only release lock to unblock
                            self.format(&info, child.stdin.as_mut().unwrap())?;
                        }
                        last_info = None;
                        continue;
                    }
                }

                debug!("waiting event info {info:?} for next timeout");
                last_info = Some(info);
            }
            // timeout
            else {
                trace!(
                    "checking timeout last info `{last_info:?}` for child process {}",
                    cur_child
                        .as_ref()
                        .map(|c| c.id().to_string())
                        .unwrap_or_else(|| "none".to_string())
                );
                cur_child = if let Some(mut child) = cur_child.take() {
                    if let Some(status) = child.try_wait()? {
                        // terminated
                        trace!(
                            "found exited process {} with status: {:?}",
                            child.id(),
                            status.code(),
                        );
                        if !status.success() {
                            error!(
                                "failed to run command `{}` with status {}",
                                self.args.command,
                                status.code().unwrap_or_default()
                            );
                        }
                        None
                    } else {
                        Some(child)
                    }
                } else {
                    None
                };

                if let Some(info) = last_info.take() {
                    let mut child = if let Some(child) = cur_child.take() {
                        child
                    } else {
                        self.spawn()?
                    };

                    // input to proc stdin
                    if self.args.send_stdin {
                        trace!(
                            "Writing info `{info:?}` to be formatted to the stdin of process {}",
                            child.id()
                        );
                        self.format(&info, child.stdin.as_mut().unwrap())?;
                    }

                    cur_child = Some(child);
                }

                last_info = None;
            }
        }

        bail!("events hang up")
    }
}

impl ProcessHandler for SingleProcessHandler {
    fn process_args(&self) -> &ProcessArgs {
        &self.args
    }

    fn new(args: ProcessArgs) -> Self {
        Self { args }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::LevelFilter;
    use rand::{distributions::Alphanumeric, Rng};
    use std::{
        env, fs,
        path::Path,
        sync::{mpsc::SendError, Once},
        thread::{self, sleep},
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

    fn rand_writes<P: AsRef<Path>>(paths: &[P]) -> Result<Vec<&P>> {
        paths
            .iter()
            .map(|p| {
                let path = p.as_ref();
                if let Some(path) = path.parent() {
                    fs::create_dir_all(path)?;
                }
                let s = rand::thread_rng()
                    .sample_iter(&Alphanumeric)
                    .take(10)
                    .map(char::from)
                    .collect::<String>();
                trace!("writing {} for watching with content: {s}", path.display());
                fs::write(path, s)?;
                Ok(p)
            })
            .collect::<Result<Vec<_>>>()
    }

    #[test]
    fn test_watcher_iter() -> Result<()> {
        let dir = tempdir()?;
        env::set_current_dir(&dir)?;
        let root = dir.path();
        let args = Args::parse_from(format!("{CRATE_NAME} -r {}", root.display()).split(' '));
        let mut watcher = EventWatcher::new(&args)?;
        watcher.start()?;
        let tmp_paths = ["a/b/c/c.txt", "a/b/b.txt", "a/a.txt"].map(|s| dir.path().join(s));
        {
            let watcher = watcher.watcher.take().unwrap();
            let tmp_paths = tmp_paths.clone();
            thread::spawn(move || {
                let effect_paths = rand_writes(&tmp_paths)?;
                assert_eq!(
                    &tmp_paths,
                    effect_paths
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .as_slice()
                );

                // waiting for receiver
                thread::sleep(Duration::from_millis(1000));
                // drop watcher and close iter
                drop(watcher);
                Ok::<_, anyhow::Error>(())
            });
        }

        let events = watcher.into_iter().collect::<Vec<_>>();
        info!("received {} events: {events:?}", events.len());
        // at least create,modify events for per path. and create/access dir
        assert!(tmp_paths.len() * 2 < events.len());
        // events contains per path
        assert!(tmp_paths.iter().all(|p| {
            events
                .iter()
                .any(|e| e.as_ref().map_or(false, |e| e.paths.contains(p)))
        }));

        Ok(())
    }

    #[test]
    fn test_single_handler() -> Result<()> {
        let events = [
            // no event for timeout
            None,
            // new event
            Some(EventInfo {
                event: EventType::Create,
                paths: vec![PathBuf::from("a.txt")],
            }),
            Some(EventInfo {
                event: EventType::Create,
                paths: vec![PathBuf::from("b.txt")],
            }),
            // timeout
            None,
            None,
            // new event
            Some(EventInfo {
                event: EventType::Create,
                paths: vec![PathBuf::from("c.txt")],
            }),
            None,
            Some(EventInfo {
                event: EventType::Create,
                paths: vec![PathBuf::from("d.txt")],
            }),
            None,
        ];

        let (tx, rx) = sync_channel(1);
        let dur = Duration::from_millis(500);
        thread::spawn(move || {
            for e in events {
                info!("sending event {e:?}");
                tx.send(e)?;
                sleep(dur);
            }
            Ok::<_, SendError<_>>(())
        });

        let args = ProcessArgs {
            command: "false".to_string(),
            send_stdin: true,
            tty: false,
        };
        let handler = SingleProcessHandler { args };
        let res = handler.handle(rx);
        debug!("handler result: {res:?}");
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("events hang up"));
        Ok(())
    }

    #[test]
    fn test_interested_mock() -> Result<()> {
        let pat = "**/etc/*";
        let args = Args::parse_from(format!(r#"{CRATE_NAME} -I {pat} dot_xxx"#).split(' '));
        let info = EventInfo {
            event: EventType::Create,
            paths: vec![PathBuf::from(
                "/home/xxx/.local/share/chezmoi/dot_xxx/etc/systemd/exact_system/a.service.tmpl",
            )],
        };
        let info2 = EventInfo {
            event: EventType::Create,
            paths: vec![PathBuf::from(
                "/home/xxx/.local/share/chezmoi/dot_xxx/etc/wsl.conf.tmpl",
            )],
        };

        let ef = EventFilter::new(&args)?;
        let globs = ef.include_globs.as_ref().unwrap();
        assert!(!globs.is_match(&info.paths[0]));
        assert!(globs.is_match(&info2.paths[0]));

        assert!(!ef.filter(&info));
        assert!(ef.filter(&info2));
        Ok(())
    }

    #[test]
    fn test_interested_excludes() -> Result<()> {
        let pat = "**/avs/failed/**";
        let args = Args::parse_from(format!(r#"{CRATE_NAME} -E {pat} dot_xxx"#).split(' '));
        let info = EventInfo {
            event: EventType::Create,
            paths: vec![PathBuf::from(
                "/home/xxx/.local/share/chezmoi/dot_xxx/avs/JAV_output/etc/systemd/exact_system/a.service.tmpl",
            )],
        };
        let info2 = EventInfo {
            event: EventType::Create,
            paths: vec![PathBuf::from(
                "/home/xxx/.local/share/chezmoi/dot_xxx/avs/failed/etc/wsl.conf.tmpl",
            )],
        };

        let ef = EventFilter::new(&args)?;
        let globs = ef.exclude_globs.as_ref().unwrap();
        assert!(!globs.is_match(&info.paths[0]));
        assert!(globs.is_match(&info2.paths[0]));

        assert!(ef.filter(&info));
        assert!(!ef.filter(&info2));

        let pat = "**/avs/failed/**";
        let args = Args::parse_from(format!(r#"{CRATE_NAME} -E {pat} dot_xxx"#).split(' '));
        let info = EventInfo {
            event: EventType::Create,
            paths: vec![PathBuf::from(
                "/home/xxx/.local/share/chezmoi/dot_xxx/avs/failed",
            )],
        };
        let ef = EventFilter::new(&args)?;
        let globs = ef.exclude_globs.as_ref().unwrap();
        assert!(!globs.is_match(&info.paths[0]));
        assert!(ef.filter(&info));
        Ok(())
    }
}
