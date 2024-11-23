use std::thread::available_parallelism;

use async_trait::async_trait;
use serde::Deserialize;
use systemstat::{Platform, System};
use termion::{color, style};

use crate::component::Component;
use crate::config::global_config::GlobalConfig;
use crate::default_prepare;

#[derive(Debug, Deserialize)]
pub struct LoadAvg {
    prefix: String,
    warn_treshold: Option<f32>,
    bad_treshold: Option<f32>,
}

#[async_trait]
impl Component for LoadAvg {
    async fn print(self: Box<Self>, _global_config: &GlobalConfig, _width: Option<usize>) {
        self.print_or_error()
            .unwrap_or_else(|err| println!("LoadAvg error: {}", err));
        println!();
    }
    default_prepare!();
}

impl LoadAvg {
    pub fn print_or_error(self) -> Result<(), std::io::Error> {
        let sys = System::new();
        let lavg = sys.load_average()?;
        let num_cpus = available_parallelism()?.get();
        let warn_treshold = self.warn_treshold.unwrap_or(num_cpus as f32);
        let bad_treshold = self.bad_treshold.unwrap_or((4 * num_cpus) as f32);

        let color = |load| {
            if load >= bad_treshold {
                color::Fg(color::Red).to_string()
            } else if load >= warn_treshold {
                color::Fg(color::Yellow).to_string()
            } else {
                color::Fg(color::Green).to_string()
            }
        };

        println!(
            "{} {}{:.2}{}, {}{:.2}{}, {}{:.2}{}",
            self.prefix,
            color(lavg.one),
            lavg.one,
            style::Reset,
            color(lavg.five),
            lavg.five,
            style::Reset,
            color(lavg.fifteen),
            lavg.fifteen,
            style::Reset
        );

        Ok(())
    }
}
