# Linux PTY and full integration coverage

Current local proof: linux-pty-final62; exact source map/config/logs/results and changed-source tar bind scope. Four Goal PTY cases pass without altered assertions or time bounds; full workspace/strict/doc/dist/feature/MSRV checks pass with two outer slots and no reported LEAK. Only the test launcher and CI selection differ from final36; functional Rust remains identical.

The prior Linux80c0 shard ran32 assertions successfully before its33rd case failed at startup: BRO correctly rejected nonterminal stdin. The remaining179 of212 selected tests were not run because that job failed fast;6 were skipped. Its raw log remains unchanged. No failing hosted filesystem image was exported. POSIX asynchronous list stdin setup occurs before explicit redirection; saveFD3 before backgrounding, then redirectfrom3 and close both spare copies. See the linked Open Group specification in acceptance/code comments.

Cargo metadata inventory confirms all36 integration targets exactlyonce. The newshard buildsone targetatatime, withzero retries, retaining originalfourshards and actualtestconcurrency. Final expanded hostedCI is separate from local Mac proof and from vendor/provider live qualification (unpassed). Older final31 reported one help-output leak; final36 clean results retain their originalsource, not relabeled current.
