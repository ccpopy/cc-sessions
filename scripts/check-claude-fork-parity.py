"""Compare real Web UI fork output with the pinned official SDK, using synthetic data.

Build dist/ and the CLI first. Pass the unmodified session_mutations.py downloaded
from the commit linked in docs/claude-session-copy.md. No Claude/model call is made.
"""

from __future__ import annotations

import argparse
import ast
import hashlib
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import re
import socket
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from urllib.error import URLError
from urllib.request import ProxyHandler, build_opener
import uuid


SDK_SHA256 = "16653fb5dbb6e9963e05ef1522e43ba4b8e28fd0b037e4749dde865ca9e4e675"
SOURCE_ID = "10000000-0000-4000-8000-000000000001"
REPO = Path(__file__).resolve().parent.parent


def load_sdk(path: Path) -> dict:
    content = path.read_bytes()
    assert hashlib.sha256(content).hexdigest() == SDK_SHA256, "SDK source differs from the pinned commit"
    tree = ast.parse(content)
    names = {"_build_fork_lines", "_parse_fork_transcript"}
    selected = [node for node in tree.body if
                (isinstance(node, ast.FunctionDef) and node.name in names)
                or (isinstance(node, ast.Assign) and any(isinstance(target, ast.Name)
                    and target.id == "_TRANSCRIPT_TYPES" for target in node.targets))]
    assert len(selected) == len(names) + 1
    scope = {
        "json": json, "uuid_mod": uuid, "datetime": datetime, "timezone": timezone,
    }
    # Execute only the two pure transform functions, not SDK imports or I/O.
    exec(compile(ast.Module(body=selected, type_ignores=[]), str(path), "exec"), scope)
    return scope


def normalize(entries: list[dict], session_id: str, source: list[dict], started: float) -> list[dict]:
    original = {e["uuid"]: e for e in source if "uuid" in e}
    assert uuid.UUID(session_id).version == 4 and session_id != SOURCE_ID
    mapping = {}
    messages = [e for e in entries if "forkedFrom" in e]
    assert messages
    for i, entry in enumerate(entries):
        assert entry["sessionId"] == session_id
        value = entry["uuid"]
        assert uuid.UUID(value).version == 4 and value not in original and value not in mapping
        mapping[value] = entry.get("forkedFrom", {}).get("messageUuid", f"metadata:{entry['type']}:{i}")
    normalized = []
    for entry in entries:
        item = dict(entry)
        item["sessionId"] = "<new-session>"
        item["uuid"] = mapping[entry["uuid"]]
        for key in ("parentUuid", "logicalParentUuid"):
            if entry.get(key) is not None:
                # A dangling pointer must fail, not be erased by normalization.
                item[key] = mapping[entry[key]]
        original_id = entry.get("forkedFrom", {}).get("messageUuid")
        if original_id is None or entry is messages[-1] or "timestamp" not in original[original_id]:
            stamp = datetime.fromisoformat(entry["timestamp"].replace("Z", "+00:00")).timestamp()
            assert started - 1 <= stamp <= time.time() + 1
            item["timestamp"] = "<now>"
        else:
            assert entry["timestamp"] == original[original_id]["timestamp"]
        normalized.append(item)
    return normalized


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--sdk-source", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=REPO / "src-tauri/target/debug" / ("cc-sessions.exe" if os.name == "nt" else "cc-sessions"))
    args = parser.parse_args()
    sdk = load_sdk(args.sdk_source)
    fixture = (REPO / "src-tauri/tests/fixtures/claude-fork.jsonl").read_bytes()
    original = [json.loads(line) for line in fixture.splitlines()]
    started = time.time()
    # All requests target our own loopback server, independently of system proxies.
    urlopen = build_opener(ProxyHandler({})).open
    with tempfile.TemporaryDirectory(prefix="cc-sessions-sdk-parity-") as temp:
        root = Path(temp)
        project = root / "claude/projects/project"
        project.mkdir(parents=True)
        source = project / f"{SOURCE_ID}.jsonl"
        source.write_bytes(fixture)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        environment = dict(os.environ, CC_SESSIONS_WEBUI_SETTINGS=str(root / "settings.json"), CC_SESSIONS_WEBUI_DIST=str(REPO / "dist"))
        command = [str(args.binary.resolve()), "--provider", "claude"]
        for provider in ("claude", "codex", "opencode", "cursor"):
            (root / provider).mkdir(exist_ok=True)
            command.extend([f"--{provider}-dir", str(root / provider)])
        command.extend(["webui", "--host", "127.0.0.1", "--port", str(port)])
        with (root / "server.log").open("w") as log:
            process = subprocess.Popen(command, cwd=REPO, env=environment, stdout=log, stderr=log,
                                       creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
            try:
                url = f"http://127.0.0.1:{port}"
                deadline = time.monotonic() + 15
                while True:
                    try:
                        with urlopen(url, timeout=1) as response:
                            html = response.read().decode()
                        break
                    except URLError:
                        if process.poll() is not None or time.monotonic() > deadline:
                            raise RuntimeError("Test Web UI did not start")
                        time.sleep(0.1)
                runtime = json.loads(re.search(r"window\.__CC_SESSIONS_WEBUI__ = (.*?);</script>", html).group(1))

                connection = HTTPConnection("127.0.0.1", port, timeout=10)

                def invoke(name: str, values: dict, *, expect_error: bool = False) -> dict:
                    connection.request("POST", f"/api/invoke/{name}", body=json.dumps(values).encode(), headers={
                        "Content-Type": "application/json", "X-CC-Sessions-Webui-Token": runtime["apiToken"],
                    })
                    response = connection.getresponse()
                    body = json.loads(response.read())
                    assert (response.status >= 400) == expect_error, f"Unexpected API status for {name}: {response.status} {body}"
                    return body

                reports = []
                base = {"claudeDir": str(root / "claude"), "sessionId": SOURCE_ID, "rolloutPath": str(source)}
                for index in (None, 1, 3, 5, 9):
                    name = "duplicate_claude_session" if index is None else "fork_claude_session_at_event"
                    values = dict(base)
                    if index is not None:
                        values.update(eventIndex=index, messageUuid=original[index]["uuid"])
                    report = invoke(name, values)
                    output = Path(report["new_rollout_path"])
                    assert output.parent == source.parent and output.stem == report["new_id"]
                    actual = [json.loads(line) for line in output.read_bytes().splitlines()]
                    transcript, replacements = sdk["_parse_fork_transcript"](fixture, SOURCE_ID)
                    sdk_id, lines = sdk["_build_fork_lines"](transcript, replacements, SOURCE_ID,
                        None if index is None else original[index]["uuid"], None, lambda: "Copy fixture")
                    expected = [json.loads(line) for line in lines]
                    assert normalize(actual, report["new_id"], original, started) == normalize(expected, sdk_id, original, started)
                    assert report["total_lines" if index is None else "included_lines"] == len(actual)
                    reports.append({"cutoff_line_index": index, "output_lines": len(actual), "sdk_match": True})
                before = set(project.iterdir())
                failure = invoke("fork_claude_session_at_event", dict(base, eventIndex=9, messageUuid=original[1]["uuid"]), expect_error=True)
                assert "所选 Claude 消息已变化" in failure["error"]
                assert set(project.iterdir()) == before and source.read_bytes() == fixture
                connection.close()
                print(json.dumps({"sdk_commit": "f101a76aed20655fde8e2c67cd1002bd7e700c3a", "cases": reports,
                                  "source_unchanged": True, "stale_selection_rejected": True}, indent=2))
            except Exception:
                log.flush()
                print((root / "server.log").read_text(encoding="utf-8", errors="replace"), file=sys.stderr)
                raise
            finally:
                process.terminate()
                process.wait(timeout=10)


if __name__ == "__main__":
    main()
