use std::collections::HashMap;
use std::fs;
use std::time::{Duration, Instant};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use clap::Parser;
use glob::glob;
use nix::unistd::geteuid;
use nix::sys::inotify::{
    AddWatchFlags,
    Inotify,
    InitFlags,
    WatchDescriptor,
};
use serde::{Serialize, Deserialize};

static VERBOSE: AtomicBool = AtomicBool::new(false);
fn verbose() -> bool {
    VERBOSE.load(Ordering::SeqCst)
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TriggerKind {
    SimpleFile,
}

#[derive(Debug, Serialize, Deserialize)]
struct Trigger {
    name: String,
    #[serde(rename = "type")]
    kind: TriggerKind,
    file: PathBuf,
    #[serde(rename = "value-map")]
    map: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
enum Action {
    SimpleFile {
        trigger: String,
        file: String,
        values: HashMap<String, String>,
    },
}

impl Action {
    fn on_trigger(&self, t: &str, value: &str) -> Result<()> {
        match self {
            Action::SimpleFile { trigger, file, values } => {
                if t != *trigger {
                    return Ok(())
                }

                if let Some(val) = values.get(value) {
                    let mut iter: Result<Vec<_>, _> = glob(file)?.collect();
                    for path in iter? {
                        if verbose() {
                            eprintln!("Writing {} to {}", val, path.display());
                        }
                        fs::write(path, val)
                            .context("Failed to write to simple-file on trigger")?;
                    }
                } else {
                    if verbose() {
                        eprintln!("Didn't find value for key {}", value);
                    }
                }
            },
        }

        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
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
