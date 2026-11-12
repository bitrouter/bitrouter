//! Native wire custody at the existing ordered authentication/send boundary.
use super::*;
use crate::gateway::model::protocol::generate_content::native;
use crate::gateway::pipeline::types::{PipelineInput, PipelineOutput, generate_content::Input};
use bitrouter_ai::tenancy::usage_evidence::Source;

pub(super) fn paired(target: &RoutingTarget) -> bool {
    matches!(&target.api_protocol,ApiProtocol::Custom(name)
        if name == bitrouter_ai::tenancy::generate_content::BRIDGE_PROTOCOL)
}

pub(super) struct Opened {
    pub(super) response: reqwest::Response,
    pub(super) tenant: Option<crate::tenancy::receipts::journal::AttemptPermit>,
    pub(super) shared: shared::Attempt,
    pub(super) scrubber: UpstreamErrorScrubber,
    pub(super) started: Instant,
    technical: Option<super::super::provider_technical::Guard>,
}

impl HttpExecutor {
    pub(super) fn preflight_native_generate_content(
        &self,
        target: &RoutingTarget,
        input: &Input,
        stream: bool,
        ctx: &PipelineContext,
    ) -> Result<bitrouter_ai::conversion::ConversionReport> {
        let execution = tenancy::execution(ctx).ok_or(BitrouterError::UpstreamUnavailable)?;
        let PipelineInput::GenerateContent(attached) = ctx.input() else {
            return Err(BitrouterError::UpstreamUnavailable);
        };
        if !input.same_original(attached) {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        execution.validate_generate_content(input)?;
        if paired(target) {
            if target.registry.is_some() {
                return Err(Input::unavailable(&target.api_protocol));
            }
            let (_, transport) = self
                .dispatch
                .lookup(&target.api_protocol)
                .ok_or_else(|| Self::no_dispatch_error(target))?;
            let url = reqwest::Url::parse(&transport.endpoint_url(&target.model_target(), stream))
                .map_err(|_| BitrouterError::UpstreamUnavailable)?;
            if !execution.is_private_adapter_endpoint(&url) {
                return Err(BitrouterError::Forbidden(
                    "unpaired native destination".into(),
                ));
            }
            input.render(&target.service_id, stream)?;
            Ok(Default::default())
        } else {
            let prompt = input
                .projection()
                .ok_or_else(|| Input::unavailable(&target.api_protocol))?;
            execution.admit_native_controls(prompt, &target.api_protocol)?;
            self.preflight_managed(target, prompt, stream, ctx)
        }
    }

    pub(super) async fn open_native_generate_content(
        &self,
        target: &RoutingTarget,
        input: &Input,
        ctx: &PipelineContext,
    ) -> Result<Opened> {
        if !paired(target) {
            return Err(Input::unavailable(&target.api_protocol));
        }
        self.preflight_native_generate_content(target, input, input.stream(), ctx)?
            .require_admitted()?;
        let (_, transport) = self
            .dispatch
            .lookup(&target.api_protocol)
            .ok_or_else(|| Self::no_dispatch_error(target))?;
        let (client, _) = self.client_for(target, true);
        let body = input.render(&target.service_id, input.stream())?;
        let url = transport.endpoint_url(&target.model_target(), input.stream());
        let trace_headers = ctx.take_outbound_trace_headers();
        let mut scrubber = UpstreamErrorScrubber::new(None);
        scrubber.capture_target(target, ctx);
        let started = Instant::now();
        let technical = (!input.stream())
            .then(|| super::super::provider_technical::begin(ctx, target, started, false));
        let request = self
            .build_authenticated_request(&RequestBuildInput {
                client: &client,
                url: &url,
                body: &body,
                managed_expected: None,
                target,
                transport,
                ctx,
                trace_headers: trace_headers.as_ref(),
            })
            .await
            .map_err(|error| scrubber.scrub_error(error))?;
        let mut shared =
            shared::Attempt::begin(self.shared_authority, &request, target, ctx).await?;
        let binding = request.tenant_binding();
        let (mut request, _, mut tenant) = request.into_parts();
        scrubber.capture_request_credentials(&request, target);
        let admission = tenancy::admit_send(
            binding.as_ref(),
            &mut tenant,
            &mut request,
            target,
            ctx,
            Some(input.stream()),
        )?;
        let response = native_work::observe(
            ctx,
            NativeProviderWorkKind::HttpDispatch,
            admission.send(&client, request, ctx, &mut scrubber),
            |response| Some(response.status().as_u16()),
        )
        .await?;
        let status = response.status();
        let retry_after = parse_retry_after(response.headers().get(reqwest::header::RETRY_AFTER));
        receipt::status(&mut tenant, status.as_u16(), retry_after)?;
        if !status.is_success() {
            shared.rejected(status.as_u16()).await?;
            let text = response_body::read(response, ctx)
                .await
                .map_err(|error| scrubber.scrub_error(error))?;
            match native::value(&text) {
                Ok(body) => receipt::native(&mut tenant, Source::HttpError, &body)?,
                Err(_) => {
                    if let Some(permit) = &mut tenant {
                        permit
                            .incomplete_usage()
                            .map_err(|_| BitrouterError::UpstreamUnavailable)?;
                    }
                }
            }
            return Err(classify_upstream_error(
                status.as_u16(),
                &scrubber.scrub_body(&text),
                retry_after,
            ));
        }
        Ok(Opened {
            response,
            tenant,
            shared,
            scrubber,
            started,
            technical,
        })
    }

    pub(super) async fn execute_native_generate_content(
        &self,
        target: &RoutingTarget,
        input: &Input,
        ctx: &PipelineContext,
    ) -> Result<ExecutionResult> {
        if input.stream() {
            return Err(Input::unavailable(&target.api_protocol));
        }
        let Opened {
            response,
            mut tenant,
            mut shared,
            scrubber,
            started,
            technical,
        } = self
            .open_native_generate_content(target, input, ctx)
            .await?;
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if media != Some("application/json") {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "native JSON media type".into(),
                usage: None,
            });
        }
        let text = response_body::read(response, ctx)
            .await
            .map_err(|error| scrubber.scrub_error(error))?;
        let body = native::value(&text).map_err(|error| scrubber.scrub_error(error.into()))?;
        receipt::native(&mut tenant, Source::Json, &body)?;
        let result = native::qualify_json(input, body)
            .map_err(|error| scrubber.scrub_error(error.into()))?;
        // This non-streaming native path never installs the stream estimator.
        // Record that affirmative fact before terminal receipt preparation;
        // historical or unavailable observations remain NotCaptured.
        if let Some(execution) = tenancy::execution(ctx) {
            execution
                .journal()
                .and_then(|journal| {
                    journal
                        .observe_estimator(None)
                        .map_err(crate::tenancy::receipts::journal_error)
                })
                .map_err(|_| BitrouterError::UpstreamUnavailable)?;
        }
        receipt::complete(&mut tenant, true)?;
        shared.complete(true, result.usage()).await?;
        let elapsed = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        if let Some(technical) = &technical {
            technical.completed();
        }
        Ok(ExecutionResult {
            provider_id: target.provider_name.clone(),
            model_id: target.service_id.clone(),
            account_label: target.account_label.clone(),
            result: PipelineOutput::GenerateContent(result),
            request_duration_ms: elapsed,
            upstream_duration_ms: Some(elapsed),
            server_tool_calls: Vec::new(),
        })
    }
    pub(super) async fn execute_native_generate_content_stream(
        &self,
        target: &RoutingTarget,
        input: &Input,
        ctx: &PipelineContext,
    ) -> Result<StreamPartStream> {
        use crate::gateway::model::protocol::StreamDecoder;
        if !input.stream() {
            return Err(Input::unavailable(&target.api_protocol));
        }
        let Opened {
            response,
            mut tenant,
            mut shared,
            scrubber,
            ..
        } = self
            .open_native_generate_content(target, input, ctx)
            .await?;
        let media = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if media != Some("text/event-stream") {
            return Err(BitrouterError::UpstreamInvalidResponse {
                message: "native stream media type".into(),
                usage: None,
            });
        }
        let limit = response_body::limit(ctx);
        response_body::check_length(&response, limit)?;
        let byte_stream = response_body::bounded(response.bytes_stream(), limit);
        let cancellation = tenancy::cancellation(ctx)?;
        let mut decoder = input.install_native_stream()?;
        let operational = tenant
            .as_ref()
            .and_then(|permit| permit.operational())
            .cloned();
        let stream = async_stream::stream! {
            let events=async_stream::try_stream! {
                futures::pin_mut!(byte_stream);
                let mut framing=native::framing::Framing::new(bitrouter_ai::tenancy::generate_content::MAX_REQUEST_BYTES);
                while let Some(chunk)=byte_stream.next().await {
                    for byte in chunk? {
                        if let Some(event)=framing.push(byte).map_err(BitrouterError::from)? {yield event;}
                    }
                }
                framing.finish().map_err(BitrouterError::from)?;
            };
            futures::pin_mut!(events);
            loop {
                let event=tokio::select! {biased;
                    ()=cancellation.cancelled()=>{drop(tenant.take());yield Err(BitrouterError::UpstreamUnavailable);return;},
                    event=events.next()=>event,
                };
                let Some(event)=event else {break};
                match event {
                    Ok(event)=>{
                        let (body,parts)=match decoder.event(&event){Ok(parts)=>parts,Err(error)=>{drop(tenant.take());yield Err(scrubber.scrub_error(error.into()));return;}};
                        if let Err(error)=receipt::native(&mut tenant,Source::GenerateContentChunk,&body)
                            .and_then(|()|receipt::batch(&tenant,&parts,false))
                            .and_then(|()|shared.observe_batch(&parts,false)) {
                            drop(tenant.take());yield Err(error);return;
                        }
                        if let Err(error)=decoder.accepted() {
                            drop(tenant.take());yield Err(scrubber.scrub_error(error.into()));return;
                        }
                        for part in parts {yield Ok(part);}
                    }
                    Err(error)=>{
                        if let Some(hop)=&operational {if cancellation.is_cancelled(){hop.dropped();}else{hop.transport_failed(matches!(error,BitrouterError::UpstreamTimeout));}}
                        drop(tenant.take());yield Err(scrubber.scrub_error(error));return;
                    }
                }
            }
            let parts=match decoder.finish(){Ok(parts)=>parts,Err(error)=>{drop(tenant.take());yield Err(scrubber.scrub_error(error.into()));return;}};
            if let Err(error)=receipt::batch(&tenant,&parts,false).and_then(|()|shared.observe_batch(&parts,false)) {
                drop(tenant.take());yield Err(error);return;
            }
            let Some(part)=parts.into_iter().next() else {drop(tenant.take());yield Err(BitrouterError::UpstreamUnavailable);return;};
            if let Err(error)=receipt::complete(&mut tenant,receipt::successful(&part)) {drop(tenant.take());yield Err(error);return;}
            if let Err(error)=shared.finish_stream().await {drop(tenant.take());yield Err(error);return;}
            drop(tenant.take());yield Ok(part);
        };
        Ok(Box::pin(stream))
    }
}
