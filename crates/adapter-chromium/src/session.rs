use crate::ChromiumError;
use cdp::protocol::{dom, runtime};

/// One attached page session (flat mode).
#[derive(Clone)]
pub struct Session {
    pub client: cdp::Client,
    pub id: String,
    service_clock: crate::service_clock::ServiceClock,
}

impl Session {
    pub fn new(client: cdp::Client, id: String) -> Self {
        Self { client, id, service_clock: Default::default() }
    }

    /// Another page of the same renderer: a dialog open in any of them blocks every page command, so they
    /// share one service clock.
    pub fn sharing_clock(client: cdp::Client, id: String, service_clock: crate::service_clock::ServiceClock) -> Self {
        Self { client, id, service_clock }
    }

    pub fn service_clock(&self) -> crate::service_clock::ServiceClock {
        self.service_clock.clone()
    }

    /// Active host-service bound on one page command. Not a determinism path: it only turns a page that blocks its
    /// main thread forever into an error instead of a hang.
    pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

    pub async fn call<C: cdp::Command>(&self, params: &C) -> Result<C::Returns, ChromiumError> {
        match self.service_clock.timeout(Self::CALL_TIMEOUT, self.client.call(Some(&self.id), params)).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(ChromiumError::CallTimeout(C::METHOD)),
        }
    }

    /// Sends without waiting for the answer (`cdp::Client::send`).
    pub fn send<C: cdp::Command>(&self, params: &C) -> Result<cdp::Answer, ChromiumError> {
        Ok(self.client.send(Some(&self.id), params)?)
    }

    pub fn subscribe<E: cdp::Event>(&self) -> cdp::Subscription<E> {
        self.client.subscribe(Some(&self.id))
    }

    pub async fn eval(
        &self,
        expr: &str,
        context: Option<runtime::ExecutionContextId>,
    ) -> Result<serde_json::Value, ChromiumError> {
        let r = self
            .call(&runtime::EvaluateParams {
                return_by_value: Some(true),
                await_promise: Some(true),
                context_id: context,
                ..runtime::EvaluateParams::new(expr.into())
            })
            .await?;
        if let Some(ex) = r.exception_details {
            let detail = ex.exception.and_then(|e| e.description).unwrap_or_default();
            return Err(ChromiumError::Script(format!("{} {detail}", ex.text)));
        }
        Ok(r.result.value.unwrap_or(serde_json::Value::Null))
    }

    /// Evaluate only in the specified browser-unique context; numeric ids may be reused.
    pub async fn eval_unique(&self, expr: &str, context: &str) -> Result<serde_json::Value, ChromiumError> {
        let r = self.call(&runtime::EvaluateParams {
            return_by_value: Some(true), await_promise: Some(true), unique_context_id: Some(context.into()),
            ..runtime::EvaluateParams::new(expr.into())
        }).await?;
        if let Some(ex) = r.exception_details {
            let detail = ex.exception.and_then(|e| e.description).unwrap_or_default();
            return Err(ChromiumError::Script(format!("{} {detail}", ex.text)));
        }
        Ok(r.result.value.unwrap_or(serde_json::Value::Null))
    }

    pub async fn eval_object(
        &self,
        expr: &str,
        context: Option<runtime::ExecutionContextId>,
    ) -> Result<Option<runtime::RemoteObjectId>, ChromiumError> {
        let r = self
            .call(&runtime::EvaluateParams { context_id: context, ..runtime::EvaluateParams::new(expr.into()) })
            .await?;
        if let Some(ex) = r.exception_details {
            return Err(ChromiumError::Script(ex.text));
        }
        Ok(r.result.object_id)
    }

    pub async fn backend_id_of(&self, object: runtime::RemoteObjectId) -> Result<dom::BackendNodeId, ChromiumError> {
        let d = self.call(&dom::DescribeNodeParams { object_id: Some(object), ..Default::default() }).await?;
        Ok(d.node.backend_node_id)
    }
}
