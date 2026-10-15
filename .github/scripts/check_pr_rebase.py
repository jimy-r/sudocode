#!/usr/bin/env python3
"""Require a linear PR rebased on current main and reject conflict markers."""

import subprocess
import sys


def is_ancestor(ancestor, descendant):
    return subprocess.run(["git", "merge-base", "--is-ancestor", ancestor, descendant]).returncode == 0


def main():
    base, head = sys.argv[1:]
    base = subprocess.check_output(
        ["git", "rev-parse", "--verify", base + "^{commit}"], text=True
    ).strip()
    head = subprocess.check_output(
        ["git", "rev-parse", "--verify", head + "^{commit}"], text=True
    ).strip()
    if not is_ancestor(base, head):
        sys.exit("PR is behind main. Fetch origin, rebase on origin/main, resolve conflicts, and push with --force-with-lease.")
    merges = subprocess.check_output(
        ["git", "rev-list", "--parents", "--min-parents=2", f"{base}..{head}"], text=True
    ).strip()
    if merges:
        sys.exit("PR contains merge commits, including integrations of main. Rebase on origin/main and push with --force-with-lease:\n" + merges)
    # Check Git's conflict markers without imposing new whitespace rules on docs.
    result = subprocess.run([
        "git", "-c", "core.whitespace=-blank-at-eol,-blank-at-eof,-space-before-tab",
        "diff", "--check", base, head,
    ])
    if result.returncode:
        sys.exit("Resolve the leftover conflict markers before merging.")
    print("PR is rebased on current main, has no PR-only merge commits, and has no new conflict markers.")


if __name__ == "__main__":
    main()
