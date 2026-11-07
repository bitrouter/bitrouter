//! Explicit private native dispatch on an already bound ordered submission.

use bitrouter_ai::stream::{ServerEvent, StreamItem};
use bitrouter_ai::tenancy::{
    generate_content::{
        self, OrderedGenerateContentCapabilitiesV1, OrderedGenerateContentRequestV1,
    },
    validation::Validate,
};
use futures_util::StreamExt;
use serde_json::Value;

use super::OrderedTenantGateway;
use crate::stream::generate_content::json::UniqueValue;
use crate::{
    gateway::tenancy::{Body, Error, Problem, Reply, failure},
    stream::Protocol,
};

impl OrderedTenantGateway<'_> {
    /// Attach explicitly observed private native protocol support to this call.
    ///
    /// The observation must match the installation, service incarnation and
    /// capability revision already bound to the selected candidate chain.
    /// Provider-native controls retain their separate explicit attachment.
    ///
    /// # Errors
    /// Rejects disabled, invalid or differently bound discovery before I/O.
    pub fn with_generate_content(
        mut self,
        capabilities: &OrderedGenerateContentCapabilitiesV1,
    ) -> Result<Self, Error> {
        capabilities
            .validate()
            .map_err(|_| Error::Request("invalid GenerateContent capabilities"))?;
        let matches = [
            (
                "bitrouter-installation-id",
                capabilities.installation_id.as_str(),
            ),
            ("bitrouter-instance-id", capabilities.instance_id.as_str()),
            (
                "bitrouter-capability-revision",
                capabilities.capability_revision.as_str(),
            ),
        ]
        .iter()
        .all(|(name, expected)| {
            self.inner
                .headers
                .get(*name)
                .is_some_and(|value| value.as_bytes() == expected.as_bytes())
        });
        if !capabilities.enabled || !matches {
            return Err(Error::Request(
                "GenerateContent discovery does not match dispatch",
            ));
        }
        self.generate_content = Some(capabilities.cached_content_enabled);
        Ok(self)
    }

    /// Submit one private native JSON or SSE request without protocol conversion.
    ///
    /// Native payloads retain their JSON values. Stream completion requires a
    /// a finish for every observed candidate followed by clean HTTP EOF. From
    /// the first finish, events share the configured event-byte retention budget
    /// until EOF. Each candidate retains its index and finish; finished
    /// candidates cannot add content. Duplicate JSON members or indices fail.
    /// A response uses either one anonymous candidate or explicit nonnegative
    /// int32 indices throughout; missing indices never imply index zero.
    ///
    /// # Errors
    /// Returns local binding, transport, service or protocol errors without retry.
    pub async fn generate_content_reply(
        self,
        request: &OrderedGenerateContentRequestV1,
    ) -> Result<Reply<Body<Value, Value>>, Error> {
        request
            .validate()
            .map_err(|_| Error::Request("invalid ordered GenerateContent request"))?;
        let Some(cache_enabled) = self.generate_content else {
            return Err(Error::Request(
                "GenerateContent requires explicit capability discovery",
            ));
        };
        if request
            .request
            .get("cachedContent")
            .is_some_and(|value| !value.is_null())
            && (!cache_enabled
                || self
                    .inner
                    .headers
                    .get(bitrouter_ai::tenancy::native_controls::HEADER)
                    .is_none_or(|value| {
                        value.as_bytes()
                            != bitrouter_ai::tenancy::native_controls::PROVIDER_NATIVE_V1.as_bytes()
                    }))
        {
            return Err(Error::Request(
                "native cache requires enabled provider-native controls",
            ));
        }
        if request.model != self.route_id {
            return Err(Error::Request("ordered route differs from request model"));
        }
        if self.generation_defaults_digest.is_some() != self.inner.generation_defaults.is_some() {
            return Err(Error::Request("missing or competing canonical defaults"));
        }
        let response_limit = self.inner.client.options.max_response_bytes;
        let reply: Reply<Body<UniqueValue, UniqueValue>> = self
            .inner
            .relay(
                generate_content::PATH,
                request,
                request.stream,
                Protocol::GenerateContent,
                generate_content::MIME,
            )
            .await?;
        let body = match reply.body {
            Body::Json(UniqueValue(value)) => {
                let mut state = crate::stream::generate_content::State::new(response_limit);
                state.observe(&value).map_err(|error| {
                    crate::gateway::tenancy::response_error(&reply.metadata, error)
                })?;
                if !state.finished() {
                    return Err(failure(&reply.metadata, Problem::Contract));
                }
                Body::Json(value)
            }
            Body::Stream(events) => Body::Stream(Box::pin(events.map(|item| {
                item.map(|item| match item {
                    StreamItem::Event(ServerEvent { event, id, data }) => {
                        StreamItem::Event(ServerEvent {
                            event,
                            id,
                            data: data.0,
                        })
                    }
                    StreamItem::End(end) => StreamItem::End(end),
                })
            }))),
        };
        Ok(Reply {
            metadata: reply.metadata,
            body,
        })
    }
}
