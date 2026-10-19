#!/usr/bin/env python3
"""Minimal fake PostgreSQL server for smoke-testing the pg83 driver."""
import hashlib
import socket
import struct
import sys
import threading

SALT = b"\x01\x02\x03\x04"
USER = "postgres"
PASSWORD = "secret"
NUL = b"\x00"


def cstr(raw: bytes) -> str:
    return raw.decode("utf-8").rstrip("\x00")


def md5_hex(data: bytes) -> str:
    return hashlib.md5(data).hexdigest()


def expected_token() -> str:
    inner = md5_hex(f"{PASSWORD}{USER}".encode())
    return "md5" + md5_hex(inner.encode() + SALT)


def message(tag: bytes, payload: bytes) -> bytes:
    return tag + struct.pack(">I", len(payload) + 4) + payload


def recv_exact(conn, size: int) -> bytes:
    chunks = b""
    while len(chunks) < size:
        chunk = conn.recv(size - len(chunks))
        if not chunk:
            raise ConnectionError("client closed")
        chunks += chunk
    return chunks


def handle(conn: socket.socket, log: list):
    (length,) = struct.unpack(">I", recv_exact(conn, 4))
    body = recv_exact(conn, length - 4)
    if body[:4] != struct.pack(">I", 196608):
        conn.sendall(message(b"E", b"SFATAL" + NUL + b"C08006" + NUL + b"Munsupported protocol" + NUL + NUL))
        return
    params = {}
    for pair in body[4:].split(NUL):
        if not pair:
            continue
        parts = pair.split(b"=", 1)
        if len(parts) == 2:
            params[cstr(parts[0])] = cstr(parts[1])
    log.append(("startup", params))

    conn.sendall(message(b"R", struct.pack(">I", 5) + SALT))
    tag = recv_exact(conn, 1)
    (length,) = struct.unpack(">I", recv_exact(conn, 4))
    payload = recv_exact(conn, length - 4)
    log.append(("auth_raw", tag, length, payload.hex()))
    token = payload.rstrip(NUL).decode("utf-8", "replace")
    if tag != b"p":
        conn.sendall(message(b"E", b"SFATAL" + NUL + b"C08006" + NUL + b"Mexpected password message" + NUL + NUL))
        return
    if token != expected_token():
        conn.sendall(message(b"E", b"SFATAL" + NUL + b"C28P01" + NUL + b"Mpassword authentication failed" + NUL + NUL))
        return
    conn.sendall(message(b"R", struct.pack(">I", 0)))

    conn.sendall(message(b"S", b"server_version" + NUL + b"8.3.1" + NUL))
    conn.sendall(message(b"S", b"client_encoding" + NUL + b"UTF8" + NUL))
    conn.sendall(message(b"K", struct.pack(">II", 4242, 0xCAFEBABE & 0xFFFFFFFF)))
    conn.sendall(message(b"Z", b"I"))

    in_txn = False
    while True:
        try:
            tag = recv_exact(conn, 1)
        except (ConnectionError, OSError):
            return
        if tag == b"X":
            log.append(("terminate", None))
            return
        if tag != b"Q":
            log.append(("unexpected_tag", tag))
            continue
        (length,) = struct.unpack(">I", recv_exact(conn, 4))
        sql = cstr(recv_exact(conn, length - 4))
        log.append(("query", sql))

        if sql.strip().upper() == "SELECT 1":
            send_row_description(conn, [("one", 23)])
            send_data_row(conn, ["1"])
            conn.sendall(message(b"C", b"SELECT 1"))
        elif "left(" in sql.lower() or "right(" in sql.lower():
            conn.sendall(message(b"E", b"SERROR" + NUL + b"C42883" + NUL + b"Mfunction left(text, integer) does not exist" + NUL + b"HNo function matches the given name and argument types." + NUL + NUL))
        elif "substring(" in sql.lower() and "payload" in sql.lower():
            send_row_description(conn, [("payload", 25)])
            send_data_row(conn, ["abc"])
            conn.sendall(message(b"C", b"SELECT 1"))
        elif "pg_database" in sql:
            send_row_description(conn, [("datname", 19)])
            for name in ["postgres", "template0", "template1"]:
                send_data_row(conn, [name])
            conn.sendall(message(b"C", b"SELECT 3"))
        elif "current_setting('server_version')" in sql:
            send_row_description(conn, [("server_version", 25), ("current_user", 19), ("current_database", 19)])
            send_data_row(conn, ["8.3.1", "postgres", "postgres"])
            conn.sendall(message(b"C", b"SELECT 1"))
        elif "pg_get_constraintdef" in sql:
            # Primary-key lookup for getColumns.
            send_row_description(conn, [("definition", 25)])
            send_data_row(conn, ['PRIMARY KEY (id)'])
            conn.sendall(message(b"C", b"SELECT 1"))
        elif "pg_attribute" in sql:
            # Column listing for getColumns. 8 columns:
            # attname, data_type, typtype, attnotnull, default, attnum, char_len, comment
            send_row_description(
                conn,
                [
                    ("attname", 19),
                    ("data_type", 19),
                    ("typtype", 18),
                    ("attnotnull", 16),
                    ("column_default", 25),
                    ("attnum", 21),
                    ("char_len", 23),
                    ("comment", 25),
                ],
            )
            send_data_row(conn, ["id", "int4", "b", "t", None, "1", None, "primary key"])
            send_data_row(conn, ["payload", "text", "b", "f", None, "2", None, None])
            send_data_row(conn, ["code", "varchar", "b", "f", None, "3", "12", None])
            conn.sendall(message(b"C", b"SELECT 3"))
        elif "pg_proc" in sql:
            # Function listing / function source.
            if "oidvectortypes" in sql:
                send_row_description(conn, [("proname", 19), ("object_type", 25), ("signature", 25)])
                send_data_row(conn, ["my_func", "FUNCTION", "(integer, text)"])
                conn.sendall(message(b"C", b"SELECT 1"))
            else:
                send_row_description(conn, [("source", 25)])
                send_data_row(
                    conn,
                    ["CREATE OR REPLACE FUNCTION public.my_func(integer, text) RETURNS boolean AS 'SELECT true' LANGUAGE sql;"],
                )
                conn.sendall(message(b"C", b"SELECT 1"))
        elif "pg_class" in sql:
            # Relation listing (listTables / listObjects).
            send_row_description(conn, [("relname", 19), ("object_type", 25), ("comment", 25)])
            send_data_row(conn, ["users", "TABLE", "user accounts"])
            send_data_row(conn, ["active_users", "VIEW", None])
            conn.sendall(message(b"C", b"SELECT 2"))
        elif "increment_by" in sql:
            # Sequence state (SELECT from the sequence relation itself).
            send_row_description(
                conn,
                [
                    ("increment_by", 20),
                    ("min_value", 20),
                    ("max_value", 20),
                    ("cache_value", 20),
                    ("is_cycled", 16),
                    ("last_value", 20),
                ],
            )
            send_data_row(conn, ["1", "1", "9223372036854775807", "1", "f", "42"])
            conn.sendall(message(b"C", b"SELECT 1"))
        elif sql.strip().upper() == "BEGIN":
            in_txn = True
            conn.sendall(message(b"C", b"BEGIN"))
        elif sql.strip().upper() in ("COMMIT", "ROLLBACK", "END"):
            in_txn = False
            conn.sendall(message(b"C", b"COMMIT"))
        else:
            conn.sendall(message(b"E", b"SERROR" + NUL + b"C42601" + NUL + b'Msyntax error at or near "smoke"' + NUL + NUL))
        conn.sendall(message(b"Z", b"T" if in_txn else b"I"))


def send_row_description(conn, fields):
    body = struct.pack(">H", len(fields))
    for name, oid in fields:
        body += name.encode() + NUL
        body += struct.pack(">IHIhiH", 0, 0, oid, -1, -1, 0)
    conn.sendall(message(b"T", body))


def send_data_row(conn, values):
    body = struct.pack(">H", len(values))
    for value in values:
        if value is None:
            body += struct.pack(">i", -1)  # SQL NULL
        else:
            encoded = value.encode()
            body += struct.pack(">I", len(encoded)) + encoded
    conn.sendall(message(b"D", body))


def main():
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 15432
    server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", port))
    server.listen(4)
    print(f"READY {port}", flush=True)
    while True:
        conn, _ = server.accept()
        log = []
        threading.Thread(target=serve, args=(conn, log), daemon=True).start()


def serve(conn, log):
    try:
        handle(conn, log)
    except Exception as error:
        import traceback
        log.append(("server_error", traceback.format_exc()))
    finally:
        conn.close()
        with open("smoke_server.log", "a", encoding="utf-8") as sink:
            for entry in log:
                sink.write(repr(entry) + "\n")


if __name__ == "__main__":
    main()
