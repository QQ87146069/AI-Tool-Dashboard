"""One-shot read-only audit. Prints schema/aggregate metadata, never chat text or credentials."""
import json
import os
import sqlite3
from pathlib import Path

home = Path.home()
codex = Path(os.environ.get("CODEX_HOME", home / ".codex"))
zcode = Path(os.environ.get("ZCODE_HOME", home / ".zcode")) / "v2"
for label, path, table in [
    ("Codex", codex / "state_5.sqlite", "threads"),
    ("ZCode", zcode / "tasks-index.sqlite", "tasks"),
]:
    print(label, "database present:", path.is_file())
    if not path.is_file():
        continue
    try:
        # mode=ro observes WAL; never use immutable=1 against a live database.
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=1) as db:
            db.execute("PRAGMA query_only=ON")
            columns = [row[1] for row in db.execute(f'PRAGMA table_info("{table}")')]
            print("columns:", json.dumps(columns))
            if label == "ZCode" and "task_status" in columns:
                print("task_status aggregate:", db.execute(
                    "SELECT task_status, COUNT(*) FROM tasks GROUP BY task_status"
                ).fetchall())
    except sqlite3.Error as error:
        print("read failed:", type(error).__name__)

cache = zcode / "coding-plan-cache.json"
print("ZCode plan cache present:", cache.is_file())
if cache.is_file():
    def shape(value, prefix=""):
        if isinstance(value, dict):
            for key, child in value.items():
                # Only names/types; no account, provider, plan or credential values.
                path = f"{prefix}.{key}" if prefix else key
                if prefix.count(".") < 4:
                    shape(child, path)
        else:
            print(prefix, type(value).__name__)
    shape(json.loads(cache.read_text(encoding="utf-8")))
