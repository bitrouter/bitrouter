# Strong-tier cost-budget optimization progress

## Approved objective

- Use only the 22 fully clean cases from `tb21-v1-full1-20260818T075324Z`.
- Promote only `agent_route/v1|unknown|mechanical|guarded` to strong.
- Target approximately 40%–50% nominal savings while modestly increasing
  strong traffic.
- Keep benchmark parsing external and BitRouter runtime generic.

## Status

- [x] Design approved by the user.
- [x] Design and implementation plan written.
- [x] Skill baseline RED recorded: without the new guidance, the operator
  invented an all-three-controls-in-band requirement and would return no-go
  despite the user-approved cheapest-control anchor; it also had no
  deterministic fail-closed planner contract.
- [x] External planner RED/GREEN complete: 5 focused CLI tests pass after
  missing-script RED and targeted duplicate/missing/error RED.
- [x] Validator-path diagnostic: the planned repository-local
  `skills/quick_validate.py` does not exist; the installed canonical validator
  is `/Users/archer/.codex/skills/.system/skill-creator/scripts/quick_validate.py`.
  The system Python lacks PyYAML, so the verified invocation is
  `uv run --with pyyaml python <validator> <skill>`.
- [x] Private optimization artifact generated and reproduced byte-for-byte:
  22 tasks, 309 joins, 22 promotions, candidate cost $7.105622196,
  strong share 104/309, conservative savings 45.8309%.
- [ ] Generic template RED/GREEN complete.
- [ ] Full verification complete.
- [ ] Pushed.
