# retro-specter

Walks back through merged PRs in a git repository and measures the
**indentation-based complexity** each one added or removed. Supports C# and Python.

```
retro-specter complexity [REV] -C <repo> [-n N] [--since DATE] [--files | --functions] [-f table|json|jsonl|csv]
```

## How it works

- Walks the first-parent history of `REV` (default `HEAD`). A commit counts as a PR if it is a
  merge commit, or if its message has a PR reference: GitHub `Merge pull request #N` / `(#N)`,
  Azure DevOps `Merged PR N`, or GitLab `See merge request !N`. `--mode merges|all` changes this.
- Diffs each PR against its first parent, which gives the PR's net change.
- Scores each logical **statement** by its nesting level. Blank lines, comments, docstrings,
  `#` preprocessor lines and brace-only lines are ignored. The indent unit (2, 4, tabs…) is
  detected per file.
- Formatter-neutral, so running CSharpier or Black doesn't show up as complexity:
  - continuation lines (wrapped arguments, method chains, initializers, lines after a
    trailing `=`/`&&`/`\`, anything inside open brackets) are folded into their statement;
  - one-line bodies (`if (x) y;`, `if x: y`) count the same as their two-line form;
  - in C#, a `{ }` block nests one level below the statement that owns it, so brace style,
    case-label indentation and lambda bodies inside wrapped argument lists don't matter;
  - `+CPLX`/`-CPLX` pair up removed and added statements that are equal apart from
    whitespace and brace placement, and only count what's left. Re-indenting code under a
    new `if` changes its level, so it still counts.
- Finds function and class declarations (Python `def`/`class`; C# methods, constructors,
  local functions, and `class`/`struct`/`record`/`interface`/`enum`). A declaration owns
  the more-indented lines that follow it. Function complexity is measured relative to the
  function body, so a flat method scores 0 however deeply its namespace and class nest it.
  Functions are matched between the two versions by qualified name (`Class.Method`), so each
  PR can report which functions were added, removed or modified.
- File contents are streamed from a persistent `git cat-file --batch` process per worker thread,
  and PRs are analyzed in parallel.

## Columns

| column | meaning |
|---|---|
| `+LINES` | logical lines added |
| `+CPLX` / `-CPLX` | sum of indentation levels of added / removed lines |
| `ΔCPLX` | whole-file complexity after minus before, over all touched files |
| `MAXD` | deepest nesting level among added lines |
| `FN +/-/~` | functions added / removed / modified |
| `ΔCLS` | change in class count |

`--functions` lists every changed function with its complexity before → after, shown as
`total (dMAX_DEPTH, NL)`. With `-f csv` it prints one row per changed function.

The table output ends with a summary: top PRs by complexity added, hotspot files, and the
functions whose complexity grew the most.

Generated files (`*.Designer.cs`, `*.g.cs`, `*_pb2.py`, …) are excluded by default. Use
`--exclude GLOB` to add more, or `--no-default-excludes` to turn this off.

## ai subcommand

Aggregates AI usage from commit messages. It reads messages and `git diff --numstat` only: no
complexity analysis, any language, PRs with no code included.

```
retro-specter ai [REV] [-C <repo>]... [--scan <dir>]... [--fetch] [--by pr|commit] [--list] [-f table|json|jsonl|csv]
```

- Models come from `Co-Authored-By:` trailers (Claude, Copilot, Gemini, GPT, Cursor, Aider, …);
  tools from `Generated with …` lines.
- `--by pr` (default): one row per merged PR. It combines the trailers of every commit in the PR
  and lists the models in one field, most commits first (`Claude Sonnet 5 (3), Claude Opus 5.5 (2)`).
  Commits on the branch that aren't in any PR are reported separately as **direct commits**.
  `--by commit`: one row per non-merge commit.
- Reports rows, commits, lines added/removed per model / tool, with vs. without AI credit, per repo.
- `--months N` / `--days N` limit the window (shorthand for `--since "N months ago"`); both
  subcommands accept them.
- `--scan DIR` finds git checkouts recursively (`--scan-depth`, default 6); `--fetch` runs
  `git fetch` first and analyzes the branch's upstream. Both subcommands accept them.
  Linked worktrees of a repository already in the set are skipped (the main checkout is kept).
