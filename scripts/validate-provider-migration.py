"""Validate provider compatibility with Codex 0.167.1 and loopback mock responses.

Requires Python 3.10+, zstandard, an existing Codex 0.167.1 binary and a freshly
built cc-sessions CLI. --output must not exist. Every home, lock, file, turn and
provider is synthetic. No credentials are copied, real providers called, or
external processes terminated. Loaded-reference SIGTERM tests run only on POSIX
and terminate only the app-server this script created. No Desktop UI is tested.

Run: python scripts/validate-provider-migration.py --codex /path/to/codex \
  --cc-sessions src-tauri/target/debug/cc-sessions --output /tmp/provider-probe-new
"""

import contextlib, hashlib, http.server, json, os, pathlib, queue, shutil, sqlite3, subprocess, threading, time, uuid, zstandard

ROOT = None
BINARY = None


class Mock(http.server.BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *args):
        pass

    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"data":[]}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        Mock.calls.append({"path": self.path, "body": body})
        rid = "resp_" + uuid.uuid4().hex
        mid = "msg_" + uuid.uuid4().hex
        msg = {
            "id": mid,
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "phase": "final_answer",
            "content": [
                {"type": "output_text", "text": "SYNTHETIC_REPLY", "annotations": []}
            ],
        }
        response = {
            "id": rid,
            "object": "response",
            "created_at": int(time.time()),
            "status": "completed",
            "model": "mock-model",
            "output": [msg],
            "usage": {
                "input_tokens": 8,
                "output_tokens": 4,
                "total_tokens": 12,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens_details": {"reasoning_tokens": 0},
            },
        }
        events = [
            {
                "type": "response.created",
                "response": {**response, "status": "in_progress", "output": []},
            },
            {
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {**msg, "status": "in_progress", "content": []},
            },
            {
                "type": "response.output_text.delta",
                "item_id": mid,
                "output_index": 0,
                "content_index": 0,
                "delta": "SYNTHETIC_REPLY",
            },
            {"type": "response.output_item.done", "output_index": 0, "item": msg},
            {"type": "response.completed", "response": response},
        ]
        data = "".join(
            "event: " + e["type"] + "\ndata: " + json.dumps(e) + "\n\n" for e in events
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Native:
    def __init__(self, home, provider="source"):
        self.home = home
        self.sequence = 0
        self.q = queue.Queue()
        self.log = []
        self.err = (home / "native.stderr").open("a")
        env = child_env(home)
        env["CODEX_HOME"] = str(home)
        self.p = subprocess.Popen(
            [
                str(BINARY),
                "app-server",
                "--stdio",
                "-c",
                f'model_provider="{provider}"',
            ],
            cwd=home,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.err,
            text=True,
        )

        def read():
            for line in self.p.stdout:
                try:
                    r = json.loads(line)
                except Exception:
                    continue
                self.log.append(r)
                self.q.put(r)

        threading.Thread(target=read, daemon=True).start()
        self.call(
            "initialize",
            {
                "clientInfo": {"name": "cc_sessions_provider_probe", "version": "1"},
                "capabilities": {"experimentalApi": True},
            },
        )
        self.p.stdin.write('{"method":"initialized"}\n')
        self.p.stdin.flush()

    def call(self, method, params, allow_error=False):
        self.sequence += 1
        self.p.stdin.write(
            json.dumps({"id": self.sequence, "method": method, "params": params}) + "\n"
        )
        self.p.stdin.flush()
        while True:
            r = self.q.get(timeout=40)
            if r.get("id") == self.sequence:
                if "error" in r:
                    if allow_error:
                        return r
                    raise AssertionError((method, r))
                return r["result"]

    def turn(self, tid, text):
        r = self.call(
            "turn/start",
            {
                "threadId": tid,
                "input": [{"type": "text", "text": text, "text_elements": []}],
            },
        )
        turnid = r["turn"]["id"]
        while True:
            x = self.q.get(timeout=50)
            if (
                x.get("method") == "turn/completed"
                and x["params"]["turn"]["id"] == turnid
            ):
                assert x["params"]["turn"]["status"] == "completed", x
                return x

    def close(self, signal=False):
        if signal:
            self.p.terminate()
        else:
            self.p.stdin.close()
        try:
            self.p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.p.kill()
            self.p.wait()
        with (self.home / "rpc.jsonl").open("a") as f:
            for r in self.log:
                f.write(json.dumps(r) + "\n")
        self.err.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def setup(home, port):
    home.mkdir(parents=True, exist_ok=False)
    text = 'project_doc_max_bytes=0\nmodel="mock-model"\nmodel_provider="source"\n[features]\nshell_snapshot=false\n'
    for name in ["source", "target", "longer_provider"]:
        text += f'[model_providers.{name}]\nname="{name}"\nbase_url="http://127.0.0.1:{port}/v1"\nwire_api="responses"\nrequires_openai_auth=false\nrequest_max_retries=0\nstream_max_retries=0\n'
    (home / "config.toml").write_text(text)


def row(home, tid):
    with sqlite3.connect(home / "state_5.sqlite") as c:
        c.row_factory = sqlite3.Row
        return dict(c.execute("SELECT * FROM threads WHERE id=?", (tid,)).fetchone())


def fixture(home, port, mode):
    setup(home, port)
    with Native(home) as n:
        r = n.call(
            "thread/start",
            {
                "cwd": str(home),
                "historyMode": mode,
                "modelProvider": "source",
                "ephemeral": False,
                "baseInstructions": "Synthetic compatibility test. Never call tools. Reply briefly.",
            },
        )
        tid = r["thread"]["id"]
        n.turn(tid, "SYNTHETIC_ORIGINAL")
    return tid, pathlib.Path(row(home, tid)["rollout_path"])


def retag(home, tid, to):
    with sqlite3.connect(home / "state_5.sqlite") as c:
        c.execute("UPDATE threads SET model_provider=? WHERE id=?", (to, tid))


CC = None


def physical(path):
    return path if path.exists() else pathlib.Path(str(path) + ".zst")


def raw(path):
    path = physical(path)
    b = path.read_bytes()
    return (
        zstandard.ZstdDecompressor().decompress(b) if str(path).endswith(".zst") else b
    )


def settings(path):
    return [
        x["payload"]["thread_settings"]
        for x in map(json.loads, raw(path).splitlines())
        if x.get("type") == "event_msg"
        and x.get("payload", {}).get("type") == "thread_settings_applied"
    ]


def backup(home, tid, path):
    b = home / "probe-backup"
    b.mkdir()
    p = physical(path)
    shutil.copy2(p, b / p.name)
    with sqlite3.connect(home / "state_5.sqlite") as src, sqlite3.connect(
        b / "state_5.sqlite"
    ) as dst:
        src.backup(dst)
    (b / "receipt.json").write_text(
        json.dumps(
            {
                "id": tid,
                "path": str(p),
                "row": row(home, tid),
                "sha256": hashlib.sha256(p.read_bytes()).hexdigest(),
            },
            indent=2,
        )
    )
    return b


def magpie_rewrite(path, to):
    p = physical(path)
    data = raw(path)
    lines = []
    for line in data.splitlines(keepends=True):
        x = json.loads(line)
        if x.get("type") == "session_meta" and x["payload"].get("model_provider"):
            old = json.dumps(x["payload"]["model_provider"]).encode()
            new = json.dumps(to).encode()
            # Independent reproduction of header-only replacement. Preserve all other bytes.
            key = b'"model_provider":'
            start = line.index(key) + len(key)
            while line[start : start + 1] == b" ":
                start += 1
            assert line[start : start + len(old)] == old
            line = line[:start] + new + line[start + len(old) :]
        lines.append(line)
    out = b"".join(lines)
    p.write_bytes(
        zstandard.ZstdCompressor().compress(out) if str(p).endswith(".zst") else out
    )


def lock_available(home, tid):
    import fcntl

    f = (home / "thread-writer-locks" / (tid + ".lock")).open("a+")
    try:
        fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        return True
    except BlockingIOError:
        return False
    finally:
        f.close()


def repair(home, kind):
    env = child_env(home)
    r = subprocess.run(
        [str(CC), "--codex-dir", str(home), "--json", "repair", kind],
        env=env,
        capture_output=True,
        text=True,
        timeout=60,
    )
    return {"code": r.returncode, "stdout": r.stdout, "stderr": r.stderr}


def summarize(result):
    if "error" in result:
        return result["error"]
    return {
        "id": result["thread"]["id"],
        "provider": result["thread"]["modelProvider"],
        "historyMode": result["thread"]["historyMode"],
        "sandbox": result.get("sandbox"),
        "approvalPolicy": result.get("approvalPolicy"),
        "cwd": result.get("cwd"),
    }


def case(out, port, mode, packed, to="target"):
    name = mode + ("-zst" if packed else "-plain") + ("-long" if to != "target" else "")
    home = out / name
    tid, path = fixture(home, port, mode)
    if packed:
        z = pathlib.Path(str(path) + ".zst")
        z.write_bytes(zstandard.ZstdCompressor().compress(path.read_bytes()))
        path.unlink()
    r = {"name": name, "id": tid, "path": str(path), "steps": {}}
    base = raw(path)
    before = row(home, tid)
    oldsettings = settings(path)[-1]
    with Native(home, to) as n:
        r["steps"]["native_cross_resume"] = summarize(
            n.call(
                "thread/resume",
                {"threadId": tid, "modelProvider": to, "excludeTurns": True},
                True,
            )
        )
        r["steps"]["native_cross_fork"] = summarize(
            n.call(
                "thread/fork",
                {"threadId": tid, "modelProvider": to, "excludeTurns": False},
                True,
            )
        )
    b = backup(home, tid, path)
    r["backup"] = str(b)
    original_physical = physical(path)
    prehash = hashlib.sha256(original_physical.read_bytes()).hexdigest()
    if mode == "legacy":
        magpie_rewrite(path, to)
    retag(home, tid, to)
    after = row(home, tid)
    r["steps"]["row_fields_changed"] = [k for k in before if before[k] != after[k]]
    r["steps"]["retag_keeps_file_bytes"] = (
        hashlib.sha256(physical(path).read_bytes()).hexdigest() == prehash
    )
    with Native(home, to) as n:
        result = n.call("thread/resume", {"threadId": tid, "excludeTurns": True}, True)
        r["steps"]["resume_after_retag"] = summarize(result)
    r["steps"]["persisted_provider_after_resume"] = settings(path)[-1][
        "model_provider_id"
    ]
    newsettings = settings(path)[-1]
    r["steps"]["settings_fields_changed"] = [
        k
        for k in set(oldsettings) | set(newsettings)
        if oldsettings.get(k) != newsettings.get(k)
    ]
    r["steps"]["repair_threads"] = repair(home, "threads")
    r["steps"]["provider_after_repair"] = row(home, tid)["model_provider"]
    r["steps"]["repair_index"] = repair(home, "index")
    with Native(home, to) as n:
        result = n.call("thread/resume", {"threadId": tid, "excludeTurns": True}, True)
        r["steps"]["cold_resume_after_repair"] = summarize(result)
        if "error" not in result:
            n.turn(tid, "SYNTHETIC_CONTINUE")
            r["steps"]["continued"] = True
            r["steps"]["provider_request_has_original"] = (
                "SYNTHETIC_ORIGINAL" in json.dumps(Mock.calls[-1]["body"])
            )
    r["steps"]["final_db_provider"] = row(home, tid)["model_provider"]
    r["steps"]["backup_bytes_match"] = (
        hashlib.sha256((b / original_physical.name).read_bytes()).hexdigest() == prehash
    )
    # Persisted history may be natively appended/materialized, but original conversation must survive.
    r["steps"]["original_text_preserved"] = b"SYNTHETIC_ORIGINAL" in raw(path)
    return r


def row_only_repair_case(out, port):
    home = out / "row-only-repair"
    tid, path = fixture(home, port, "paginated")
    retag(home, tid, "target")
    r = {
        "name": "row-only-before-native-resume",
        "before": row(home, tid)["model_provider"],
        "repair": repair(home, "threads"),
        "after": row(home, tid)["model_provider"],
    }
    return r


def loaded_case(out, port, mode):
    home = out / ("loaded-" + mode)
    tid, path = fixture(home, port, mode)
    backup(home, tid, path)
    n = Native(home, "source")
    n.call("thread/resume", {"threadId": tid, "excludeTurns": True})
    r = {"name": "loaded-" + mode, "writer_lock_available": lock_available(home, tid)}
    if mode == "legacy":
        magpie_rewrite(path, "target")
    retag(home, tid, "target")
    n.close(signal=True)
    r["after_sigterm"] = row(home, tid)["model_provider"]
    with Native(home, "target") as m:
        result = m.call("thread/resume", {"threadId": tid, "excludeTurns": True}, True)
        r["resume"] = summarize(result)
        if "error" not in result:
            m.turn(tid, "SYNTHETIC_LOADED_CONTINUE")
            r["continued"] = True
    return r


def child_env(home):
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith("CODEX_")
        and k not in ("OPENAI_API_KEY", "ANTHROPIC_API_KEY")
    }
    env.update(
        HOME=str(home),
        XDG_CACHE_HOME=str(home / "probe-cache"),
        XDG_CONFIG_HOME=str(home / "probe-config"),
        XDG_DATA_HOME=str(home / "probe-data"),
    )
    env["PATH"] = str(ROOT / "bin") + os.pathsep + env.get("PATH", "")
    return env


def cli(home, arguments):
    process = subprocess.run(
        [str(CC), "--codex-dir", str(home), "--json"] + arguments,
        env=child_env(home),
        capture_output=True,
        text=True,
        timeout=80,
    )
    try:
        result = json.loads(process.stdout)
    except ValueError:
        result = {"stdout": process.stdout, "stderr": process.stderr}
    return {"exit": process.returncode, "report": result}


def product_case(out, port, mode, packed, loaded, inherited=False):
    name = (
        "cc-"
        + mode
        + ("-zst" if packed else "-plain")
        + ("-loaded" if loaded else "")
        + ("-inherited" if inherited else "")
    )
    home = out / name
    tid, path = fixture(home, port, mode)
    parent_path = None
    if inherited:
        parent_path = path
        with Native(home, "source") as native:
            fork = native.call(
                "thread/fork",
                {"threadId": tid, "modelProvider": "source", "excludeTurns": False},
            )
            tid = fork["thread"]["id"]
        path = pathlib.Path(row(home, tid)["rollout_path"])
        parent_before = raw(parent_path)
    if packed:
        z = pathlib.Path(str(path) + ".zst")
        z.write_bytes(zstandard.ZstdCompressor().compress(path.read_bytes()))
        path.unlink()
    original = raw(path)
    (home / "config.toml").write_text(
        (home / "config.toml")
        .read_text()
        .replace('model_provider="source"', 'model_provider="target"')
    )
    live = None
    if loaded:
        live = Native(home, "source")
        live.call("thread/resume", {"threadId": tid, "excludeTurns": True})
    args = [
        "repair",
        "clone",
        "--id",
        tid,
        "--target-provider",
        "target",
        "--strategy",
        "follow",
    ]
    result = {"name": name, "id": tid, "preview": cli(home, args + ["--dry-run"])}
    try:
        result["migration"] = cli(home, args)
    finally:
        if live:
            live.close()
    expected = mode == "paginated" and not loaded
    migration = result["migration"]["report"]
    assert migration.get("ok") == expected, result
    result["history_prefix"] = raw(path).startswith(original)
    assert result["history_prefix"]
    provider = "target" if expected else "source"
    result["db_provider"] = row(home, tid)["model_provider"]
    assert result["db_provider"] == provider
    result["persisted_provider"] = settings(path)[-1]["model_provider_id"]
    assert result["persisted_provider"] == provider
    b = pathlib.Path(migration["backup_path"])
    receipt = json.loads((b / "provider-migration.json").read_text())
    result["receipt_status"] = receipt["status"]
    assert receipt["status"] == ("completed" if expected else "failed_unchanged")
    result["backup_logical_bytes_match"] = (
        b / path.relative_to(home)
    ).read_bytes() == original
    assert result["backup_logical_bytes_match"]
    result["backup_verify"] = cli(home, ["backup", "verify", "--backup-path", str(b)])
    assert result["backup_verify"]["report"]["all_ok"], result
    if expected:
        result["noop"] = cli(home, args)
        assert (
            result["noop"]["report"]["ok"]
            and result["noop"]["report"]["backup_path"] is None
        )
        result["repair_threads"] = repair(home, "threads")
        result["repair_index"] = repair(home, "index")
        assert result["repair_threads"]["code"] == result["repair_index"]["code"] == 0
        with Native(home, "target") as native:
            result["cold_resume"] = summarize(
                native.call("thread/resume", {"threadId": tid, "excludeTurns": True})
            )
            native.turn(tid, "SYNTHETIC_CC_CONTINUE")
            result["continued"] = True
            result["request_contains_original"] = "SYNTHETIC_ORIGINAL" in json.dumps(
                Mock.calls[-1]["body"]
            )
            assert result["request_contains_original"]
        # Restore only this synthetic fixture after preserving continuation evidence.
        result["restore"] = cli(
            home,
            ["backup", "restore", "--backup-path", str(b), "--id", tid, "--overwrite"],
        )
        assert result["restore"]["report"].get("ok"), result
        result["restored_provider"] = row(home, tid)["model_provider"]
        assert result["restored_provider"] == "source"
        result["restored_original_bytes"] = raw(path) == original
        assert result["restored_original_bytes"]
        (home / "config.toml").write_text(
            (home / "config.toml")
            .read_text()
            .replace('model_provider="target"', 'model_provider="source"')
        )
        with Native(home, "source") as native:
            result["restored_resume"] = summarize(
                native.call("thread/resume", {"threadId": tid, "excludeTurns": True})
            )
    if parent_path:
        result["inherited_parent_bytes_unchanged"] = raw(parent_path) == parent_before
        assert result["inherited_parent_bytes_unchanged"]
    return result


def main():
    import argparse

    global ROOT, BINARY, CC
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", type=pathlib.Path, required=True)
    parser.add_argument("--cc-sessions", type=pathlib.Path, required=True)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    ROOT = args.output.resolve()
    BINARY = args.codex.resolve()
    CC = args.cc_sessions.resolve()
    assert BINARY.is_file() and CC.is_file()
    version = subprocess.check_output([str(BINARY), "--version"], text=True).strip()
    assert version == "codex-cli 0.167.1", version
    ROOT.mkdir(parents=True, exist_ok=False)
    (ROOT / "bin").mkdir()
    link = ROOT / "bin" / ("codex.exe" if os.name == "nt" else "codex")
    if os.name == "nt":
        shutil.copy2(BINARY, link)
    else:
        link.symlink_to(BINARY)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Mock)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    report = {
        "version": version,
        "magpie_reference": "1f99f6f0b54a0627be425c09af31260705c16857",
        "cc_comparison_base": "8bdb72ec40465a8f49434638fdc3c326b42f9363",
        "cases": [],
    }
    failures = []
    tasks = [
        (case, (mode, packed, to))
        for mode, packed, to in [
            ("legacy", False, "target"),
            ("legacy", True, "target"),
            ("paginated", False, "target"),
            ("paginated", True, "target"),
            ("paginated", False, "longer_provider"),
        ]
    ]
    tasks.append((row_only_repair_case, ()))
    if os.name != "nt":
        tasks.extend([(loaded_case, ("legacy",)), (loaded_case, ("paginated",))])
    tasks.extend(
        (product_case, (mode, packed, loaded))
        for mode, packed, loaded in [
            ("paginated", False, False),
            ("paginated", True, False),
            ("paginated", False, True),
            ("legacy", False, False),
            ("legacy", True, False),
        ]
    )
    tasks.append((product_case, ("paginated", False, False, True)))
    try:
        for fn, params in tasks:
            try:
                result = fn(ROOT, server.server_port, *params)
                if fn == case:
                    steps = result["steps"]
                    assert (
                        steps["native_cross_resume"]["data"]["type"]
                        == "modelProviderMismatch"
                    )
                    assert (
                        steps["persisted_provider_after_resume"] == params[2]
                        and steps["provider_after_repair"] == params[2]
                    )
                    assert (
                        steps["continued"]
                        and steps["provider_request_has_original"]
                        and steps["backup_bytes_match"]
                    )
                    assert steps["row_fields_changed"] == ["model_provider"] and steps[
                        "settings_fields_changed"
                    ] == ["model_provider_id"]
                    if params[0] == "paginated":
                        assert steps["retag_keeps_file_bytes"]
                elif fn == row_only_repair_case:
                    assert result["before"] == "target" and result["after"] == "source"
                elif fn == loaded_case:
                    assert not result["writer_lock_available"] and result["continued"]
            except Exception as error:
                result = {
                    "case": fn.__name__,
                    "parameters": params,
                    "error": repr(error),
                }
                failures.append(result)
            report["cases"].append(result)
            print(json.dumps(result, ensure_ascii=False), flush=True)
            (ROOT / "report.json").write_text(
                json.dumps(report, indent=2, ensure_ascii=False)
            )
    finally:
        server.shutdown()
        (ROOT / "mock-calls.json").write_text(json.dumps(Mock.calls, indent=2))
        report["failures"] = failures
        (ROOT / "report.json").write_text(
            json.dumps(report, indent=2, ensure_ascii=False)
        )
    print("Report:", ROOT / "report.json")
    if failures:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
