#![warn(missing_docs)] // error if there are missing docs
#![allow(clippy::upper_case_acronyms)]

//! This crate provides a basis for creating new Temporal SDKs without completely starting from
//! scratch

#[cfg(test)]
#[macro_use]
pub extern crate assert_matches;
#[macro_use]
extern crate tracing;
extern crate core;

mod abstractions;
#[cfg(feature = "ephemeral-server")]
pub mod ephemeral_server;
mod internal_flags;
mod pollers;
mod protosext;
pub mod replay;
pub(crate) mod retry_logic;
pub mod telemetry;
mod worker;

#[cfg(test)]
mod core_tests;
#[cfg(test)]
#[macro_use]
mod test_help;

pub(crate) use temporal_sdk_core_api::errors;

pub use pollers::{
    Client, ClientOptions, ClientOptionsBuilder, ClientTlsConfig, RetryClient, RetryConfig,
    TlsConfig, WorkflowClientTrait,
};
pub use temporal_sdk_core_api as api;
pub use temporal_sdk_core_protos as protos;
pub use temporal_sdk_core_protos::TaskToken;
pub use url::Url;
pub use worker::{Worker, WorkerConfig, WorkerConfigBuilder};

use crate::{
    replay::{HistoryForReplay, ReplayWorkerInput},
    telemetry::{
        metrics::MetricsContext, remove_trace_subscriber_for_current_thread,
        set_trace_subscriber_for_current_thread, telemetry_init, TelemetryInstance,
    },
};
use futures::Stream;
use std::sync::Arc;
use temporal_client::{ConfiguredClient, TemporalServiceClientWithMetrics};
use temporal_sdk_core_api::{
    errors::{CompleteActivityError, PollActivityError, PollWfError},
    telemetry::TelemetryOptions,
    Worker as WorkerTrait,
};
use temporal_sdk_core_protos::coresdk::ActivityHeartbeat;

/// Create a worker for replaying one or more existing histories. It will auto-shutdown as soon as
/// all histories have finished being replayed.
///
/// You do not necessarily need a [CoreRuntime] for replay workers, but it's advisable to create
/// one and use it to run the replay worker's async functions the same way you would for a normal
/// worker.
pub fn init_replay_worker<I>(rwi: ReplayWorkerInput<I>) -> Result<Worker, anyhow::Error>
where
    I: Stream<Item = HistoryForReplay> + Send + 'static,
{
    info!(
        task_queue = rwi.config.task_queue.as_str(),
        "Registering replay worker"
    );
    rwi.into_core_worker()
}

mod sealed {
    use super::*;

    /// Allows passing different kinds of clients into things that want to be flexible. Motivating
    /// use-case was worker initialization.
    ///
    /// Needs to exist in this crate to avoid blanket impl conflicts.
    pub struct AnyClient(Box<ConfiguredClient<TemporalServiceClientWithMetrics>>);
    impl AnyClient {
        pub(crate) fn into_inner(self) -> Box<ConfiguredClient<TemporalServiceClientWithMetrics>> {
            self.0
        }
    }

    impl From<RetryClient<ConfiguredClient<TemporalServiceClientWithMetrics>>> for AnyClient {
        fn from(c: RetryClient<ConfiguredClient<TemporalServiceClientWithMetrics>>) -> Self {
            Self(Box::new(c.into_inner()))
        }
    }
    impl From<RetryClient<Client>> for AnyClient {
        fn from(c: RetryClient<Client>) -> Self {
            Self(Box::new(c.into_inner().into_inner()))
        }
    }
    impl From<Arc<RetryClient<Client>>> for AnyClient {
        fn from(c: Arc<RetryClient<Client>>) -> Self {
            Self(Box::new(c.get_client().inner().clone()))
        }
    }
    impl From<ConfiguredClient<TemporalServiceClientWithMetrics>> for AnyClient {
        fn from(c: ConfiguredClient<TemporalServiceClientWithMetrics>) -> Self {
            Self(Box::new(c))
        }
    }
}

/// Holds shared state/components needed to back instances of workers and clients. More than one
/// may be instantiated, but typically only one is needed. More than one runtime instance may be
/// useful if multiple different telemetry settings are required.
pub struct CoreRuntime {
    telemetry: TelemetryInstance,
    runtime: Option<tokio::runtime::Runtime>,
    runtime_handle: tokio::runtime::Handle,
}

impl CoreRuntime {
    /// Create a new core runtime with the provided telemetry options and tokio runtime builder.
    /// Also initialize telemetry for the thread this is being called on.
    ///
    /// Note that this function will call the [tokio::runtime::Builder::enable_all] builder option
    /// on the Tokio runtime builder, and will call [tokio::runtime::Builder::on_thread_start] to
    /// ensure telemetry subscribers are set on every tokio thread.
    ///
    /// **Important**: You need to call this *before* calling any async functions on workers or
    /// clients, otherwise the tracing subscribers will not be properly attached.
    ///
    /// # Panics
    /// If a tokio runtime has already been initialized. To re-use an existing runtime, call
    /// [CoreRuntime::new_assume_tokio].
    pub fn new(
        telemetry_options: TelemetryOptions,
        mut tokio_builder: tokio::runtime::Builder,
    ) -> Result<Self, anyhow::Error> {
        let telemetry = telemetry_init(telemetry_options)?;
        let subscriber = telemetry.trace_subscriber();
        let runtime = tokio_builder
            .enable_all()
            .on_thread_start(move || {
                if let Some(sub) = subscriber.as_ref() {
                    set_trace_subscriber_for_current_thread(sub.clone());
                }
            })
            .build()?;
        let _rg = runtime.enter();
        let mut me = Self::new_assume_tokio_initialized_telem(telemetry);
        me.runtime = Some(runtime);
        Ok(me)
    }

    /// Initialize telemetry for the thread this is being called on, assuming a tokio runtime is
    /// already active and this call exists in its context. See [Self::new] for more.
    ///
    /// # Panics
    /// If there is no currently active Tokio runtime
    pub fn new_assume_tokio(telemetry_options: TelemetryOptions) -> Result<Self, anyhow::Error> {
        let telemetry = telemetry_init(telemetry_options)?;
        Ok(Self::new_assume_tokio_initialized_telem(telemetry))
    }

    /// Construct a runtime from an already-initialized telemetry instance, assuming a tokio runtime
    /// is already active and this call exists in its context. See [Self::new] for more.
    ///
    /// # Panics
    /// If there is no currently active Tokio runtime
    pub fn new_assume_tokio_initialized_telem(telemetry: TelemetryInstance) -> Self {
        let runtime_handle = tokio::runtime::Handle::current();
        if let Some(sub) = telemetry.trace_subscriber() {
            set_trace_subscriber_for_current_thread(sub);
        }
        Self {
            telemetry,
            runtime: None,
            runtime_handle,
        }
    }

    /// Get a handle to the tokio runtime used by this Core runtime.
    pub fn tokio_handle(&self) -> tokio::runtime::Handle {
        self.runtime_handle.clone()
    }

    /// Return a reference to the owned [TelemetryInstance]
    pub fn telemetry(&self) -> &TelemetryInstance {
        &self.telemetry
    }

    /// Return a mutable reference to the owned [TelemetryInstance]
    pub fn telemetry_mut(&mut self) -> &mut TelemetryInstance {
        &mut self.telemetry
    }
}

impl Drop for CoreRuntime {
    fn drop(&mut self) {
        remove_trace_subscriber_for_current_thread();
    }
}
