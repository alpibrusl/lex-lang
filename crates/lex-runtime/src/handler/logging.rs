//! `log` effect: process-wide log level / format / sink state and the `log.*` dispatch.

use super::*;

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub(super) enum LogLevel { Debug, Info, Warn, Error }

#[derive(Clone, Copy, PartialEq)]
pub(super) enum LogFormat { Text, Json }

#[derive(Clone)]
pub(super) enum LogSink {
    Stderr,
    File(std::sync::Arc<Mutex<std::fs::File>>),
}

pub(super) struct LogState {
    pub(super) level: LogLevel,
    pub(super) format: LogFormat,
    pub(super) sink: LogSink,
}

pub(super) fn log_state() -> &'static Mutex<LogState> {
    static STATE: OnceLock<Mutex<LogState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(LogState {
        level: LogLevel::Info,
        format: LogFormat::Text,
        sink: LogSink::Stderr,
    }))
}

pub(super) fn parse_log_level(s: &str) -> Option<LogLevel> {
    match s {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" => Some(LogLevel::Warn),
        "error" => Some(LogLevel::Error),
        _ => None,
    }
}

pub(super) fn level_label(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Debug => "debug",
        LogLevel::Info => "info",
        LogLevel::Warn => "warn",
        LogLevel::Error => "error",
    }
}

pub(super) fn emit_log(level: LogLevel, msg: &str) {
    let state = log_state().lock().unwrap();
    if level < state.level {
        return;
    }
    let ts = chrono::Utc::now().to_rfc3339();
    let line = match state.format {
        LogFormat::Text => format!("[{}] {}: {}\n", ts, level_label(level), msg),
        LogFormat::Json => {
            // Hand-rolled JSON to avoid pulling serde_json into the
            // hot path; msg gets minimal escaping (the four common
            // cases that break a JSON line).
            let escaped = msg
                .replace('\\', "\\\\")
                .replace('"',  "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            format!(
                "{{\"ts\":\"{ts}\",\"level\":\"{}\",\"msg\":\"{escaped}\"}}\n",
                level_label(level),
            )
        }
    };
    let sink = state.sink.clone();
    drop(state);
    match sink {
        LogSink::Stderr => {
            use std::io::Write;
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
        LogSink::File(f) => {
            use std::io::Write;
            if let Ok(mut g) = f.lock() {
                let _ = g.write_all(line.as_bytes());
            }
        }
    }
}

impl DefaultHandler {
    pub(super) fn dispatch_log(&mut self, op: &str, args: Vec<Value>) -> Result<Value, String> {
        match op {
            "debug" | "info" | "warn" | "error" => {
                let msg = expect_str(args.first())?;
                let level = match op {
                    "debug" => LogLevel::Debug,
                    "info" => LogLevel::Info,
                    "warn" => LogLevel::Warn,
                    _ => LogLevel::Error,
                };
                emit_log(level, msg);
                Ok(Value::Unit)
            }
            "set_level" => {
                let s = expect_str(args.first())?;
                match parse_log_level(s) {
                    Some(l) => {
                        log_state().lock().unwrap().level = l;
                        Ok(ok(Value::Unit))
                    }
                    None => Ok(err(Value::Str(format!(
                        "log.set_level: unknown level `{s}`; expected debug|info|warn|error").into()))),
                }
            }
            "set_format" => {
                let s = expect_str(args.first())?;
                let fmt = match s {
                    "text" => LogFormat::Text,
                    "json" => LogFormat::Json,
                    other => return Ok(err(Value::Str(format!(
                        "log.set_format: unknown format `{other}`; expected text|json").into()))),
                };
                log_state().lock().unwrap().format = fmt;
                Ok(ok(Value::Unit))
            }
            "set_sink" => {
                let path = expect_str(args.first())?;
                if path == "-" {
                    log_state().lock().unwrap().sink = LogSink::Stderr;
                    return Ok(ok(Value::Unit));
                }
                if let Err(e) = self.ensure_fs_write_path(path) {
                    return Ok(err(Value::Str(e.into())));
                }
                match std::fs::OpenOptions::new()
                    .create(true).append(true).open(path)
                {
                    Ok(f) => {
                        log_state().lock().unwrap().sink = LogSink::File(std::sync::Arc::new(Mutex::new(f)));
                        Ok(ok(Value::Unit))
                    }
                    Err(e) => Ok(err(Value::Str(format!("log.set_sink `{path}`: {e}").into()))),
                }
            }
            other => Err(format!("unsupported log.{other}")),
        }
    }
}
