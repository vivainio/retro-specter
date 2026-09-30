# retro-specter

Walks back through merged PRs in a git repository and measures the
**indentation-based complexity** each one added or removed. Supports C# and Python.

```
retro-specter complexity [REV] [-C <repo>]... [--scan <dir>]... [--months N | --days N] [--files | --functions] [-f table|json|jsonl|csv]
```

`-j N` (before the subcommand) sets the worker threads. Build with `cargo build --release`
(binary in `target/release/retro-specter`) or install with `cargo install --path .`.

With several repositories (`-C` repeated, or `--scan DIR` to find checkouts recursively), the
table gets a REPO column and hotspots are prefixed with `repo:`; the summary covers all of them.
Linked git worktrees of a repository already in the set are skipped.

## How it works

- Walks the first-parent history of `REV` (default `HEAD`). A commit counts as a PR if it is a
  merge commit, or if its message has a PR reference: GitHub `Merge pull request #N` / `(#N)`,
  Azure DevOps `Merged PR N`, or GitLab `See merge request !N`. `--mode merges|all` changes this.
  `git pull` merges (`Merge branch 'master' of <url>`, or merging the branch's own upstream back
  in) are not PRs; `dump` uses the same rule.
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

## dump and ai subcommands

`dump` writes a raw record of a repository's PRs and commits as JSONL; `ai` aggregates AI usage
from such dumps. Dumping is the only step that touches git, so you dump once and analyze many
times, or write your own scripts against the dump.

```
retro-specter dump [REV] [-C <repo>]... [--scan <dir>]... [--fetch] [--months N | --days N] -o <dir>
retro-specter ai [FILE|DIR]... [--by pr|commit] [--list] [-f table|json|jsonl|csv]
```

**Dump format** (one file per repo, `<dir>/<repo>.jsonl`; a single repo without `-o` goes to stdout).
No commit text is stored: no subjects, no messages. Records keep only what was derived from a
message at dump time (PR number, merged branch name, AI credits), so changing the AI vendor list
means re-dumping.

- `{"type":"pr", repo, sha, pr, branch, author, author_email, date, ai_models, ai_tools, added, removed, commits, squash}`
  a merged PR (merge commit, or squash/numbered commit). `added`/`removed` are its net change
  against the target branch. `ai_*` are the credits in the merge commit's own message.
- `{"type":"commit", repo, sha, pr_merge, pr, branch, author, author_email, date, ai_models, ai_tools, added, removed}`
  a non-merge commit. `pr_merge` is the `sha` of the PR record that brought it in (`null` for
  a direct commit), so commits group into PRs by `(repo, pr_merge)`. A squash-merged PR has
  both records for the same sha; rebase-merged commits look like direct commits. `ai_models`
  come from `Co-Authored-By:` trailers and `ai_tools` from `Generated with …` lines.
- `branch` is the merged branch's name taken from the merge subject. For merges without a PR
  number (`pr: null`) it is the pseudo-PR label (`Merge branch 'x'`, `Merge x into main`, ...).
  It is the one piece of free-form text left in a dump, and a branch name can contain a name.
- A `git pull` merge (`Merge branch 'master' of <url>`, or merging the trunk into itself) is not
  a PR: it emits no `pr` record, and the commits it brought in are dumped as direct commits.
- **Authors** (`--authors pseudonym|real`, default `pseudonym`): authors are mnemonic pseudonyms
  like `amber-otter` and `author_email` is dropped. Names are handed out in order of first
  appearance and shared by all repositories dumped in the same run, so a person is the same name
  in every file of that run. Nothing is stored between runs: the names are shuffled by a per-run
  seed, so a later dump gives different names (a coincidental repeat for a different person is
  possible but rare, so don't mix dumps from different runs when counting people).
  `--authors real` keeps names and emails (with the repo's `.mailmap` applied).

**ai**: models come from `Co-Authored-By:` trailers (Claude, Copilot, Gemini, GPT, Cursor, Aider, …),
tools from `Generated with …` lines. Reads files or directories of `*.jsonl` (default: stdin).

- `--by pr` (default): one row per PR, models listed most commits first
  (`Claude Sonnet 5 (3), Claude Opus 5.5 (2)`); commits outside any PR are reported separately as
  direct commits. `--by commit`: one row per commit.
- Line counts for a PR are its net change, so work a later commit reverts isn't counted twice.
  Git can't say which surviving lines came from which commit, so AI lines for a PR mixing AI and
  other commits are the net lines scaled by the AI commits' share of commit churn (an estimate;
  exact when all or none of the commits credit AI). CSV/JSON also carry the raw per-commit churn.

`--months N` / `--days N` (dump time) limit the window (`--since`/`--until` also work); `--scan DIR` finds git checkouts
recursively; `--fetch` runs `git fetch` first and dumps the branch's upstream. Linked worktrees of
a repository already in the set are skipped (the main checkout is kept). `complexity` accepts the
same repo options. `--mode` applies to `dump` too, and `-n` there counts PRs and direct commits together.
