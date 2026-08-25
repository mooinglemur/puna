//! A very small `--key value` parser, shared by both tools.
//!
//! Hand-rolled rather than `clap`, which would be the obvious reach and is a real dependency for
//! two dev binaries nobody deploys. The repository has no argument parser at all today — every
//! shipped tier takes its configuration from the environment — so adding one to the graph for this
//! would be the largest thing in the change by some distance.
//!
//! What it gives up is worth naming: no abbreviations, no `-x` short forms, no subcommands. What it
//! keeps is the part that matters for a tool run by hand — **an unknown flag is an error naming
//! itself**, rather than being ignored while somebody wonders why `--slots 200` did nothing.

use anyhow::{Result, bail};
use std::collections::HashMap;
use std::str::FromStr;

pub struct Args {
    values: HashMap<String, String>,
    flags: Vec<String>,
}

impl Args {
    /// Parse `--key value` pairs and bare `--flag`s, refusing anything not in `known`.
    pub fn parse(known: &[&str], bare: &[&str]) -> Result<Self> {
        let mut values = HashMap::new();
        let mut flags = Vec::new();
        let mut it = std::env::args().skip(1);

        while let Some(arg) = it.next() {
            let Some(key) = arg.strip_prefix("--") else {
                bail!("unexpected argument {arg:?}; every option is spelled --like-this");
            };
            if bare.contains(&key) {
                flags.push(key.to_string());
                continue;
            }
            if !known.contains(&key) {
                bail!(
                    "unknown option --{key}. Known: {}",
                    known
                        .iter()
                        .chain(bare)
                        .map(|k| format!("--{k}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            let Some(value) = it.next() else {
                bail!("--{key} needs a value");
            };
            values.insert(key.to_string(), value);
        }
        Ok(Self { values, flags })
    }

    pub fn is_set(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }

    pub fn opt<T: FromStr>(&self, key: &str) -> Result<Option<T>>
    where
        T::Err: std::fmt::Display,
    {
        match self.values.get(key) {
            None => Ok(None),
            Some(raw) => match raw.parse() {
                Ok(v) => Ok(Some(v)),
                Err(e) => bail!("--{key} {raw:?} is not valid: {e}"),
            },
        }
    }

    pub fn get<T: FromStr>(&self, key: &str, default: T) -> Result<T>
    where
        T::Err: std::fmt::Display,
    {
        Ok(self.opt(key)?.unwrap_or(default))
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn require(&self, key: &str) -> Result<&str> {
        match self.values.get(key) {
            Some(v) => Ok(v),
            None => bail!("--{key} is required"),
        }
    }
}

/// Parse a duration written the way a person writes one: `30s`, `5m`, `2h`, or bare seconds.
pub fn duration(raw: &str) -> Result<std::time::Duration> {
    let (number, scale) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3600),
        _ => (raw, 1),
    };
    let seconds: f64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("{raw:?} is not a duration; try 30s, 5m or 2h"))?;
    if seconds < 0.0 {
        bail!("{raw:?} is negative");
    }
    Ok(std::time::Duration::from_secs_f64(seconds * scale as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_the_way_they_are_written() {
        assert_eq!(duration("30").unwrap().as_secs(), 30);
        assert_eq!(duration("30s").unwrap().as_secs(), 30);
        assert_eq!(duration("5m").unwrap().as_secs(), 300);
        assert_eq!(duration("2h").unwrap().as_secs(), 7200);
        assert_eq!(duration("0.5s").unwrap().as_millis(), 500);
        assert!(duration("soon").is_err());
        assert!(duration("-1s").is_err());
    }
}
