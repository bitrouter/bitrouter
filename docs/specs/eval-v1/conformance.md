# Eval exchange conformance cases

1. An authenticated producer stores a subject and a result and reads only
   evidence available to its configured owner. A missing subject is not success.
2. An identical result replay is distinguished from changed immutable content.
3. A frozen snapshot remains bound to its original evidence membership after
   new results are submitted.
4. Posting evidence does not publish or mutate the active routing policy.
5. A lost response does not prove that a subject or result was not committed.
