"""Print the first collector publishing run SHA containing an implementation.

Push boundaries come from GitHub Actions, never from Git ancestry alone.
Requires public GitHub run metadata and fetched, non-shallow origin/main history.
Does not assert CI success: the caller must inspect all three images anonymously.
"""

import json
import re
import subprocess
import sys
from urllib.request import Request, urlopen


def publishing_revision(anchor):
    if not re.fullmatch(r"[0-9a-f]{40}", anchor):
        raise ValueError("supply the full implementation commit SHA")
    merged = subprocess.run(
        ["git", "merge-base", "--is-ancestor", anchor, "origin/main"],
        check=False, stderr=subprocess.PIPE, text=True,
    )
    if merged.returncode == 1:
        raise ValueError("implementation is not on origin/main; publication is pending")
    merged.check_returncode()
    origin = subprocess.check_output(
        ["git", "remote", "get-url", "origin"], text=True
    ).strip()
    repository = re.fullmatch(
        r"(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)"
        r"([\w.-]+/[\w.-]+?)(?:\.git)?/?", origin
    )
    if repository is None:
        raise ValueError("origin must identify a public github.com repository")

    run_shas = set()
    page = 1
    while True:
        # Do not filter by success: a failed first publication must not silently
        # resolve to a later build. Unfiltered pagination avoids the API's
        # 1,000-result cap for filtered workflow-run searches.
        url = (f"https://api.github.com/repos/{repository[1]}/actions/workflows/"
               f"collector-image.yml/runs?per_page=100&page={page}")
        request = Request(url, headers={
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "bris-publishing-revision",
        })
        with urlopen(request, timeout=30) as response:
            runs = json.load(response)["workflow_runs"]
        for run in runs:
            if run["event"] == "push" and run["head_branch"] == "main":
                if not re.fullmatch(r"[0-9a-f]{40}", run["head_sha"]):
                    raise ValueError("GitHub returned an invalid workflow run SHA")
                run_shas.add(run["head_sha"])
        if len(runs) < 100:
            break
        page += 1

    revisions = subprocess.check_output(
        ["git", "rev-list", "--first-parent", "origin/main"], text=True
    ).splitlines()
    for revision in reversed(revisions):
        if revision not in run_shas:
            continue
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", anchor, revision], check=False
        )
        if result.returncode == 1:
            continue
        result.check_returncode()
        return revision
    raise ValueError("no publishing push run contains implementation; publication is pending")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: publishing_revision.py IMPLEMENTATION_SHA")
        print(publishing_revision(sys.argv[1]))
    except (ValueError, OSError, KeyError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
