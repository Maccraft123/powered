use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use mio::unix::SourceFd;
use std::os::fd::AsRawFd;

use anyhow::{Context, Result};
use clap::Parser;
use glob::glob;
use mio::{Events, Interest, Poll, Token};
use serde::{Deserialize, Deserializer};
use udev::{Device, MonitorBuilder};

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
struct Trigger {
    name: String,
    device: String,
    property: String,
    #[serde(rename = "value-map")]
    map: HashMap<String, String>,
}

impl Trigger {
    fn name(&self) -> &str {
        &self.name
    }
    fn device(&self) -> &str {
        &self.device
    }
    fn value<'me>(&'me self, d: &Device) -> Option<&'me str> {
        assert_eq!(*d.devpath(), *self.device);
        if let Some(raw) = d.property_value(&self.property) {
            self.map
                .get(str::from_utf8(raw.as_bytes()).unwrap())
                .map(|x| x.as_str())
        } else {
            eprintln!(
                "{}: Property '{}' not found in device '{}'",
                self.name, self.property, self.device
            );
            None
        }
    }
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
                let iter: Result<Vec<_>, _> = glob(file)?.collect();
                for path in iter? {
                    vprintln!("Writing {} to {}", val, path.display());
                    fs::write(path, val).context("Failed to write to simple-file on trigger")?;
                }
            }
        }

        Ok(())
    }
    fn de_sysctl<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let s = String::deserialize(d)?;

        let path = s.split(".").fold(String::from("/proc/sys"), |path, seg| {
            format!("{path}/{seg}")
        });

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

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Enable extra debug output
    #[arg(short, long)]
    verbose: bool,
    /// Config file
    #[arg(short, long)]
    cfg: PathBuf,
    /// Exit immediately after applying current profile
    #[arg(short, long)]
    oneshot: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    VERBOSE.store(args.verbose, Ordering::SeqCst);

    vprintln!("Got args: {:#?}", args);

    let cfg_str = fs::read_to_string(&args.cfg).context("Failed to read the config file")?;
    let cfg: Config = toml::from_str(&cfg_str).context("Failed to deserialize the config file")?;

    vprintln!("Got config: {:#?}", cfg);

    let mut trigger_map: HashMap<OsString, &Trigger> = HashMap::new();
    for trig in &cfg.trigger {
        let device = trig.device().to_owned().into();
        trigger_map.insert(device, trig);
    }

    let mon = MonitorBuilder::new()?
        .match_subsystem("power_supply")?
        .listen()?;

    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);
    poll.registry().register(
        &mut SourceFd(&mon.as_raw_fd()),
        Token(0),
        Interest::READABLE | Interest::WRITABLE,
    )?;

    loop {
        poll.poll(&mut events, None)?;

        for mio_event in &events {
            if mio_event.token() != Token(0) || !mio_event.is_writable() {
                continue;
            }

            for ev in mon.iter() {
                if let Some(handler) = trigger_map.get_mut(ev.devpath()) {
                    let name = handler.name().to_owned();
                    vprintln!("Running trigger {:?}", &name);

                    if let Some(val) = handler.value(&ev.device()) {
                        vprintln!("Trigger value: {:?}", val);
                        if let Err(e) = cfg.on_trigger(&name, val) {
                            eprintln!("{e:#}");
                        }
                    }
                }
            }
        }
    }
}
