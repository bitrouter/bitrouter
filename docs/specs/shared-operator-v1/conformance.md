# Shared operator v1 conformance cases

1. An inference or legacy-control token cannot authenticate the operator
   listener. Insufficient grants fail before a mutation is applied.
2. Unknown query/body fields, non-JSON or encoded mutation bodies and oversized
   input are rejected. Responses include `Cache-Control: no-store`.
3. An exact actor/operation replay returns the original receipt without applying
   the command twice; changed input with the same identity conflicts.
4. A timed-out mutation remains unknown until a same-actor receipt resolves it;
   an unresolved receipt is not reported as rollback or successful execution.
5. Credential read projections do not disclose imported credential plaintext.
