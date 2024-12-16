use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::thread::available_parallelism;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use itertools::Itertools;
use regex::Regex;
use serde::{Deserialize, Serialize};
use termion::{color, style};
use walkdir::WalkDir;

use crate::component::{Component, Constraints, PrepareReturn};
use crate::config::global_config::GlobalConfig;
use crate::constants::INDENT_WIDTH;
use crate::default_prepare;

/// A container for component configuration from the configuration file
#[derive(Clone, Deserialize)]
pub struct CgStats {
    /// File where to store Cgroup statistic needed by the next run
    state_file: String,
    /// List only Cgroups with higher CPU usage (0.01 ~ 1%)
    threshold: f64,
}

#[async_trait]
impl Component for CgStats {
    fn prepare(self: Box<Self>, global_config: &GlobalConfig) -> PrepareReturn {
        self.prepare_or_error(global_config)
            .map_err(|e| {
                eprintln!("cg_stats error: {e}");
                e
            })
            .unwrap_or((self, Some(Constraints { min_width: None })))
    }
    async fn print(self: Box<Self>, _global_config: &GlobalConfig, _width: Option<usize>) {
        println!("cg_stats component failed");
    }
}

impl CgStats {
    pub fn prepare_or_error(
        &self,
        _global_config: &GlobalConfig,
    ) -> Result<PrepareReturn, Box<dyn Error>> {
        let num_cpus = available_parallelism()?.get();
        let now = read_cg_state()?;

        let mut prepared_cg_stats = PreparedCgStats::default();

        if let Ok(before) = fs::read_to_string(&self.state_file)
            .and_then(|s| toml::from_str::<State>(&s).map_err(io::Error::other))
        {
            let time_span = now.time.duration_since(before.time)?;
            let treshold = self.threshold;
            prepared_cg_stats.time_span = time_span;
            prepared_cg_stats.users =
                get_prepared_stats(&now.user, &before.user, time_span, num_cpus, treshold);
            prepared_cg_stats.services =
                get_prepared_stats(&now.system, &before.system, time_span, num_cpus, treshold);
            prepared_cg_stats.max_name_width = prepared_cg_stats
                .users
                .iter()
                .chain(prepared_cg_stats.services.iter())
                .map(|s| s.name.len())
                .max()
                .unwrap_or(0);
        }
        fs::write(&self.state_file, toml::to_string(&now)?)?;
        let min_width = INDENT_WIDTH + prepared_cg_stats.max_name_width + 12 + 5;
        Ok((
            Box::new(prepared_cg_stats),
            Some(Constraints {
                min_width: Some(min_width),
            }),
        ))
    }
}

struct PreparedStat {
    name: String,
    load: f64, // CPU load [0, 1]
}

#[derive(Default)]
pub struct PreparedCgStats {
    time_span: Duration,
    max_name_width: usize,
    users: Vec<PreparedStat>,
    services: Vec<PreparedStat>,
}

#[async_trait]
impl Component for PreparedCgStats {
    async fn print(self: Box<Self>, global_config: &GlobalConfig, width: Option<usize>) {
        let secs = self.time_span.as_secs();
        let rounded_time = if secs < 180 {
            Duration::from_secs(secs)
        } else {
            Duration::from_secs((secs + 30) / 60 * 60)
        };
        println!(
            "CPU usage in the past {}:",
            humantime::format_duration(rounded_time)
        );
        let indent = " ".repeat(INDENT_WIDTH);
        let width = width.unwrap_or(global_config.progress_width - INDENT_WIDTH);
        let bar_width = width - INDENT_WIDTH - self.max_name_width - 1 - 5;
        for (title, data) in [("Users", &self.users), ("Services", &self.services)] {
            if !data.is_empty() {
                println!("{indent}{title}:");
            }
            for stat in data {
                println!(
                    "{indent}{indent}{name:<width$} {percent:3.0}% {bar}",
                    name = stat.name,
                    bar = format_bar(global_config, bar_width, stat.load),
                    percent = stat.load * 100.0,
                    width = self.max_name_width,
                );
            }
        }
        println!();
    }

    default_prepare!();
}

/// Statistics read from a single cgroup
#[derive(Serialize, Deserialize)]
struct CgStat {
    usage_usec: u64, // CPU usage
}

/// Statistics from multiple cgroups read at certain time. CPU usage
/// is calculated from two instances of State taken at different
/// times.
#[derive(Serialize, Deserialize)]
struct State {
    time: SystemTime,
    user: HashMap<String, CgStat>,   // user.slice
    system: HashMap<String, CgStat>, // system.slice
}

fn full_color(ratio: f64) -> String {
    match (ratio * 100.) as usize {
        0..=75 => color::Fg(color::Green).to_string(),
        76..=95 => color::Fg(color::Yellow).to_string(),
        _ => color::Fg(color::Red).to_string(),
    }
}

fn format_bar(global_config: &GlobalConfig, width: usize, full_ratio: f64) -> String {
    let without_ends_width =
        width - global_config.progress_suffix.len() - global_config.progress_prefix.len();

    let bar_full = ((without_ends_width as f64) * full_ratio.clamp(0.0, 1.0)).round() as usize;
    let bar_empty = without_ends_width - bar_full;
    let full_color = full_color(full_ratio);

    [
        global_config.progress_prefix.to_string(),
        full_color,
        global_config
            .progress_full_character
            .to_string()
            .repeat(bar_full),
        color::Fg(color::LightBlack).to_string(),
        global_config
            .progress_empty_character
            .to_string()
            .repeat(bar_empty),
        style::Reset.to_string(),
        global_config.progress_suffix.to_string(),
    ]
    .join("")
}

/// Calculate CPU usage from two states taken at different times. The
/// result will include only Cgroups with CPU usage >= threshold.
fn get_prepared_stats(
    now: &HashMap<String, CgStat>,
    before: &HashMap<String, CgStat>,
    time_span: Duration,
    num_cpus: usize,
    threshold: f64,
) -> Vec<PreparedStat> {
    let mut stats = Vec::new();
    for key in now.keys().sorted() {
        if before.contains_key(key) {
            let s1 = before.get(key).unwrap();
            let s2 = now.get(key).unwrap();
            let load = (s2.usage_usec as i64 - s1.usage_usec as i64) as f64
                / time_span.as_micros() as f64
                / num_cpus as f64;
            if load >= threshold {
                stats.push(PreparedStat {
                    name: key.clone(),
                    load,
                });
            }
        }
    }
    stats
}

/// Read statistics from a single Cgroup
fn read_cg_stat(cg_path: &Path) -> Result<CgStat, Box<dyn Error>> {
    let path = cg_path.join("cpu.stat");
    let f = File::open(path.clone())?;
    for line in BufReader::new(f).lines() {
        let l = line?;
        let (key, value) = l
            .split_whitespace()
            .next_tuple()
            .ok_or_else(|| io::Error::other(format!("Reading fields from {path:?}")))?;
        match (key, value.parse::<u64>()?) {
            ("usage_usec", val) => return Ok(CgStat { usage_usec: val }),
            _ => (),
        }
    }
    Err(io::Error::other("Missing {field} in {path}").into())
}

/// Read statistics from direct children of a Cgroup given by `slice`.
/// The keys of the returned hash map are the names of Cgroups passed
/// through the `rename_key` function.
fn read_stats<F>(slice: &str, rename_key: F) -> Result<HashMap<String, CgStat>, Box<dyn Error>>
where
    F: Fn(&str) -> String,
{
    let mut stats = HashMap::new();
    for entry in WalkDir::new(["/sys/fs/cgroup", slice].iter().collect::<PathBuf>())
        .min_depth(1)
        .max_depth(1)
    {
        let e = entry?;
        if e.file_type().is_dir() {
            let stat = read_cg_stat(e.path())?;
            stats.insert(rename_key(&e.file_name().to_string_lossy()), stat);
        }
    }
    Ok(stats)
}

fn read_cg_state() -> Result<State, Box<dyn Error>> {
    let mut state = State {
        time: SystemTime::now(),
        user: HashMap::new(),
        system: HashMap::new(),
    };
    // Read statistics of system services and shorten too long names, e.g.,
    // docker-dcd9a8c71b756de71a4a837c005840f84e0ed92574704ae1c89409c57980aaee.scope
    let re = Regex::new(r"\.service|\.scope|\.slice")?;
    state.system = read_stats("system.slice", |key| {
        let name_no_suffix = re.replace(key, "");
        let max_len = 23;
        if name_no_suffix.len() <= max_len {
            name_no_suffix.to_string()
        } else {
            let mut name = name_no_suffix.to_string();
            name.truncate(max_len - 3);
            name += "...";
            name
        }
    })?;

    // Read statistics of users and convert UIDs to user names
    let re = Regex::new(r"^user-([0-9]+)\.slice$")?;
    state.user = read_stats("user.slice", |key| match re.captures(key) {
        Some(cap) => {
            let uid = match cap[1].parse::<u32>() {
                Ok(uid) => uid,
                Err(_) => return key.to_owned(),
            };
            let user = users::get_user_by_uid(uid);
            let username = user.as_ref().and_then(|u| u.name().to_str()).unwrap_or(key);
            username.to_owned()
        }
        None => key.to_owned(),
    })?;
    Ok(state)
}
