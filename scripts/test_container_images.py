#!/usr/bin/env python3
"""Build the real Dockerfiles and smoke-test their target-architecture binaries.

Run on native builders (or configure emulation first). Defaults to both images
and both advertised platforms; CI uses --skip-build after buildx --load.
"""

import argparse
import json
from pathlib import Path
import secrets
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", choices=("docker", "podman"), default="docker")
    parser.add_argument("--image", choices=("collector", "cli"))
    parser.add_argument("--platform", choices=("linux/amd64", "linux/arm64"))
    parser.add_argument("--tag", help="Use with one --image and --platform")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args()
    if (args.tag or args.skip_build) and not (args.image and args.platform):
        parser.error("--tag/--skip-build require --image and --platform")
    root = Path(__file__).resolve().parents[1]

    def run(*command, capture=False):
        return subprocess.run(
            [args.engine, *command], cwd=root, check=True, text=True,
            stdout=subprocess.PIPE if capture else None,
        ).stdout

    for image in [args.image] if args.image else ("collector", "cli"):
        for platform in [args.platform] if args.platform else ("linux/amd64", "linux/arm64"):
            arch = platform.split("/")[1]
            tag = args.tag or f"localhost/bris-{image}:regression-{arch}"
            if not args.skip_build:
                build = ["buildx", "build", "--load"] if args.engine == "docker" else ["build"]
                run(*build, "--platform", platform, "-f", f"crates/bris-{image}/Dockerfile", "-t", tag, ".")
            metadata = json.loads(run("image", "inspect", tag, capture=True))[0]
            if metadata["Architecture"] != arch or metadata["Os"] != "linux":
                raise RuntimeError(f"{tag}: wrong platform in image metadata")
            if image == "cli":
                help_text = run("run", "--rm", "--platform", platform, tag, "replay", "--help", capture=True)
                if "--all-sessions" not in help_text or "--render-frames" not in help_text:
                    raise RuntimeError("CLI image does not provide the nightly replay interface")
            else:
                container = run(
                    "run", "-d", "--platform", platform,
                    "-e", "BRIS_COLLECTOR_DATA_ROOT=/var/lib/bris-collector",
                    "-e", "BRIS_COLLECTOR_BIND=0.0.0.0:8443",
                    "-e", f"BRIS_COLLECTOR_BEARER_TOKEN={secrets.token_hex(32)}",
                    tag, capture=True,
                ).strip()
                try:
                    for attempt in range(30):
                        probe = subprocess.run(
                            [args.engine, "exec", container, "wget", "-qO-", "http://127.0.0.1:8443/healthz"],
                            capture_output=True, text=True,
                        )
                        if probe.returncode == 0:
                            if probe.stdout.strip() != "ok":
                                raise RuntimeError(f"Unexpected health response: {probe.stdout!r}")
                            break
                        if attempt == 29:
                            run("logs", container)
                            raise RuntimeError("Collector failed to become healthy in 30 seconds")
                        time.sleep(1)
                finally:
                    run("rm", "-f", container)
            print(f"PASS bris-{image} {platform}: actual image build and runtime smoke", flush=True)


if __name__ == "__main__":
    main()
