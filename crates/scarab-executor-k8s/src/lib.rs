//! Kubernetes adapter for the [`scarab_engine::Executor`] port.
//!
//! Adapter crate: pairs the pure `scarab-engine` domain with `kube` +
//! `k8s-openapi`. Each step becomes a Pod (or Job); this is a stub.

use async_trait::async_trait;
use scarab_engine::ports::{ExecHandle, ExecState};
use scarab_engine::{ExecError, Executor, StepRun};

/// A Kubernetes-backed executor. Holds an optional client so the composition
/// root can construct it without contacting an API server.
pub struct K8sExecutor {
    #[allow(dead_code)]
    client: Option<kube::Client>,
    #[allow(dead_code)]
    namespace: String,
}

impl K8sExecutor {
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            client: None,
            namespace: namespace.into(),
        }
    }

    pub fn with_client(namespace: impl Into<String>, client: kube::Client) -> Self {
        Self {
            client: Some(client),
            namespace: namespace.into(),
        }
    }
}

#[async_trait]
impl Executor for K8sExecutor {
    async fn launch(&self, _step: &StepRun) -> Result<ExecHandle, ExecError> {
        // TODO(kube): build and apply this Pod spec via the kube client.
        let _pod: Option<k8s_openapi::api::core::v1::Pod> = None;
        unimplemented!("K8sExecutor::launch")
    }

    async fn poll(&self, _handle: &ExecHandle) -> Result<ExecState, ExecError> {
        unimplemented!("K8sExecutor::poll")
    }

    async fn cancel(&self, _handle: &ExecHandle) -> Result<(), ExecError> {
        unimplemented!("K8sExecutor::cancel")
    }
}
