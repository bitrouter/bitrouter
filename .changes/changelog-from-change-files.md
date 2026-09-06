---
type: changed
title: "The changelog is written from per-PR change files, not commit subjects"
---

Contributor-facing. User-visible changes are now described in a
`.changes/<slug>.md` file on the branch that makes them, and CI requires one on
every pull request that is not labelled `no-changelog`. See
[`.changes/README.md`](.changes/README.md) for the format.

release-plz still owns versioning, tagging, and publishing, and still derives
the version bump from conventional commits. What changed is what the changelog
*says*: curated prose first, with the generated commit list kept below it in a
collapsed block. The release PR folds pending change files in and deletes them.
