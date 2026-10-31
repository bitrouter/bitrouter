# Inference gateway conformance cases

1. Exercise each enabled inbound endpoint against a compatible test upstream;
   unavailable or incompatible operations fail without claiming execution.
2. Complete streams using the terminal required by their inbound protocol.
   Truncate before that terminal and verify the client does not report success.
3. Inject an error after headers and verify it is not represented as a new HTTP
   status or as a successful protocol end.
4. Preserve supported extension fields and absent/null distinctions in a
   same-protocol round trip. Reject a required incompatible conversion.
5. The SDK does not automatically replay a disconnected request; the absence
   of a response is not treated as evidence of non-execution.
6. In shared mode reproduce the request-fingerprint compatibility vector, then
   verify identical idempotent submissions return the original execution and
   changed semantic headers or body conflict without another dispatch.
