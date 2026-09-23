# retro-specter

Walks back through merged PRs in a git repository and measures the
**indentation-based complexity** each one added or removed. Supports C# and Python.

```
retro-specter [REV] -C <repo> [-n N] [--since DATE] [--files] [-f table|json|jsonl|csv]
```

## How it works

- Walks the first-parent history of `REV` (default `HEAD`). A commit counts as a PR if it is a
  merge commit, or if its message has a PR reference: GitHub `Merge pull request #N` / `(#N)`,
  Azure DevOps `Merged PR N`, or GitLab `See merge request !N`. `--mode merges|all` changes this.
- Diffs each PR against its first parent, which gives the PR's net change.
- Scores each logical line by its indentation level. Blank lines, comments, docstrings,
  `#` preprocessor lines and brace-only lines are ignored. The indent unit (2, 4, tabs…)
  is detected per file.
- File contents are streamed from a persistent `git cat-file --batch` process per worker thread,
  and PRs are analyzed in parallel.

## Columns

| column | meaning |
|---|---|
| `+LINES` | logical lines added |
| `+CPLX` / `-CPLX` | sum of indentation levels of added / removed lines |
| `ΔCPLX` | whole-file complexity after minus before, over all touched files |
| `MAXD` | deepest nesting level among added lines |

The table output ends with a summary: top PRs by complexity added, and hotspot files.

Generated files (`*.Designer.cs`, `*.g.cs`, `*_pb2.py`, …) are excluded by default. Use
`--exclude GLOB` to add more, or `--no-default-excludes` to turn this off.
