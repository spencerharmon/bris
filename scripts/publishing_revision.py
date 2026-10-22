"""Print the first mainline revision containing an explicit implementation SHA.

Image tags use the publishing push SHA, not the implementation branch SHA.
Unlike the current origin/main tip, this revision survives unrelated later pushes.
Requires fetched, non-shallow origin/main history; does not assert CI success.
"""

import re
import subprocess
import sys


def publishing_revision(anchor):
    if not re.fullmatch(r"[0-9a-f]{40}", anchor):
        raise ValueError("supply the full implementation commit SHA")
    revisions = subprocess.check_output(
        ["git", "rev-list", "--first-parent", "origin/main"], text=True
    ).splitlines()
    published = None
    for revision in revisions:
        result = subprocess.run(
            ["git", "merge-base", "--is-ancestor", anchor, revision], check=False
        )
        if result.returncode == 1:
            break
        result.check_returncode()
        published = revision
    if published is None:
        raise ValueError("implementation is not on origin/main; publication is pending")
    return published


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: publishing_revision.py IMPLEMENTATION_SHA")
        print(publishing_revision(sys.argv[1]))
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
