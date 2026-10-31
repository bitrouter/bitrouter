# Observation schema v2 conformance cases

1. The emitted instrumentation scope and normally completed span attributes
   agree with the committed schema's names, types and conditions.
2. A conditional attribute absent because its condition was not met is not
   synthesized as zero or as a successful operation.
3. A prematurely abandoned/reaped span can lack later lifecycle attributes;
   a consumer does not treat that as a complete successful operation.
4. Deployment extensions respect reserved attribute prefixes and declared
   extension value types. Unknown optional attributes do not break a reader.
