//! `log` records go to logcat under the tag `ext4android`.

use std::ffi::{CStr, CString, c_char, c_int};

#[link(name = "log")]
unsafe extern "C" {
    fn __android_log_write(prio: c_int, tag: *const c_char, text: *const c_char) -> c_int;
}

const TAG: &CStr = c"ext4android";

// android/log.h priorities
const VERBOSE: c_int = 2;
const DEBUG: c_int = 3;
const INFO: c_int = 4;
const WARN: c_int = 5;
const ERROR: c_int = 6;

struct Logcat;

impl log::Log for Logcat {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let prio = match record.level() {
            log::Level::Error => ERROR,
            log::Level::Warn => WARN,
            log::Level::Info => INFO,
            log::Level::Debug => DEBUG,
            log::Level::Trace => VERBOSE,
        };
        let text = format!("{}: {}", record.target(), record.args()).replace('\0', "\\0");
        let Ok(text) = CString::new(text) else {
            return;
        };
        // SAFETY: both strings are NUL-terminated and outlive the call
        unsafe { __android_log_write(prio, TAG.as_ptr(), text.as_ptr()) };
    }

    fn flush(&self) {}
}

static LOGGER: Logcat = Logcat;

pub fn init() {
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(if cfg!(debug_assertions) {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        });
    }
}
