//! Process-wide CPU budget for native scanning and proving.

use crate::error::{EngineError, Result};
use std::{error::Error, sync::Mutex};

pub const MAX_NATIVE_THREADS: usize = 64;

/// The default leaves CPU capacity for the host application and other processes.
pub fn default_native_threads(logical_cpus: usize) -> usize {
    (logical_cpus / 2).clamp(1, 8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeThreadPool {
    pub logical_cpus: usize,
    /// `None` means the embedding application already owns the global Rayon pool.
    /// Its limit cannot be established by this initializer.
    pub threads: Option<usize>,
}

static POOL: Mutex<Option<NativeThreadPool>> = Mutex::new(None);

/// Configure once, before the first native scan or proof. `None` honors
/// `RAYON_NUM_THREADS`, otherwise uses half the available logical CPUs, capped
/// at eight. Explicit limits are 1..=64; changing a live pool requires a restart.
/// A caller-owned pool is preserved, but an explicit request cannot silently
/// claim to have limited that pool.
pub fn initialize_native_threads(threads: Option<usize>) -> Result<NativeThreadPool> {
    if let Some(value) = threads {
        validate_threads(value)?;
    }
    let mut pool = POOL.lock().map_err(|_| config_error())?;
    if let Some(info) = *pool {
        if threads.is_some() && threads != info.threads {
            return Err(EngineError::Message(
                "native thread pool already configured; configure the budget before wallet startup"
                    .into(),
            ));
        }
        return Ok(info);
    }
    let explicit = match threads {
        Some(value) => Some(validate_threads(value)?),
        None => match std::env::var("RAYON_NUM_THREADS") {
            Ok(value) => Some(parse_threads(&value)?),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err(config_error()),
        },
    };
    let logical_cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
    let requested = explicit.unwrap_or_else(|| default_native_threads(logical_cpus));
    let info = match rayon::ThreadPoolBuilder::new()
        .num_threads(requested)
        .thread_name(|i| format!("z-stack-worker-{i}"))
        .build_global()
    {
        Ok(()) => NativeThreadPool {
            logical_cpus,
            threads: Some(requested),
        },
        // This builder never opts into use_current_thread. Rayon exposes an I/O
        // source for worker creation failures; its already-initialized error has none.
        Err(error) if error.source().is_none() && threads.is_none() => NativeThreadPool {
            logical_cpus,
            threads: None,
        },
        Err(_) => return Err(EngineError::Message(
            "native thread pool initialization failed; configure the budget before wallet startup"
                .into(),
        )),
    };
    *pool = Some(info);
    Ok(info)
}

fn validate_threads(value: usize) -> Result<usize> {
    if (1..=MAX_NATIVE_THREADS).contains(&value) {
        Ok(value)
    } else {
        Err(config_error())
    }
}

fn parse_threads(value: &str) -> Result<usize> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(config_error());
    }
    validate_threads(value.parse().map_err(|_| config_error())?)
}

fn config_error() -> EngineError {
    EngineError::Message("native thread budget must be an integer from 1 to 64".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_defaults_and_invalid_overrides() {
        for (cores, threads) in [
            (0, 1),
            (1, 1),
            (2, 1),
            (4, 2),
            (8, 4),
            (16, 8),
            (32, 8),
            (128, 8),
        ] {
            assert_eq!(default_native_threads(cores), threads);
        }
        for invalid in [
            "",
            "0",
            "65",
            "-1",
            "+2",
            "2.5",
            " 4",
            "password",
            "999999999999999999999",
        ] {
            assert!(parse_threads(invalid).is_err());
        }
        assert_eq!(parse_threads("22").unwrap(), 22);
    }
}
