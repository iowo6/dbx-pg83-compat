#!/usr/bin/env python3
"""End-to-end smoke test: drives the built plugin binary as a DBX host would.

Starts the fake PostgreSQL server, spawns target/release/dbx-pg83-driver,
speaks the DBX jdbc-plugin JSON-RPC protocol over stdio, and asserts:
  * testConnection succeeds and reports PostgreSQL 8.3.1
  * connect + executeQuery of a 9.1-style left() SQL is rewritten to substring()
    (the fake server rejects raw left() with 42883, exactly like PG 8.3)
  * metadata calls (listDatabases) work
  * begin/commit manual transactions work
  * a server error surfaces as a JSON-RPC error with SQLSTATE
"""
import json
import os
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
EXE = os.path.join(ROOT, "dist", "dbx-pg83-compat", "bin", "dbx-pg83-driver.bat")
DRIVER_CMD = ["cmd", "/c", EXE]

PORT = 15433
CONNECTION = {
    "id": "conn-1",
    "name": "smoke",
    "db_type": "jdbc",
    "connection_string": f"jdbc:postgresql://127.0.0.1:{PORT}/postgres?user=postgres&password=secret",
}


class Plugin:
    def __init__(self):
        self.proc = subprocess.Popen(
            DRIVER_CMD, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        )
        self.next_id = 1

    def call(self, method, params=None):
        request = {"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}
        self.next_id += 1
        self.proc.stdin.write(json.dumps(request) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(f"plugin closed after {method}: {self.proc.stderr.read()}")
        return json.loads(line)

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


def start_fake_server():
    script = os.path.join(HERE, "fake_pg_server.py")
    log_path = os.path.join(HERE, "smoke_server.log")
    if os.path.exists(log_path):
        os.remove(log_path)
    proc = subprocess.Popen([sys.executable, script, str(PORT)], stdout=subprocess.PIPE, text=True)
    ready = proc.stdout.readline().strip()
    if not ready.startswith("READY"):
        raise RuntimeError(f"fake server failed: {ready}")
    return proc


def check(name, condition, detail=""):
    status = "PASS" if condition else "FAIL"
    print(f"[{status}] {name}" + (f" — {detail}" if detail and not condition else ""))
    return condition


def main():
    server = start_fake_server()
    plugin = Plugin()
    failures = 0
    try:
        # 1. testConnection
        result = plugin.call("testConnection", {"connection": CONNECTION})
        failures += not check(
            "testConnection ok",
            result.get("result", {}).get("ok") is True
            and result["result"]["databaseInfo"]["productVersion"].startswith("8.3.1"),
            json.dumps(result),
        )

        # 2. connect
        result = plugin.call("connect", {"connection": CONNECTION})
        failures += not check(
            "connect ok", result.get("result", {}).get("ok") is True, json.dumps(result)
        )

        # 3. executeQuery with the DBX preview shape (left() from table_select.rs)
        preview_sql = 'SELECT left("payload", 8193) AS "payload" FROM t LIMIT 100;'
        result = plugin.call(
            "executeQuery",
            {
                "connection": CONNECTION,
                "database": "postgres",
                "sql": preview_sql,
                "maxRows": 100,
            },
        )
        ok = (
            result.get("result", {}).get("rows") == [["abc"]]
            and result["result"]["columns"] == ["payload"]
        )
        failures += not check("left() preview rewritten and executed", ok, json.dumps(result))

        # 4. user SQL with right()
        result = plugin.call(
            "executeQuery",
            {"connection": CONNECTION, "sql": "SELECT left(right(name, 3), 2)", "maxRows": 10},
        )
        # The fake server only answers substring+payload; the right() rewrite
        # contains substring but not 'payload', so it lands in the syntax-error
        # branch — proving the rewrite happened (no 42883).
        message = json.dumps(result)
        failures += not check(
            "right() rewritten (no 42883)",
            "42883" not in message and "syntax error" in message,
            message,
        )

        # 5. raw left() without rewrite would fail — control case via metadata
        result = plugin.call("listDatabases", {"connection": CONNECTION})
        names = [entry["name"] for entry in result.get("result", [])]
        failures += not check("listDatabases", names == ["postgres", "template0", "template1"], json.dumps(result))

        # 6. manual transaction
        result = plugin.call("beginManualTransaction", {"connection": CONNECTION})
        failures += not check("beginManualTransaction", result.get("result", {}).get("ok") is True, json.dumps(result))
        result = plugin.call(
            "executeInManualTransaction",
            {"connection": CONNECTION, "sql": "SELECT 1", "maxRows": 10},
        )
        failures += not check(
            "executeInManualTransaction",
            result.get("result", {}).get("rows") == [["1"]],
            json.dumps(result),
        )
        result = plugin.call("commitManualTransaction", {"connection": CONNECTION})
        failures += not check("commitManualTransaction", result.get("result", {}).get("ok") is True, json.dumps(result))

        # 7. getColumns resolves primary keys via pg_get_constraintdef
        result = plugin.call(
            "getColumns",
            {"connection": CONNECTION, "schema": "public", "table": "users"},
        )
        columns = {entry["name"]: entry for entry in result.get("result", [])}
        ok = (
            set(columns) == {"id", "payload", "code"}
            and columns["id"]["is_primary_key"] is True
            and columns["id"]["is_nullable"] is False
            and columns["payload"]["is_nullable"] is True
            and columns["payload"]["is_primary_key"] is False
            and columns["code"]["character_maximum_length"] == 12
        )
        failures += not check("getColumns with constraintdef PK", ok, json.dumps(result))

        # 8. listObjects uses oidvectortypes, not pg_get_function_identity_arguments
        result = plugin.call("listObjects", {"connection": CONNECTION, "schema": "public"})
        objects = {entry["name"]: entry for entry in result.get("result", [])}
        ok = (
            "users" in objects
            and objects["users"]["object_type"] == "TABLE"
            and objects["users"]["comment"] == "user accounts"
            and "my_func" in objects
            and objects["my_func"]["object_type"] == "FUNCTION"
            and objects["my_func"]["signature"] == "(integer, text)"
        )
        failures += not check("listObjects with oidvectortypes signatures", ok, json.dumps(result))

        # 9. sequence source reads the sequence relation itself
        result = plugin.call(
            "getObjectSource",
            {"connection": CONNECTION, "schema": "public", "name": "users_id_seq", "object_type": "SEQUENCE"},
        )
        source = result.get("result", {}).get("source", "")
        ok = (
            "CREATE SEQUENCE" in source
            and "INCREMENT BY 1" in source
            and "last_value: 42" in source
        )
        failures += not check("sequence source from relation state", ok, json.dumps(result))

        # 10. unknown method surfaces the DBX-style error text
        result = plugin.call("notAMethod")
        failures += not check(
            "unknown method error",
            "Unsupported JDBC plugin method" in result.get("error", {}).get("message", ""),
            json.dumps(result),
        )
    finally:
        plugin.close()
        server.terminate()

    # Verify server-side log: the rewrite must NOT have sent left()/right(),
    # and the metadata queries must stay inside the 8.3 function vocabulary.
    with open(os.path.join(HERE, "smoke_server.log"), encoding="utf-8") as sink:
        queries = [eval(line) for line in sink]  # noqa: S307 — our own log
    sent_sql = [entry[1] for entry in queries if entry[0] == "query"]
    has_raw_left = any("left(" in sql.lower() or "right(" in sql.lower() for sql in sent_sql)
    failures += not check("server never saw left()/right()", not has_raw_left, str(sent_sql))
    check(
        "server saw substring rewrite",
        any("substring(" in sql for sql in sent_sql),
        str(sent_sql),
    )
    forbidden = [
        "pg_get_function_identity_arguments",
        "pg_get_function_arguments",
        "pg_get_functiondef",
        "_pg_truetypid",
        "_pg_truetypmod",
        "_pg_char_max_length",
        "seq_increment",
        "pg_sequence",
    ]
    offenders = [
        token
        for token in forbidden
        if any(token in sql.lower() for sql in sent_sql)
    ]
    failures += not check("metadata SQL stayed 8.3-compatible", not offenders, str(offenders))

    print()
    if failures:
        print(f"{failures} smoke check(s) FAILED")
        sys.exit(1)
    print("All smoke checks passed.")


if __name__ == "__main__":
    main()
