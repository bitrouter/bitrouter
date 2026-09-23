import os


class FakeAgent:
    def system_one(self, state, questions):
        if os.environ.get("FAKE_LAYA_FAIL_INFER") == "1":
            raise RuntimeError("private inference diagnostic")
        assert state == {"ticket": "synthetic"}
        assert set(questions) == {"approve"}
        return {
            "model": "laya-rl-agent",
            "answers": {
                "approve": {
                    "type": "noul",
                    "noul": 0.75,
                    "confidence": 0.75,
                    "action": {"act_probability": 1.0},
                }
            },
            "usage": {"input_tokens": 12, "output_tokens": 0},
        }


def load(snapshot):
    assert snapshot == "/fake/pinned/laya/checkpoint"
    return FakeAgent()
