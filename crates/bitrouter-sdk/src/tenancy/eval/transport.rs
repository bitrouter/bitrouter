use std::io::{self, Write};

use bitrouter_ai::tenancy::{
    operation::{DispatchState, Error as TenancyError},
    validation::Validate,
};
use reqwest::{RequestBuilder, header};
use serde::Serialize;

use super::{TenancyClient, validation::EvalReply};
use crate::error::{Error, Result, ServiceFailure};

const RESPONSE_BYTES: usize = 8 * 1024 * 1024;

impl TenancyClient {
    pub(super) async fn owned_eval_send<T: EvalReply>(
        &self,
        request: RequestBuilder,
        check: impl FnOnce(&T) -> Result<()>,
    ) -> Result<T> {
        // Uses the existing client's pool, deadlines, proxy policy, disabled
        // decompression, and disabled redirects/retries. Eval DTOs deliberately
        // do not acquire the separate management contract's Validate trait.
        let response = request.send().await.map_err(Error::transport)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let failure = |body, body_error| {
            Error::Tenancy(Box::new(ServiceFailure {
                status,
                headers: headers.clone(),
                body,
                body_error,
            }))
        };
        if headers.get_all(header::CONTENT_TYPE).iter().count() != 1
            || !crate::transport::has_content_type(&response, "application/json")
            || headers
                .get_all(header::CONTENT_ENCODING)
                .iter()
                .any(|value| value != "identity")
        {
            return Err(failure(
                None,
                Some(Error::Response(
                    "evaluation response must be unencoded JSON",
                )),
            ));
        }
        let bytes = crate::transport::body(
            response,
            self.options.max_response_bytes.min(RESPONSE_BYTES),
        )
        .await
        .map_err(|error| failure(None, Some(error)))?;
        if !(200..300).contains(&status) {
            return match decode_error(&bytes) {
                Ok(body) => Err(failure(Some(body), None)),
                Err(error) => Err(failure(None, Some(error))),
            };
        }
        // Direct typed decoding keeps duplicate known fields visible to serde.
        // Evidence validation, digests and evaluator admission remain server-owned.
        let output: T = serde_json::from_slice(&bytes).map_err(|_| {
            failure(
                None,
                Some(Error::Response("invalid evaluation response shape")),
            )
        })?;
        output
            .validate_reply()
            .map_err(|error| failure(None, Some(error)))?;
        check(&output).map_err(|error| failure(None, Some(error)))?;
        Ok(output)
    }
}

fn decode_error(bytes: &[u8]) -> Result<TenancyError> {
    let error: TenancyError = serde_json::from_slice(bytes)
        .map_err(|_| Error::Response("invalid owned evaluation error shape"))?;
    error
        .validate()
        .map_err(|_| Error::Response("invalid owned evaluation error invariants"))?;
    if error.operation_id.is_some()
        || error.execution_id.is_some()
        || error.dispatch_state != DispatchState::NotStarted
    {
        return Err(Error::Response("invalid owned evaluation error identity"));
    }
    // NotStarted describes provider work only. In particular, a fresh authority
    // denial can follow a committed ledger mutation. Never use the management
    // transport's definite_rejection classifier for owned evaluation writes.
    Ok(error)
}

pub(super) fn encode(input: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        overflowed: bool,
    }
    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.bytes.len().saturating_add(bytes.len()) > self.limit {
                self.overflowed = true;
                return Err(io::Error::other("evaluation request exceeds byte limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut writer = Bounded {
        bytes: Vec::new(),
        limit,
        overflowed: false,
    };
    if serde_json::to_writer(&mut writer, input).is_err() {
        return Err(Error::Request(if writer.overflowed {
            "evaluation request exceeds byte limit"
        } else {
            "invalid evaluation request encoding"
        }));
    }
    Ok(writer.bytes)
}
