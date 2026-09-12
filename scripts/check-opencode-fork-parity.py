"""Compare Web UI copies with OpenCode 1.18.30's real fork API using synthetic data.

Build dist/ and the CC Sessions CLI first. Pass an official OpenCode 1.18.30 binary.
Both servers use isolated directories; no provider/model request is made.
"""
from __future__ import annotations

import argparse
import base64
from contextlib import closing, contextmanager
from copy import deepcopy
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import re
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from urllib.error import URLError
from urllib.parse import urlsplit

REPO = Path(__file__).resolve().parent.parent
FLAGS = subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0


def request(url, data=None, headers=None):
    body = None if data is None else json.dumps(data).encode()
    parsed = urlsplit(url)
    connection = HTTPConnection(parsed.hostname, parsed.port, timeout=15)
    try:
        connection.request("GET" if data is None else "POST", parsed.path or "/", body,
                           {"Content-Type": "application/json", **(headers or {})})
        response = connection.getresponse()
        result = response.read().decode()
        if response.status >= 400:
            raise RuntimeError(f"HTTP {response.status} {url}: {result}")
        return result
    finally:
        connection.close()


def port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@contextmanager
def server(command, root, environment, endpoint, log_name):
    with (root / log_name).open("w", encoding="utf-8") as log:
        process = subprocess.Popen(command, cwd=root / "project", env=environment,
                                   stdout=log, stderr=log, creationflags=FLAGS)
        try:
            deadline = time.monotonic() + 45
            while True:
                try:
                    request(endpoint)
                    break
                except (URLError, OSError):
                    if process.poll() is not None or time.monotonic() > deadline:
                        raise RuntimeError((root / log_name).read_text(encoding="utf-8", errors="replace"))
                    time.sleep(0.1)
            yield
        except BaseException:
            log.flush()
            print((root / log_name).read_text(encoding="utf-8", errors="replace")[-3000:], file=sys.stderr)
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)


def locator(db, session):
    encoded = base64.urlsafe_b64encode(json.dumps({"db": str(db), "session": session}).encode()).decode().rstrip("=")
    return "opencode:" + encoded


def seed(db, session, messages):
    with closing(sqlite3.connect(db)) as conn:
        for message in messages:
            data = message["data"]
            conn.execute("INSERT INTO message (id,session_id,time_created,time_updated,data) VALUES (?,?,?,?,?)",
                         (message["id"], session, data["time"]["created"], data["time"]["created"], json.dumps(data)))
            for part in message["parts"]:
                conn.execute("INSERT INTO part (id,message_id,session_id,time_created,time_updated,data) VALUES (?,?,?,?,?,?)",
                             (part["id"], message["id"], session, part["created"], part["created"], json.dumps(part["data"])))
        conn.execute("UPDATE session SET metadata=?, cost=999, tokens_input=999, time_archived=7, share_url=?, parent_id=? WHERE id=?",
                     (json.dumps({"nested": {"keep": True}}), "https://example.invalid/shared", "ses_parent", session))
        conn.commit()


def stored(db, session):
    with closing(sqlite3.connect(db)) as conn:
        conn.row_factory = sqlite3.Row
        info = dict(conn.execute("SELECT * FROM session WHERE id=?", (session,)).fetchone())
        messages = []
        for message in conn.execute("SELECT * FROM message WHERE session_id=? ORDER BY time_created,id", (session,)):
            item = dict(message)
            item["data"] = json.loads(item["data"])
            item["parts"] = []
            for part in conn.execute("SELECT * FROM part WHERE message_id=? ORDER BY id", (item["id"],)):
                part = dict(part)
                part["data"] = json.loads(part["data"])
                item["parts"].append(part)
            messages.append(item)
        events = []
        for event in conn.execute("SELECT * FROM event WHERE aggregate_id=? ORDER BY seq", (session,)):
            event = dict(event)
            event["data"] = json.loads(event["data"])
            events.append(event)
        sequence = dict(conn.execute("SELECT * FROM event_sequence WHERE aggregate_id=?", (session,)).fetchone())
        return {"info": info, "messages": messages, "events": events, "sequence": sequence}


def normalized(value, started):
    value = deepcopy(value)
    session = value["info"]["id"]
    assert re.fullmatch(r"ses_[0-9a-f]{12}[A-Za-z0-9]{14}", session)
    message_ids = {m["id"]: f"<message-{i}>" for i, m in enumerate(value["messages"])}
    part_ids = {p["id"]: f"<part-{i}-{j}>" for i, m in enumerate(value["messages"]) for j, p in enumerate(m["parts"])}

    def fresh_timestamp(value):
        assert started - 1000 <= value <= time.time() * 1000 + 1000, value
        return "<now>"

    def payload(data):
        if "parentID" in data:
            data["parentID"] = message_ids[data["parentID"]]
        if data.get("type") == "compaction" and "tail_start_id" in data:
            data["tail_start_id"] = message_ids[data["tail_start_id"]]
        return data

    info = value["info"]
    for key in ("metadata", "model", "permission", "revert", "summary_diffs"):
        if info.get(key) is not None:
            info[key] = json.loads(info[key])
    info["id"], info["slug"] = "<session>", "<slug>"
    for key in ("time_created", "time_updated"):
        info[key] = fresh_timestamp(info[key])
    for m in value["messages"]:
        assert re.fullmatch(r"msg_[0-9a-f]{12}[A-Za-z0-9]{14}", m["id"])
        m["id"] = message_ids[m["id"]]
        assert m["session_id"] == session
        m["session_id"] = "<session>"
        m["time_updated"] = fresh_timestamp(m["time_updated"])
        payload(m["data"])
        for p in m["parts"]:
            assert re.fullmatch(r"prt_[0-9a-f]{12}[A-Za-z0-9]{14}", p["id"])
            p["id"] = part_ids[p["id"]]
            p["message_id"] = message_ids[p["message_id"]]
            assert p["session_id"] == session
            p["session_id"] = "<session>"
            for key in ("time_created", "time_updated"):
                p[key] = fresh_timestamp(p[key])
            payload(p["data"])
    for i, event in enumerate(value["events"]):
        assert event["seq"] == i and event["aggregate_id"] == session
        assert re.fullmatch(r"evt_[0-9a-f]{12}[A-Za-z0-9]{14}", event["id"])
        event["id"], event["aggregate_id"] = f"<event-{i}>", "<session>"
        data = event["data"]
        assert data["sessionID"] == session
        data["sessionID"] = "<session>"
        if event["type"] == "session.created.1":
            data["info"]["id"], data["info"]["slug"] = "<session>", "<slug>"
            for key in ("created", "updated"):
                data["info"]["time"][key] = fresh_timestamp(data["info"]["time"][key])
        elif event["type"] == "message.updated.1":
            data["info"]["id"] = message_ids[data["info"]["id"]]
            assert data["info"]["sessionID"] == session
            data["info"]["sessionID"] = "<session>"
            payload(data["info"])
        elif event["type"] == "message.part.updated.1":
            data["time"] = fresh_timestamp(data["time"])
            data["part"]["id"] = part_ids[data["part"]["id"]]
            data["part"]["messageID"] = message_ids[data["part"]["messageID"]]
            assert data["part"]["sessionID"] == session
            data["part"]["sessionID"] = "<session>"
            payload(data["part"])
        else:
            raise AssertionError(f"Unexpected event: {event['type']}")
    assert value["sequence"]["aggregate_id"] == session
    assert value["sequence"]["seq"] == len(value["events"]) - 1
    assert value["sequence"]["owner_id"] is None
    value["sequence"]["aggregate_id"] = "<session>"
    return value


def difference(actual, expected, path=""):
    if isinstance(actual, dict) and isinstance(expected, dict):
        return [diff for key in actual.keys() | expected.keys()
                for diff in difference(actual.get(key), expected.get(key), f"{path}/{key}")]
    if isinstance(actual, list) and isinstance(expected, list) and len(actual) == len(expected):
        return [diff for i, (a, b) in enumerate(zip(actual, expected)) for diff in difference(a, b, f"{path}/{i}")]
    return [] if actual == expected else [f"{path}: {actual!r} != {expected!r}"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--opencode", type=Path, required=True)
    parser.add_argument("--binary", type=Path, default=REPO / "src-tauri/target/debug" / ("cc-sessions.exe" if os.name == "nt" else "cc-sessions"))
    args = parser.parse_args()
    messages = json.loads((REPO / "src-tauri/tests/fixtures/opencode-fork.json").read_text())
    with tempfile.TemporaryDirectory(prefix="cc-sessions-opencode-parity-") as temp:
        root = Path(temp)
        for name in ("project", "home", "codex", "claude", "cursor", "data/opencode"):
            (root / name).mkdir(parents=True)
        db = root / "data/opencode/opencode.db"
        (root / "config.json").write_text("{}", encoding="utf-8")
        environment = dict(os.environ, OPENCODE_DB=str(db), OPENCODE_TEST_HOME=str(root / "home"),
                           OPENCODE_CONFIG=str(root / "config.json"), OPENCODE_CONFIG_DIR=str(root / "config/opencode"),
                           OPENCODE_CONFIG_CONTENT='{"plugin":[]}', OPENCODE_DISABLE_PROJECT_CONFIG="1",
                           OPENCODE_DISABLE_MODELS_FETCH="1", OPENCODE_DISABLE_AUTOUPDATE="1",
                           CC_SESSIONS_WEBUI_SETTINGS=str(root / "settings.json"), CC_SESSIONS_WEBUI_DIST=str(REPO / "dist"))
        for variable in ("OPENCODE_SERVER_PASSWORD", "OPENCODE_SERVER_USERNAME"):
            environment.pop(variable, None)
        for variable, directory in (("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache"), ("XDG_STATE_HOME", "state")):
            environment[variable] = str(root / directory)
        version = subprocess.check_output([str(args.opencode.resolve()), "--version"], env=environment, creationflags=FLAGS).decode().strip()
        assert version == "1.18.30", f"Expected pinned OpenCode 1.18.30, got {version}"
        native_url, manager_url = f"http://127.0.0.1:{port()}", f"http://127.0.0.1:{port()}"
        native_command = [str(args.opencode.resolve()), "--pure", "serve", "--hostname", "127.0.0.1", "--port", native_url.rsplit(":", 1)[1]]
        manager_command = [str(args.binary.resolve()), "--provider", "opencode"]
        for provider, directory in (("opencode", "data/opencode"), ("codex", "codex"), ("claude", "claude"), ("cursor", "cursor")):
            manager_command += [f"--{provider}-dir", str(root / directory)]
        manager_command += ["webui", "--host", "127.0.0.1", "--port", manager_url.rsplit(":", 1)[1]]
        with server(native_command, root, environment, native_url + "/global/health", "native.log"), server(manager_command, root, environment, manager_url, "manager.log"):
            runtime = json.loads(re.search(r"window\.__CC_SESSIONS_WEBUI__ = (.*?);</script>", request(manager_url)).group(1))

            def invoke(name, data):
                return json.loads(request(manager_url + "/api/invoke/" + name, data, {"X-CC-Sessions-Webui-Token": runtime["apiToken"]}))

            source = json.loads(request(native_url + "/session", {"title": "Copy fixture (fork #2)"}))["id"]
            seed(db, source, messages)
            source_before = stored(db, source)
            path = locator(db, source)
            preview = invoke("preview_session_range", {"provider": "opencode", "rolloutPath": path, "offset": 0, "limit": 100})
            for part in (None, "prt_a10", "prt_z20", "prt_a20", "prt_b20", "prt_b30", "prt_a40"):
                started = time.time() * 1000
                values = {"opencodeDir": str(db.parent), "sessionId": source, "rolloutPath": path}
                count = len(messages)
                if part:
                    event = next(e for e in preview if e["raw"]["opencode"]["part_id"] == part)
                    message = event["raw"]["opencode"]["message_id"]
                    values["cutoff"] = {"event_index": event["index"], "message_id": message, "part_id": part}
                    count = next(i for i, m in enumerate(messages) if m["id"] == message) + 1
                actual = invoke("copy_opencode_session", values)
                assert actual["message_count"] == count
                assert actual["part_count"] == sum(len(m["parts"]) for m in messages[:count])
                boundary = {} if count == len(messages) else {"messageID": messages[count]["id"]}
                expected = json.loads(request(native_url + f"/session/{source}/fork", boundary))
                actual_data, expected_data = normalized(stored(db, actual["new_id"]), started), normalized(stored(db, expected["id"]), started)
                assert actual_data == expected_data, "\n".join(difference(actual_data, expected_data))
                # A native read is a compatibility check in addition to the SQLite comparison.
                native_messages = json.loads(request(native_url + f"/session/{actual['new_id']}/message"))
                assert len(native_messages) == count
                assert stored(db, source) == source_before
                print(f"PASS {part or 'full'}: {count} messages, {actual['part_count']} parts; rows + journal match native", flush=True)
            empty = json.loads(request(native_url + "/session", {"title": "Empty fixture"}))["id"]
            started = time.time() * 1000
            actual = invoke("copy_opencode_session", {"opencodeDir": str(db.parent), "sessionId": empty, "rolloutPath": locator(db, empty)})
            expected = json.loads(request(native_url + f"/session/{empty}/fork", {}))
            assert normalized(stored(db, actual["new_id"]), started) == normalized(stored(db, expected["id"]), started)
            print("PASS empty session: rows + journal match native", flush=True)


if __name__ == "__main__":
    main()
