//! The in-process transport (`hyve/docs/03-adapters-and-pipeline.md` §3.2).

use hyve::HyveError;
use hyve::HyveResult;
use hyve::api::{BoxFuture, Call, Reply, Transport, errors};
use orbic_core::{Credential, Payload, Pipeline, RequestMeta};

/// Runs `#[server]` calls through a pipeline in the same process: the
/// request model is handed over as a value (no serialization) and the
/// guards run as for any request. Single binaries and port-less full-stack
/// tests provide it where the calls are made:
/// `hyve::api::provide_transport(&scope, Arc::new(InProcess::new(pipeline)))`.
#[derive(Clone)]
pub struct InProcess {
    pipeline: Pipeline,
    credential: Option<Credential>,
}

impl std::fmt::Debug for InProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InProcess")
            .field("credential", &self.credential)
            .finish_non_exhaustive()
    }
}

impl InProcess {
    /// A transport into `pipeline`, without a credential.
    pub fn new(pipeline: Pipeline) -> Self {
        InProcess {
            pipeline,
            credential: None,
        }
    }

    /// Calls carry `credential` (the signed-in user of a single binary).
    pub fn with_credential(mut self, credential: Credential) -> Self {
        self.credential = Some(credential);
        self
    }
}

impl Transport for InProcess {
    fn call(&self, call: Call) -> BoxFuture<'_, HyveResult<Reply>> {
        let cancellation = call.cancellation().clone();
        let meta = RequestMeta {
            credential: self.credential.clone(),
            ..RequestMeta::default()
        };
        let response = self.pipeline.dispatch_call(call, meta);
        Box::pin(async move {
            let response = tokio::select! {
                r = response => r,
                () = cancellation.cancelled() => {
                    return Err(errors::cancelled("the caller cancelled the call"));
                }
            };
            match response.outcome {
                Ok(Payload::Value(v)) => Ok(Reply::Value(v)),
                Ok(Payload::Empty) => Ok(Reply::NoContent),
                Ok(other) => Err(HyveError::internal(format!(
                    "an in-process call was answered with {other:?}"
                ))),
                Err(e) => Err(e),
            }
        })
    }
}
