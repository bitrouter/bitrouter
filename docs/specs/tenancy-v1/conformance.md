# Tenancy v1 conformance cases

1. Discover the installation over an authenticated connection. A missing or
   mismatched installation binding on a protected read fails before disclosure.
2. Reject malformed, duplicate-header, duplicate-key and schema-invalid input;
   separately reject a schema-valid command that fails semantic validation.
3. A tenant credential cannot access another tenant; narrowing or revoking its
   current binding removes authority even if a prior request succeeded.
4. A resource-scoped grant cannot activate a whole-tenant configuration.
5. An exact authorized replay returns the existing result; a changed command
   with the same operation identity conflicts. A replay of credential issuance
   never returns its secret.
6. A receipt page with no matches succeeds with an empty page. Correlation
   reuse does not deduplicate an execution.
7. A timed-out write is reported as uncertain until independently resolved;
   an unauthorized lookup does not reveal another principal's operation.
8. When Eval is enabled, duplicate result replay reports duplicate, conflicting
   immutable content fails and all responses carry `Cache-Control: no-store`.
