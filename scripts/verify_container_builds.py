#!/usr/bin/env python3
"""Check public native image CI for the exact checkout tree, including merge trees.

No local container engine or GitHub credentials are required. Each workflow must
have successfully built, run and published both architectures. A commit with the
same tree is accepted so a no-content-change merge can reuse its branch's tests.
"""

import argparse
import json
import re
import subprocess
from urllib.request import Request, urlopen


def api(path):
    request = Request(f"https://api.github.com/repos/{path}", headers={
        "Accept": "application/vnd.github+json",
        "User-Agent": "bris-container-build-check",
    })
    with urlopen(request, timeout=30) as response:
        return json.load(response)


def verify(repository, tree):
    for workflow in ("collector-image", "bris-cli-image"):
        runs = api(f"{repository}/actions/workflows/{workflow}.yml/runs?per_page=100")
        for run in runs["workflow_runs"]:
            if run["status"] != "completed" or run["conclusion"] != "success":
                continue
            commit = api(f"{repository}/git/commits/{run['head_sha']}")
            if commit["tree"]["sha"] != tree:
                continue
            jobs = api(f"{repository}/actions/runs/{run['id']}/jobs?per_page=100")["jobs"]
            required = {
                "build-native (amd64, ubuntu-latest)": (
                    "Build and load native image", "Run native container regressions", "Push tested native image"),
                "build-native (arm64, ubuntu-24.04-arm)": (
                    "Build and load native image", "Run native container regressions", "Push tested native image"),
                "build-and-push": (
                    "Publish multiarch manifest", "Verify the pushed image is pullable by digest"),
            }
            for name, steps in required.items():
                job = next((job for job in jobs if job["name"] == name), None)
                if job is None or job["conclusion"] != "success":
                    raise RuntimeError(f"{run['html_url']}: missing successful job {name}")
                passed = {step["name"] for step in job["steps"] if step["conclusion"] == "success"}
                if not set(steps).issubset(passed):
                    raise RuntimeError(f"{run['html_url']}: missing build/runtime/publication steps in {name}")
            print(f"PASS {workflow}: tree={tree} {run['html_url']}", flush=True)
            break
        else:
            raise RuntimeError(f"{workflow}: no successful native build/runtime publication for tree {tree}; "
                               "dispatch the image workflow on the branch and rerun after completion")


def test_published_images():
    """Pytest integration entrypoint; require real CI for this checkout."""
    origin = subprocess.check_output(["git", "remote", "get-url", "origin"], text=True).strip()
    repository = re.fullmatch(
        r"(?:https://github\.com/|git@github\.com:|ssh://git@github\.com/)"
        r"([\w.-]+/[\w.-]+?)(?:\.git)?/?", origin,
    )
    assert repository is not None, "origin must be a public GitHub repository"
    assert not subprocess.check_output(["git", "status", "--porcelain"], text=True).strip(), "commit changes first"
    tree = subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], text=True).strip()
    verify(repository[1], tree)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("repository", help="Public GitHub owner/repository")
    args = parser.parse_args()
    if not re.fullmatch(r"[\w.-]+/[\w.-]+", args.repository):
        parser.error("repository must be owner/repository")
    if subprocess.check_output(["git", "status", "--porcelain"], text=True).strip():
        parser.error("commit the checkout changes before verifying its CI tree")
    tree = subprocess.check_output(["git", "rev-parse", "HEAD^{tree}"], text=True).strip()
    verify(args.repository, tree)
