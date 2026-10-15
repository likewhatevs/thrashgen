//! Recording format: the JSON-lines file `record` writes and `aggregate`
//! reads. The first line is a [`Header`]; every later line is a
//! [`Sample`] (counters and memory over one interval) or a
//! [`CpuSample`] (busy cores over one second).

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::profile::{CpuIdent, Provenance};

/// Format version written in every header.
pub const FORMAT_VERSION: u32 = 1;

/// What a recording observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A cgroup v2 directory, by path relative to the cgroup root.
    Cgroup {
        /// Path such as `system.slice/foo.service`.
        path: String,
    },
    /// An existing process and its threads.
    Process {
        /// Process id.
        pid: u32,
    },
    /// A command the recorder launched.
    Command {
        /// The command line.
        argv: Vec<String>,
    },
    /// Everything on the machine.
    Host,
}

/// First line of a recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// [`FORMAT_VERSION`] at the time of writing.
    pub version: u32,
    /// Hostname.
    pub host: String,
    /// Kernel release.
    pub kernel: String,
    /// CPU identity, when it could be determined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<CpuIdent>,
    /// Wall-clock start, seconds since the Unix epoch.
    pub started_unix_s: f64,
    /// Requested spacing of [`Sample`] lines, seconds.
    pub interval_s: f64,
    /// Requested spacing of [`CpuSample`] lines, seconds.
    pub cpu_interval_s: f64,
    /// What was recorded.
    pub target: Target,
    /// Scope, mode and method per field, constant for the recording.
    #[serde(default)]
    pub provenance: BTreeMap<String, Provenance>,
}

/// Counters and memory over one interval, for one scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    /// Unix seconds at the end of the interval.
    pub t: f64,
    /// Scope identifier: cgroup path, pid, or `host`.
    pub scope: String,
    /// Length of the interval actually elapsed, seconds.
    pub dt: f64,
    /// Field values, keyed as in [`crate::profile::FIELDS`]; rates are
    /// per second, levels are the mean over the interval.
    pub values: BTreeMap<String, f64>,
}

/// Busy cores over one short interval, for one scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CpuSample {
    /// Unix seconds at the end of the interval.
    pub t: f64,
    /// Scope identifier, as in [`Sample`].
    pub scope: String,
    /// CPU time consumed divided by wall time elapsed.
    pub cores: f64,
}

/// One line of a recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// The first line.
    Header(Header),
    /// A counter and memory sample.
    Sample(Sample),
    /// A busy-cores sample.
    Cpu(CpuSample),
}

/// A whole recording in memory.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    /// The header line.
    pub header: Header,
    /// Counter and memory samples in file order.
    pub samples: Vec<Sample>,
    /// Busy-cores samples in file order.
    pub cpu: Vec<CpuSample>,
}

/// Errors reading a recording.
#[derive(Debug, Error)]
pub enum ReadError {
    /// The file is empty.
    #[error("recording is empty")]
    Empty,
    /// The first line is not a header.
    #[error("line 1 is not a header")]
    NoHeader,
    /// A header appeared after the first line.
    #[error("line {0}: second header")]
    ExtraHeader(usize),
    /// Unsupported format version.
    #[error("format version {0} is newer than {FORMAT_VERSION}")]
    Version(u32),
    /// A line did not parse.
    #[error("line {line}: {source}")]
    Parse {
        /// 1-based line number.
        line: usize,
        /// Parser error.
        #[source]
        source: serde_json::Error,
    },
    /// Underlying I/O.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Recording {
    /// Parse a recording from its lines.
    pub fn read(reader: impl BufRead) -> Result<Self, ReadError> {
        let mut header = None;
        let mut samples = Vec::new();
        let mut cpu = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line?;
            let n = idx + 1;
            if line.trim().is_empty() {
                continue;
            }
            let record: Record = serde_json::from_str(&line)
                .map_err(|source| ReadError::Parse { line: n, source })?;
            match record {
                Record::Header(h) => {
                    if header.is_some() {
                        return Err(ReadError::ExtraHeader(n));
                    }
                    if h.version > FORMAT_VERSION {
                        return Err(ReadError::Version(h.version));
                    }
                    header = Some(h);
                }
                Record::Sample(s) => {
                    if header.is_none() {
                        return Err(ReadError::NoHeader);
                    }
                    samples.push(s);
                }
                Record::Cpu(c) => {
                    if header.is_none() {
                        return Err(ReadError::NoHeader);
                    }
                    cpu.push(c);
                }
            }
        }
        let header = header.ok_or(ReadError::Empty)?;
        Ok(Recording {
            header,
            samples,
            cpu,
        })
    }

    /// Write the recording as JSON lines.
    pub fn write(&self, mut w: impl Write) -> io::Result<()> {
        write_record(&mut w, &Record::Header(self.header.clone()))?;
        for s in &self.samples {
            write_record(&mut w, &Record::Sample(s.clone()))?;
        }
        for c in &self.cpu {
            write_record(&mut w, &Record::Cpu(c.clone()))?;
        }
        Ok(())
    }

    /// Distinct scope identifiers, in first-seen order.
    pub fn scopes(&self) -> Vec<&str> {
        let mut seen = Vec::new();
        for s in self
            .samples
            .iter()
            .map(|s| s.scope.as_str())
            .chain(self.cpu.iter().map(|c| c.scope.as_str()))
        {
            if !seen.contains(&s) {
                seen.push(s);
            }
        }
        seen
    }
}

/// Write one record as a single JSON line.
pub fn write_record(mut w: impl Write, record: &Record) -> io::Result<()> {
    serde_json::to_writer(&mut w, record)?;
    w.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Method, Mode, Scope};

    fn header() -> Header {
        Header {
            version: FORMAT_VERSION,
            host: "box".into(),
            kernel: "7.1.0".into(),
            cpu: None,
            started_unix_s: 1_700_000_000.0,
            interval_s: 60.0,
            cpu_interval_s: 1.0,
            target: Target::Cgroup {
                path: "a.slice/b".into(),
            },
            provenance: BTreeMap::from([(
                "ipc".to_string(),
                Provenance {
                    scope: Scope::Cgroup,
                    mode: Mode::All,
                    method: Method::Perf,
                },
            )]),
        }
    }

    fn recording() -> Recording {
        Recording {
            header: header(),
            samples: vec![Sample {
                t: 1_700_000_060.0,
                scope: "a.slice/b".into(),
                dt: 60.0,
                values: BTreeMap::from([("ipc".to_string(), 1.5), ("anon_bytes".to_string(), 1e9)]),
            }],
            cpu: vec![
                CpuSample {
                    t: 1_700_000_001.0,
                    scope: "a.slice/b".into(),
                    cores: 2.0,
                },
                CpuSample {
                    t: 1_700_000_002.0,
                    scope: "a.slice/b".into(),
                    cores: 2.5,
                },
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let r = recording();
        let mut buf = Vec::new();
        r.write(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(text.lines().count(), 4);
        assert!(text.starts_with(r#"{"kind":"header","#));
        assert!(
            text.lines()
                .nth(1)
                .unwrap()
                .starts_with(r#"{"kind":"sample","#)
        );
        assert!(
            text.lines()
                .nth(2)
                .unwrap()
                .starts_with(r#"{"kind":"cpu","#)
        );
        let back = Recording::read(text.as_bytes()).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.scopes(), ["a.slice/b"]);
    }

    #[test]
    fn target_tags() {
        let t = Target::Command {
            argv: vec!["sleep".into(), "1".into()],
        };
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r#"{"kind":"command","argv":["sleep","1"]}"#
        );
        assert_eq!(
            serde_json::to_string(&Target::Host).unwrap(),
            r#"{"kind":"host"}"#
        );
        assert_eq!(
            serde_json::to_string(&Target::Process { pid: 7 }).unwrap(),
            r#"{"kind":"process","pid":7}"#
        );
    }

    #[test]
    fn blank_lines_are_skipped() {
        let r = recording();
        let mut buf = Vec::new();
        r.write(&mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap().replace('\n', "\n\n");
        assert_eq!(Recording::read(text.as_bytes()).unwrap(), r);
    }

    #[test]
    fn errors() {
        assert!(matches!(
            Recording::read("".as_bytes()),
            Err(ReadError::Empty)
        ));
        assert!(matches!(
            Recording::read("\n \n".as_bytes()),
            Err(ReadError::Empty)
        ));

        let sample = r#"{"kind":"sample","t":1,"scope":"x","dt":1,"values":{}}"#;
        assert!(matches!(
            Recording::read(sample.as_bytes()),
            Err(ReadError::NoHeader)
        ));
        let cpu = r#"{"kind":"cpu","t":1,"scope":"x","cores":1}"#;
        assert!(matches!(
            Recording::read(cpu.as_bytes()),
            Err(ReadError::NoHeader)
        ));

        let h = serde_json::to_string(&Record::Header(header())).unwrap();
        let two = format!("{h}\n{h}\n");
        assert!(matches!(
            Recording::read(two.as_bytes()),
            Err(ReadError::ExtraHeader(2))
        ));

        let bad = format!("{h}\nnot json\n");
        match Recording::read(bad.as_bytes()) {
            Err(ReadError::Parse { line: 2, .. }) => {}
            other => panic!("{other:?}"),
        }

        let mut future = header();
        future.version = FORMAT_VERSION + 1;
        let f = serde_json::to_string(&Record::Header(future)).unwrap();
        assert!(
            matches!(Recording::read(f.as_bytes()), Err(ReadError::Version(v)) if v == FORMAT_VERSION + 1)
        );
    }

    #[test]
    fn scopes_are_in_first_seen_order_across_both_streams() {
        let mut r = recording();
        r.cpu.insert(
            0,
            CpuSample {
                t: 0.0,
                scope: "z".into(),
                cores: 0.0,
            },
        );
        r.samples.push(Sample {
            t: 0.0,
            scope: "m".into(),
            dt: 1.0,
            values: BTreeMap::new(),
        });
        assert_eq!(r.scopes(), ["a.slice/b", "m", "z"]);
    }
}
