#!/usr/bin/env python3
"""Build the real Dockerfiles and smoke-test their target-architecture runtimes.

Run on native builders (or configure emulation first). Defaults to both images
and both advertised platforms; CI uses --skip-build after buildx --load.
Select --image explorer for static-server routing; pytest runs its Podman/amd64 check.
"""

import argparse
import base64
import http.client
import json
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
from urllib.parse import urljoin, urlsplit


def explorer_smoke(run, platform, tag, root):
    with tempfile.TemporaryDirectory(prefix="bris-explorer-") as tmp:
        corpus = Path(tmp)
        corpus.chmod(0o755)
        fixtures = {
            "index.json": b'{"schema_version":1,"sessions":[]}',
            "sessions/example/bris-replay-report.json": b'{"captures":[]}',
            "sessions/example/captures/sample/frames/00000000-render.png": base64.b64decode(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a0u8AAAAASUVORK5CYII="
            ),
        }
        for path, content in fixtures.items():
            target = corpus / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)
            target.chmod(0o644)
        container = run(
            "run", "-d", "--platform", platform,
            "-v", f"{corpus}:/srv/corpus:ro,Z",
            "-p", "127.0.0.1::8080", tag, capture=True,
        ).strip()
        try:
            port = int(run("port", container, "8080/tcp", capture=True).strip().rsplit(":", 1)[1])

            def get(path):
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
                try:
                    connection.request("GET", path, headers={"Host": "explorer.example.com"})
                    response = connection.getresponse()
                    return response.status, response.getheader("Location"), response.read()
                finally:
                    connection.close()

            for attempt in range(30):
                try:
                    healthy = get("/healthz") == (200, None, b"ok\n")
                except (OSError, http.client.HTTPException):
                    healthy = False
                if healthy:
                    break
                if attempt == 29:
                    run("logs", container)
                    raise RuntimeError("Explorer failed to become healthy in 30 seconds")
                time.sleep(1)

            # HTTPConnection does not follow redirects: inspect the raw header,
            # then resolve it as a browser behind an HTTPS ingress would.
            status, location, _ = get("/")
            if status != 302 or location != "/tools/corpus-explorer/index.html":
                raise RuntimeError(f"Expected relative root redirect, got {status} Location: {location!r}")
            resolved = urljoin("https://explorer.example.com/", location)
            if resolved != "https://explorer.example.com/tools/corpus-explorer/index.html":
                raise RuntimeError(f"Redirect changed external origin: {resolved}")
            status, _, body = get(urlsplit(resolved).path)
            if status != 200 or b"<title>Bris corpus explorer</title>" not in body:
                raise RuntimeError("Root redirect did not resolve to the explorer document")
            for asset in ("explorer.js", "explorer.css", "data/ne_110m_coastline.min.json"):
                expected = (root / "tools/corpus-explorer" / asset).read_bytes()
                if get(f"/tools/corpus-explorer/{asset}") != (200, None, expected):
                    raise RuntimeError(f"Baked asset is missing or shadowed: {asset}")
            for path, expected in fixtures.items():
                if get(f"/{path}") != (200, None, expected):
                    raise RuntimeError(f"Mounted corpus route failed: {path}")
            for path in ("/sessions/example/missing.json", "/sessions/example/"):
                if get(path)[0] != 404:
                    raise RuntimeError(f"Missing corpus file/directory should return 404: {path}")
            print("PASS explorer: relative Location, HTTPS origin, document/title, assets, health, mounted corpus, 404s", flush=True)
        finally:
            run("rm", "-f", container)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", choices=("docker", "podman"), default="docker")
    parser.add_argument("--image", choices=("collector", "cli", "explorer"))
    parser.add_argument("--platform", choices=("linux/amd64", "linux/arm64"))
    parser.add_argument("--tag", help="Use with one --image and --platform")
    parser.add_argument("--skip-build", action="store_true")
    args = parser.parse_args(argv)
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
                if args.engine == "podman" and image == "explorer":
                    build += ["--format", "docker"]  # Preserve the Dockerfile HEALTHCHECK.
                dockerfile = ("tools/corpus-explorer/deploy/Dockerfile" if image == "explorer"
                              else f"crates/bris-{image}/Dockerfile")
                run(*build, "--platform", platform, "-f", dockerfile, "-t", tag, ".")
            metadata = json.loads(run("image", "inspect", tag, capture=True))[0]
            if metadata["Architecture"] != arch or metadata["Os"] != "linux":
                raise RuntimeError(f"{tag}: wrong platform in image metadata")
            if image == "explorer":
                explorer_smoke(run, platform, tag, root)
            elif image == "cli":
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


def test_explorer_image():
    """Pytest entry point for the native Podman image integration check."""
    main(["--engine", "podman", "--image", "explorer", "--platform", "linux/amd64"])


if __name__ == "__main__":
    main()
