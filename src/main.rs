use std::{
    io::Write,
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
use log::{debug, info, trace, warn};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use strum::{Display, EnumString};

/// Simple program to greet a person  
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, action = clap::ArgAction::Count, default_value_t = 1)]
    verbose: u8,

    #[arg(short, long, default_value_t = false)]
    recurive: bool,

    #[arg(short = 'i', long, default_value = "30s")]
    poll_interval: humantime::Duration,

    #[arg(short, long)]
    command: Option<String>,

    // /// 指定该选项可以在命令执行错误的情况下重新执行，而不是终止当前程序
    // #[arg(long, default_value_t = false)]
    // ignore_command_failed: bool,

    // #[arg(short, long)]
    // format: Option<String>,

    #[arg(short, long)]
    events: Option<Vec<EventType>>,

    /// 选择需要监控的路径
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
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, ValueEnum, EnumString, Display)]
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

struct Cli {
    args: Args,
    watcher: Arc<Mutex<RecommendedWatcher>>,
    rx: Receiver<EventInfo>,
}

impl Cli {
    pub fn new() -> Result<Self> {
        let mut args = Args::parse();
        args.init_log()?;

        if args.paths.is_empty() {
            info!("use current path . by default");
            args.paths.push(".".into());
        }

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
        })
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

        let interested_events = self.args.events.as_ref();
        info!(
            "waiting {} events in {} {} paths: {:?}",
            if let Some(es) = interested_events {
                format!("{es:?}")
            } else {
                "all".to_string()
            },
            if self.args.recurive { "recurive" } else { "" },
            paths.len(),
            paths
        );
        let timeout: Duration = self.args.poll_interval.into();

        let mut cur_child = None::<Child>;
        let mut last_info = None::<EventInfo>;
        loop {
            trace!(
                "waiting timeout {} for next info. last info: {last_info:?}",
                timeout.as_secs()
            );
            match self.rx.recv_timeout(timeout) {
                Ok(info) => {
                    if interested_events
                        .filter(|es| es.iter().all(|e| *e != info.event))
                        .is_some()
                    {
                        debug!("skipped received event {info:?}");
                        continue;
                    }

                    // 已存在的进程发送stdin 或 已终止时重启任务 或 重新计数任务

                    if let Some(_cmd) = self.args.command.as_deref() {
                        if let Some(child) = cur_child.as_mut() {
                            // check child still live?
                            if let Some(status) = child.try_wait()? {
                                // terminated
                                info!(
                                    "Executing command again for exited status {:?} command process {}",
                                    status.code(),
                                    child.id()
                                );

                                // rerun for next timeout
                                cur_child.take();
                            } else {
                                // alive
                                debug!(
                                    "found running process {} for recv timeout {}",
                                    child.id(),
                                    timeout.as_secs()
                                );

                                // input to proc stdin
                                {
                                    let line = format!("{info:?}");
                                    let stdin = child.stdin.as_mut().unwrap();
                                    writeln!(stdin, "{line}")?;
                                    // dont close stdin, only release lock to unblock
                                }
                                last_info = None;
                                continue;
                            }
                        }

                        last_info = Some(info);
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
                                info!("starting new process `{cmd}` by info: {info:?}");
                                self.spawn(cmd)?
                            };

                            // input to proc stdin
                            {
                                let line = format!("{info:?}");
                                let stdin = child.stdin.as_mut().unwrap();
                                writeln!(stdin, "{line}")?;
                                // dont close stdin, only release lock to unblock
                            }

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
        let args =
            shlex::split(cmd).ok_or_else(|| anyhow!("Unable to parse the command: {cmd}"))?;

        debug!("executing command with args: {args:?}");
        let name = &args[0];
        Command::new(name)
            .args(&args[1..])
            .stdin(Stdio::piped())
            // .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(Into::into)
    }
}

fn main() {
    if let Err(e) = Cli::new().and_then(|cli| cli.start()) {
        eprintln!("failed to run cli: {e}");
        exit(1)
    }
}
