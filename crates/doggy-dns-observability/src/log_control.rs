//! Runtime log filtering: the stdout and OTLP log layers each read a
//! reloadable `EnvFilter`, and [`LogLevelHandle`] swaps them without a restart.
//!
//! The reload wrapper goes around the `EnvFilter`, not the layer: `reload`'s
//! `Layer::on_event` takes the lock on every event, while the `Filter` reads it
//! only for callsite registration and span bookkeeping (`Handle::modify`
//! rebuilds the interest cache).

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tracing_subscriber::Registry;
use tracing_subscriber::filter::{EnvFilter, Filtered};
use tracing_subscriber::layer::Layered;
use tracing_subscriber::reload;

/// The OTel SDK's own logs must never be forwarded through the OTLP bridge:
/// re-exporting them can recursively trigger another export. `EnvFilter`
/// targets match by raw string prefix, so one directive covers every
/// `opentelemetry*` crate and only `tracing_opentelemetry` needs its own.
const OTLP_SELF_OFF: &str = "opentelemetry=off,tracing_opentelemetry=off";

/// `EnvFilter` picks the longest matching target, so a directive naming an
/// `opentelemetry*` or `tracing_opentelemetry*` target would out-rank
/// [`OTLP_SELF_OFF`]. Every target in those crates contains this fragment, so a
/// directive must be refused when it does.
const OTEL_TARGET_FRAGMENT: &str = "opentelemetry";

/// Whether a directive string may name one of the OTel crates the OTLP output
/// must never forward; installing it would let the bridge re-export the SDK's
/// own logs.
pub(crate) fn targets_otel(directives: &str) -> bool {
    directives.contains(OTEL_TARGET_FRAGMENT)
}

pub(crate) type BoxedLayer<S> = Box<dyn tracing_subscriber::Layer<S> + Send + Sync + 'static>;

/// The stdout output's layer: any formatter, filtered by the reloadable filter.
pub(crate) type StdoutLayer =
    Filtered<BoxedLayer<Registry>, reload::Layer<EnvFilter, Registry>, Registry>;
/// The subscriber stack once the stdout output is installed.
pub(crate) type WithStdout = Layered<StdoutLayer, Registry>;
/// The OTLP logs bridge, filtered by its own reloadable filter.
pub(crate) type OtlpLayer =
    Filtered<BoxedLayer<WithStdout>, reload::Layer<EnvFilter, WithStdout>, WithStdout>;

/// The directives in force on each log output; `None` means that output is off.
/// The OTLP output always appends its own exclusions to what is reported here.
#[derive(Debug, Clone)]
pub struct LogLevels {
    pub stdout: Option<String>,
    pub otlp: Option<String>,
}

/// Retargets the process's log filters at runtime. Cheap to clone; every clone
/// edits the same installed layers.
#[derive(Clone)]
pub struct LogLevelHandle(Arc<Inner>);

impl Default for LogLevelHandle {
    /// A handle for a process with no log outputs; `Observability::init`
    /// replaces it with the handle of the subscriber it installs.
    fn default() -> Self {
        Self(Arc::new(Inner {
            stdout: None,
            otlp: None,
            baseline: String::new(),
            applied: Mutex::new(LogLevels {
                stdout: None,
                otlp: None,
            }),
        }))
    }
}

struct Inner {
    stdout: Option<Output<Registry>>,
    otlp: Option<Output<WithStdout>>,
    /// Directives this process started with; [`LogLevelHandle::reset`] restores them.
    baseline: String,
    applied: Mutex<LogLevels>,
}

struct Output<S> {
    handle: reload::Handle<EnvFilter, S>,
    /// Appended to every filter installed on this output. Empty for stdout.
    suffix: &'static str,
}

/// The two log layers plus the handle that swaps their filters.
pub(crate) struct LogLayers {
    pub stdout: StdoutLayer,
    pub otlp: Option<OtlpLayer>,
    pub handle: LogLevelHandle,
}

/// Build the log layers. `stdout` is the formatter to filter, `stdout_enabled`
/// says whether that output is on, `baseline` is the directive string both
/// outputs start from ([`LogLevelHandle::reset`] restores it), and a missing
/// OTLP layer means no bridge is installed.
pub(crate) fn build(
    stdout: BoxedLayer<Registry>,
    stdout_enabled: bool,
    otlp: Option<BoxedLayer<WithStdout>>,
    baseline: &str,
) -> Result<LogLayers> {
    let stdout_directives = if stdout_enabled { baseline } else { "off" };
    let stdout_filter = EnvFilter::try_new(stdout_directives)
        .with_context(|| format!("invalid stdout log filter '{stdout_directives}'"))?;
    let (stdout_filter, stdout_handle) = reload::Layer::new(stdout_filter);
    let stdout_layer = Filtered::new(stdout, stdout_filter);
    let stdout = if stdout_enabled {
        Some(Output {
            handle: stdout_handle,
            suffix: "",
        })
    } else {
        None
    };

    let (otlp_layer, otlp) = match otlp {
        Some(bridge) => {
            let filter = EnvFilter::try_new(compose(baseline, OTLP_SELF_OFF))
                .with_context(|| format!("invalid OTLP log filter '{baseline}'"))?;
            let (filter, handle) = reload::Layer::new(filter);
            let output = Output {
                handle,
                suffix: OTLP_SELF_OFF,
            };
            (Some(Filtered::new(bridge, filter)), Some(output))
        }
        None => (None, None),
    };

    let applied = LogLevels {
        stdout: stdout.as_ref().map(|_| baseline.to_string()),
        otlp: otlp.as_ref().map(|_| baseline.to_string()),
    };
    let handle = LogLevelHandle(Arc::new(Inner {
        stdout,
        otlp,
        baseline: baseline.to_string(),
        applied: Mutex::new(applied),
    }));

    Ok(LogLayers {
        stdout: stdout_layer,
        otlp: otlp_layer,
        handle,
    })
}

impl LogLevelHandle {
    /// The directives each output is currently filtering by.
    pub fn current(&self) -> LogLevels {
        self.0.lock().clone()
    }

    /// Replace the directives in force on every enabled output. Rejects blank
    /// strings, unparsable directives, and ones naming an OTel target.
    pub fn set(&self, directives: &str) -> Result<LogLevels, String> {
        let directives = directives.trim();
        if directives.is_empty() {
            return Err("directives must not be empty".to_string());
        }
        if targets_otel(directives) {
            return Err(
                "directives must not name an opentelemetry* target: the OTLP output always \
                 excludes the OTel SDK's own crates"
                    .to_string(),
            );
        }
        self.0.swap(directives)
    }

    /// Restore each output to the directives it started with.
    pub fn reset(&self) -> Result<LogLevels, String> {
        self.0.swap(&self.0.baseline)
    }
}

impl<S> Output<S> {
    fn filter(&self, directives: &str) -> Result<EnvFilter, String> {
        EnvFilter::try_new(compose(directives, self.suffix)).map_err(|error| error.to_string())
    }

    fn install(&self, filter: EnvFilter) -> Result<(), String> {
        self.handle
            .modify(|current| *current = filter)
            .map_err(|error| error.to_string())
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, LogLevels> {
        self.applied
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Parse, install and report one directive string on every enabled output.
    /// Both outputs parse before either is changed, and the guard is held across
    /// the install so the reported state and the installed filters move together.
    /// Each entry is stamped only after its own install succeeds, so a failed
    /// second install still leaves the report truthful.
    fn swap(&self, directives: &str) -> Result<LogLevels, String> {
        let mut applied = self.lock();
        let stdout = self
            .stdout
            .as_ref()
            .map(|output| output.filter(directives))
            .transpose()?;
        let otlp = self
            .otlp
            .as_ref()
            .map(|output| output.filter(directives))
            .transpose()?;
        if let (Some(output), Some(filter)) = (&self.stdout, stdout) {
            output.install(filter)?;
            applied.stdout = Some(directives.to_string());
        }
        if let (Some(output), Some(filter)) = (&self.otlp, otlp) {
            output.install(filter)?;
            applied.otlp = Some(directives.to_string());
        }
        Ok(applied.clone())
    }
}

fn compose(directives: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        directives.to_string()
    } else {
        format!("{directives},{suffix}")
    }
}
