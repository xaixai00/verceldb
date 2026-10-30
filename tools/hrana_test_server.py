"""Small Hrana-over-HTTP (v2/v3 pipeline) server backed by sqlite3, written
from docs/HRANA_3_SPEC.md, for testing clients without a real Turso database.

Supports: execute, batch (with ok/error/not/and/or/is_autocommit conditions),
sequence, close, get_autocommit; batons for multi-request streams; bearer token.
Used in CI to test the Turso driver; real deployments use Turso itself.
Usage: python3 -I tools/hrana_test_server.py <db-file> <port> [token]
"""
import base64, json, secrets, sqlite3, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DB, PORT = sys.argv[1], int(sys.argv[2])
TOKEN = sys.argv[3] if len(sys.argv) > 3 else None
streams = {}           # baton -> connection
lock = threading.Lock()
log = open(DB + ".requests.log", "a")


class Conn:
    def __init__(self):
        self.db = sqlite3.connect(DB, isolation_level=None, check_same_thread=False, timeout=5)
        self.sqls = {}  # sql_id -> text (store_sql)


def new_conn():
    return Conn()


def to_py(v):
    t = v["type"]
    if t == "null": return None
    if t == "integer": return int(v["value"])
    if t == "float": return float(v["value"])
    if t == "text": return v["value"]
    if t == "blob": return base64.b64decode(v["base64"] + "=" * (-len(v["base64"]) % 4))
    raise ValueError(f"bad value type {t}")


def from_py(x):
    if x is None: return {"type": "null"}
    if isinstance(x, bool): return {"type": "integer", "value": str(int(x))}
    if isinstance(x, int): return {"type": "integer", "value": str(x)}
    if isinstance(x, float): return {"type": "float", "value": x}
    if isinstance(x, str): return {"type": "text", "value": x}
    if isinstance(x, (bytes, memoryview)):  # sqld emits unpadded base64
        return {"type": "blob", "base64": base64.b64encode(bytes(x)).decode().rstrip("=")}
    raise ValueError(type(x))


def execute(conn, stmt):
    sql = stmt["sql"] if stmt.get("sql") is not None else conn.sqls[stmt["sql_id"]]
    args = [to_py(a) for a in stmt.get("args", [])]
    named = {a["name"].lstrip(":@$"): to_py(a["value"]) for a in stmt.get("named_args", [])}
    cur = conn.db.execute(sql, named if named else args)
    rows = cur.fetchall() if cur.description else []
    want = stmt.get("want_rows", True)
    return {
        "cols": [{"name": d[0], "decltype": None} for d in (cur.description or [])],
        "rows": [[from_py(v) for v in r] for r in rows] if want else [],
        "affected_row_count": max(cur.rowcount, 0) if not cur.description else 0,
        "last_insert_rowid": str(cur.lastrowid) if cur.lastrowid is not None else None,
        "rows_read": 0, "rows_written": 0, "query_duration_ms": 0.0,
    }


def err(e):
    msg = str(e)
    code = "SQLITE_CONSTRAINT_UNIQUE" if "UNIQUE constraint failed" in msg else (
        "SQLITE_CONSTRAINT_CHECK" if "CHECK constraint failed" in msg else "SQLITE_ERROR")
    return {"message": f"SQLite error: {msg}", "code": code}


def cond(c, results, errors, conn):
    t = c["type"]
    if t == "ok": return results[c["step"]] is not None
    if t == "error": return errors[c["step"]] is not None
    if t == "not": return not cond(c["cond"], results, errors, conn)
    if t == "and": return all(cond(x, results, errors, conn) for x in c["conds"])
    if t == "or": return any(cond(x, results, errors, conn) for x in c["conds"])
    if t == "is_autocommit": return not conn.db.in_transaction
    raise ValueError(t)


def batch(conn, b):
    results, errors = [], []
    for step in b["steps"]:
        c = step.get("condition")
        if c is not None and not cond(c, results, errors, conn):
            results.append(None); errors.append(None); continue
        try:
            results.append(execute(conn, step["stmt"])); errors.append(None)
        except Exception as e:
            results.append(None); errors.append(err(e))
    return {"step_results": results, "step_errors": errors}


def handle_request(conn, r):
    t = r["type"]
    if t == "execute": return {"type": "execute", "result": execute(conn, r["stmt"])}
    if t == "batch": return {"type": "batch", "result": batch(conn, r["batch"])}
    if t == "sequence":
        conn.db.executescript(r["sql"] if r.get("sql") is not None else conn.sqls[r["sql_id"]]); return {"type": "sequence"}
    if t == "get_autocommit": return {"type": "get_autocommit", "is_autocommit": not conn.db.in_transaction}
    if t == "store_sql":
        if r["sql_id"] in conn.sqls: raise ValueError("sql_id already in use")
        conn.sqls[r["sql_id"]] = r["sql"]; return {"type": "store_sql"}
    if t == "close_sql":
        conn.sqls.pop(r["sql_id"], None); return {"type": "close_sql"}
    if t == "close": return {"type": "close"}
    raise ValueError(f"unsupported request {t}")


class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def send_json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers(); self.wfile.write(body)

    def authed(self):
        if TOKEN is None: return True
        return self.headers.get("Authorization") == f"Bearer {TOKEN}"

    def do_GET(self):
        if self.path in ("/v2", "/v3", "/health", "/version"):
            self.send_response(200); self.end_headers(); return
        self.send_response(404); self.end_headers()

    def do_POST(self):
        if not self.authed():
            return self.send_json(401, {"message": "Unauthorized: invalid token"})
        if self.path not in ("/v2/pipeline", "/v3/pipeline"):
            return self.send_json(404, {"message": "not found"})
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        log.write(json.dumps({"path": self.path, "body": body})[:4000] + "\n"); log.flush()
        with lock:
            baton = body.get("baton")
            conn = streams.pop(baton, None) if baton else new_conn()
            if conn is None:
                return self.send_json(400, {"message": "invalid baton"})
            results, closed = [], False
            for r in body["requests"]:
                try:
                    results.append({"type": "ok", "response": handle_request(conn, r)})
                except Exception as e:
                    results.append({"type": "error", "error": err(e)})
                if r["type"] == "close": closed = True
            new_baton = None
            if not closed:
                new_baton = secrets.token_hex(16); streams[new_baton] = conn
            else:
                if conn.db.in_transaction: conn.db.rollback()
                conn.db.close()
        self.send_json(200, {"baton": new_baton, "base_url": None, "results": results})


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
