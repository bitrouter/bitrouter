//! Private checked host entrypoints. Public protocol values do not prove tenancy.
use super::*;

pub(super) struct Host {
    pub app: Arc<App>,
    pub caller: CallerContext,
    pub request_headers: http::HeaderMap,
    pub harness: Arc<dyn HarnessPort>,
    #[cfg(feature = "harness")]
    pub owned: Option<Arc<crate::tenancy::execution::session::Binding>>,
}

impl Host {
    pub(super) fn check(
        &self,
        grant: &bitrouter_ai::core::protocol::OwnershipGrant,
    ) -> Result<(), CoreError> {
        #[cfg(feature = "harness")]
        if let Some(binding) = &self.owned {
            let service = self.app.native_session_service()?;
            if !binding.belongs_to(&service) {
                return Err(reject(
                    ErrorCode::UnauthorizedScope,
                    "native binding belongs to a different host",
                ));
            }
            binding.grant(grant)?;
            return binding.check();
        }
        let _ = grant;
        self.app.require_legacy_core()
    }
}

impl CoreSession {
    pub(super) fn check_owned(&self) -> Result<(), CoreError> {
        #[cfg(feature = "harness")]
        if let Some(binding) = &self.shared.owned {
            binding.check()?;
        }
        Ok(())
    }
    pub(super) fn check_owned_action(&self, action: &Action) -> Result<(), CoreError> {
        if matches!(action, Action::Interrupt { .. }) {
            return Ok(());
        }
        self.check_owned()
    }
    pub(super) async fn execute_model(
        &self,
        prompt: Prompt,
        headers: http::HeaderMap,
        control: Arc<dyn NativeExecutionControl>,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::types::PipelineResponse> {
        #[cfg(feature = "harness")]
        if let Some(binding) = &self.shared.owned {
            return self
                .shared
                .app
                .execute_native_owned(binding, prompt, headers, control)
                .await;
        }
        self.shared
            .app
            .clone()
            .execute_native_controlled_with_headers(
                prompt,
                self.shared.caller.clone(),
                headers,
                control,
            )
            .await
    }

    #[cfg(feature = "harness")]
    pub(crate) async fn reconcile_owned(&self, durable: &DurableHead) -> Result<(), CoreError> {
        let binding = self.shared.owned.as_ref().ok_or_else(|| {
            reject(
                ErrorCode::UnauthorizedScope,
                "cleanup requires an owned native session",
            )
        })?;
        let grant = self.shared.live.lock().await.gate.grant().clone();
        binding.grant(&grant)?;
        self.reconnect_inner(&grant, durable).await?;
        Ok(())
    }

    #[cfg(feature = "harness")]
    pub(crate) async fn bind_owned_registered<Register, Registered>(
        request: Bind,
        capabilities: &Capabilities,
        app: Arc<App>,
        binding: Arc<crate::tenancy::execution::session::Binding>,
        harness: Arc<dyn HarnessPort>,
        register: Register,
    ) -> Result<Self, CoreError>
    where
        Register: FnOnce(CoreSession) -> Registered + Send,
        Registered: std::future::Future<Output = Result<(), CoreError>> + Send,
    {
        let caller = binding.authority().caller();
        Self::bind_host(
            request,
            capabilities,
            Host {
                app,
                caller,
                request_headers: http::HeaderMap::new(),
                harness,
                owned: Some(binding),
            },
            register,
        )
        .await
    }

    #[cfg(feature = "harness")]
    pub(crate) async fn restore_owned_registered<Register, Registered>(
        request: bitrouter_ai::core::protocol::Restore,
        capabilities: &Capabilities,
        app: Arc<App>,
        binding: Arc<crate::tenancy::execution::session::Binding>,
        harness: Arc<dyn HarnessPort>,
        register: Register,
    ) -> Result<Self, CoreError>
    where
        Register: FnOnce(CoreSession) -> Registered + Send,
        Registered: std::future::Future<Output = Result<(), CoreError>> + Send,
    {
        let caller = binding.authority().caller();
        Self::restore_host(
            request,
            capabilities,
            Host {
                app,
                caller,
                request_headers: http::HeaderMap::new(),
                harness,
                owned: Some(binding),
            },
            register,
        )
        .await
    }
}
