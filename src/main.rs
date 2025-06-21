use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use glob::glob;
use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify, WatchDescriptor};
use nix::unistd::geteuid;
use serde::{Deserialize, Deserializer, Serialize};

static VERBOSE: AtomicBool = AtomicBool::new(false);
fn verbose() -> bool {
    VERBOSE.load(Ordering::SeqCst)
}

macro_rules! vprintln {
    ($($tt: tt)*) => {
        if verbose() {
            println!($($tt)*)
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TriggerKind {
    SimpleFile,
}

#[derive(Debug, Deserialize)]
struct Trigger {
    name: String,
    #[serde(rename = "type")]
    kind: TriggerKind,
    file: PathBuf,
    #[serde(rename = "value-map")]
    map: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Action {
    trigger: String,
    values: HashMap<String, String>,
    #[serde(flatten)]
    inner: ActionInner,
}

impl Action {
    fn on_trigger(&self, t: &str, value: &str) -> Result<()> {
        if t != self.trigger {
            return Ok(());
        }

        if let Some(val) = self.values.get(value) {
            self.inner.run(val)?;
        } else {
            vprintln!("Didn't find value for key {}", value);
        }

        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
enum ActionInner {
    SimpleFile {
        file: String,
    },
    Sysctl {
        #[serde(rename = "ctl", deserialize_with = "ActionInner::de_sysctl")]
        file: String,
    },
}

impl ActionInner {
    fn run(&self, val: &str) -> Result<()> {
        match self {
            ActionInner::SimpleFile { file } | ActionInner::Sysctl { file } => {
                let mut iter: Result<Vec<_>, _> = glob(file)?.collect();
                for path in iter? {
                    vprintln!("Writing {} to {}", val, path.display());
                    fs::write(path, val).context("Failed to write to simple-file on trigger")?;
                }
            },
        }

        Ok(())
    }
    fn de_sysctl<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let s = String::deserialize(d)?;

        let path = s.split(".")
            .fold(String::from("/proc/sys"), |path, seg| format!("{path}/{seg}"));

        if Path::new(&path).exists() {
            Ok(path)
        } else {
            use serde::de::Error;
            let err = format!("No sysctl knob: '{s}'");
            Err(D::Error::custom(err))
        }
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    action: Vec<Action>,
    trigger: Vec<Trigger>,
}

impl Config {
    fn on_trigger(&self, trig: &str, value: &str) -> Result<()> {
        for action in self.action.iter() {
            action.on_trigger(trig, value)?;
        }
        Ok(())
    }
}

struct TriggerHandler<'a> {
    trigger: &'a Trigger,
    last_access: Option<Instant>,
    cached_val: Option<&'a String>,
}

impl<'a> TriggerHandler<'a> {
    fn new<'b>(trigger: &'a Trigger, inotify: &'b Inotify) -> Result<(Self, WatchDescriptor)> {
        let desc = inotify.add_watch(&trigger.file, AddWatchFlags::IN_ACCESS)?;

        Ok((Self {
            last_access: None,
            trigger,
            cached_val: None,
        }, desc))
    }
    fn name(&self) -> &str {
        &self.trigger.name
    }
    fn poll_and_name(&mut self) -> Result<(Option<&str>, &str)> {
        if self.last_access.is_some_and(|instant| instant.elapsed() < Duration::from_millis(50)) {
            return Ok((None, &self.trigger.name));
        }

        let raw = fs::read_to_string(&self.trigger.file)?;
        self.last_access = Some(Instant::now());
        let val = self.trigger.map.get(raw.trim());

        if val.is_none() {
            eprintln!("Warning: No value map for {} in trigger {}", raw, self.trigger.name);
        }

        if val != self.cached_val {
            self.cached_val = val;
            Ok((self.cached_val.map(|s| s.as_str()), &self.trigger.name))
        } else {
            Ok((None, &self.trigger.name))
        }
    }
}

#[derive(Debug, Clone, Parser)]
struct Args {
    #[arg(short, long)]
    verbose: bool,
    #[arg(short, long)]
    cfg: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    VERBOSE.store(args.verbose, Ordering::SeqCst);

    println!("Got args: {:#?}", args);

    let cfg_str = fs::read_to_string(&args.cfg)
        .context("Failed to read the config file")?;
    let cfg: Config = toml::from_str(&cfg_str)
        .context("Failed to deserialize the config file")?;

    println!("Got config: {:#?}", cfg);

    let mut trigger_map = HashMap::new();

    let inotify = Inotify::init(InitFlags::empty())
        .context("Failed to initialize an inotify instance")?;
    for trig in &cfg.trigger {
        let (mut handler, watch) = TriggerHandler::new(trig, &inotify)?;
        let (value, name) = handler.poll_and_name()?;

        if let Some(val) = value {
            if verbose() {
                println!("Init trigger {:?} result: {:?}", name, value);
            }
            if let Err(e) = cfg.on_trigger(name, &val) {
                eprintln!("{e:#}");
            }
        }

        trigger_map.insert(watch, handler);
    }

    loop {
        let events = inotify.read_events().unwrap();
        for ev in &events {
            if verbose() {
                println!("Processing event: {:#?}", ev);
            }

            if let Some(handler) = trigger_map.get_mut(&ev.wd) {
                let (value, name) = handler.poll_and_name()?;
                if let Some(val) = value {
                    if verbose() {
                        println!("Trigger {:?} result: {:?}", name, value);
                    }
                    if let Err(e) = cfg.on_trigger(name, &val) {
                        eprintln!("{e:#}");
                    }
                }
            }
        }
    }
    Ok(())
}
