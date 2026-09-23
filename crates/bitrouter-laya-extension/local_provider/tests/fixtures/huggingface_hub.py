import os


def snapshot_download(repo_id, revision, allow_patterns, local_files_only):
    if os.environ.get("FAKE_LAYA_FAIL_START") == "1":
        raise OSError("private startup diagnostic")
    assert repo_id == "convaiinnovations/laya-typed-decisions"
    assert revision == "f9ab0b228f0fc0f14d873dbc99038f135c2da1b2"
    assert allow_patterns
    assert local_files_only
    return "/fake/pinned/laya/checkpoint"
