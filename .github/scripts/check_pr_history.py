#!/usr/bin/env python3
"""Reject merges that change files no commit in the range edits, and leftover
conflict markers.

`check_pr_history.py <base> <head>` checks the changes `head` brings on top of
`merge-base(base, head)`. For a pull request, base is `origin/main`; in a merge
queue, base is the group's base commit. A PR may be behind main — the merge
queue integrates it — so only what the PR itself changes is checked.

Every file that differs must be edited by some non-merge commit in the range.
A file that only a merge commit changed was rewritten while resolving a merge
— historically, agents' merge-from-main resolutions silently reverted other
PRs' lines this way. Adapting to main is fine, but not silently: make the edit
in its own commit, or declare it with a `Merge-adapts: <path>` trailer on any
commit in the range (an empty commit works for a merge already pushed).
"""

import subprocess
import sys


def git(*args):
    return subprocess.check_output(["git", *args], text=True)


def commit(ref):
    return git("rev-parse", "--verify", "--end-of-options", ref + "^{commit}").strip()


def main():
    base, head = (commit(ref) for ref in sys.argv[1:])
    base = git("merge-base", base, head).strip()
    changed = set(git("diff", "--name-only", "--no-renames", base, head).split("\n")) - {""}
    authored = set(
        git("log", "--no-merges", "--format=", "--name-only", "--no-renames", f"{base}..{head}").split("\n")
    ) - {""}
    declared = set(
        git("log", "--format=%(trailers:key=Merge-adapts,valueonly,separator=%x0a)", f"{base}..{head}").split("\n")
    ) - {""}
    unexplained = sorted(changed - authored - {path.strip() for path in declared})
    if unexplained:
        sys.exit(
            "A merge commit changed files that no commit in this range edits:\n"
            + "".join(f"  {path}\n" for path in unexplained)
            + "A merge resolution rewrote them, which is how other PRs' work gets silently "
            "reverted. If that was unintended, restore them to the merged result. If it was "
            "an intended adaptation, make it visible: put it in its own commit, or declare it "
            "on any commit with a trailer, e.g.\n"
            "  git commit --allow-empty -m 'Declare merge adaptation' -m 'Merge-adapts: <path>'"
        )
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print(f"{len(changed)} changed files, each edited by a commit in the range; no conflict markers.")


if __name__ == "__main__":
    main()
