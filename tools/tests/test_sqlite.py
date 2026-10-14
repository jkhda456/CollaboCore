"""SQLite in the guest: the sqlite3 shell of tools.cpio and Python's sqlite3 module (python.cpio),
which are built from the same SQLite with the same options. The shell runs both from a script and
in a terminal (the engine in a pseudo-terminal), where libedit edits the line and recalls history.

    COLLABO_RUNTIME=dist/runtime/collabo-core-linux-x64 python3 tools/tests/test_sqlite.py
    TOOLS_IMAGE=tools/out/tools.cpio PYTHON_IMAGE=python/out/python.cpio ...   other images
"""
import json, os, re, subprocess, sys, time
S = os.path.dirname(os.path.abspath(__file__)); sys.path.insert(0, S)
import ptydrive
from ptydrive import plain
RT = os.environ["COLLABO_RUNTIME"]
fails = []
def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f"\n     {str(detail)[-1500:]}"))
    if not ok: fails.append(name)

entry = json.load(open(os.path.join(RT, "manifest.json")))["entry"]
images = {"--python-image": os.environ.get("PYTHON_IMAGE"), "--tools-image": os.environ.get("TOOLS_IMAGE")}
argv, words = [os.path.join(RT, entry[0])], iter(entry[1:])
for word in words:
    argv.append(word)
    if word.startswith("--"):
        value = next(words)
        argv.append(images.get(word) or (os.path.join(RT, value) if value.startswith("app/") else value))

def guest(script, timeout=180):
    """Output of a shell script run in the guest (after the prompt)."""
    out = subprocess.run([argv[0], "exec"] + argv[1:] + ["--no-network", "--", "sh", "-c", script],
                         capture_output=True, text=True, timeout=timeout).stdout.replace("\r", "")
    return out.split("root@collabo:~# ", 1)[-1]

out = guest(r'''
cd /tmp
sqlite3 --version
sqlite3 t.db "create table t(a, b); insert into t values (6, 7), (1, 2);
  create virtual table f using fts5(x); insert into f values ('hello world'), ('bye');
  create virtual table r using rtree(id, x0, x1); insert into r values (1, 0, 5);
  select 'mul', a * b from t where a = 6; select 'fts', x from f where f match 'hello';
  select 'math', sqrt(16); select 'rtree', id from r where x0 <= 3 and x1 >= 3;
  select 'json', json_object('a', 1); pragma journal_mode;"
printf '.mode json\nselect * from t order by a;\n.once |wc -l\nselect * from t;\n.system echo system-ok\n' | sqlite3 t.db
python3 -c "
import sqlite3
c = sqlite3.connect('/tmp/t.db')
print('python', sqlite3.sqlite_version, c.execute('select sum(a * b) from t').fetchone()[0],
      c.execute(\"select x from f where f match 'bye'\").fetchone()[0])
c.execute('create table p(v)'); c.executemany('insert into p values (?)', [(i,) for i in range(1000)]); c.commit()"
sqlite3 t.db "select 'shell-sees', count(*), sum(v) from p"
''')
version = re.search(r"^(3\.\d+\.\d+) ", out, re.M)
check("sqlite3 --version", version, out)
check("a query", "mul|42" in out, out)
check("FTS5", "fts|hello world" in out, out)
check("math functions", "math|4.0" in out, out)
check("R*Tree", "rtree|1" in out, out)
check("JSON", 'json|{"a":1}' in out, out)
check("journal mode without WAL is delete", re.search(r"^delete$", out, re.M), out)
check(".mode json", '[{"a":1,"b":2},\n{"a":6,"b":7}]' in out, out)
check(".once |cmd (popen)", re.search(r"^2$", out, re.M), out)
check(".system (system())", "system-ok" in out, out)
check("Python's sqlite3 is the same SQLite", version and f"python {version[1]} 44 bye" in out, out)
check("the shell reads what Python wrote", "shell-sees|1000|499500" in out, out)

# In a terminal: the banner, a query, an edited line (Ctrl-A goes to the start), history (up arrow).
PROMPT = r"root@collabo:[^\n#]*#"
t = ptydrive.Session(argv + ["--no-network", "--arg", "collabo.quiet=1"], cols=100, rows=30, env={"TERM": "xterm-256color"})
seen = 0
def run(keys, until, timeout=30):
    global seen
    t.send(keys)
    rx, end = re.compile(until, re.S), time.time() + timeout
    while time.time() < end:
        text = plain(t.all.decode(errors="replace"))
        if m := rx.search(text, seen):
            out, seen = text[seen:m.end()], m.end()
            return out
        t.read(0.2)
    raise TimeoutError(f"no {until!r} in:\n{text[seen:][-3000:]}")
try:
    run("", PROMPT, 90)
    out = run("sqlite3\r", r"sqlite> ")
    check("the shell starts in a terminal", "SQLite version" in out, out)
    out = run("select 6*7;\r", r"42.*?sqlite> ")
    check("a query in a terminal", "42" in out, out)
    out = run("7*8;\x01select \r", r"56.*?sqlite> ")
    check("line editing (Ctrl-A, then insert)", "56" in out, out)
    out = run("\x1b[A\x1b[A\r", r"42.*?sqlite> ")
    check("history (up arrow twice)", "42" in out, out)
    run(".quit\r", PROMPT)
except TimeoutError as e:
    check("terminal session", False, e)
finally:
    t.close()

print(f"{'FAIL' if fails else 'PASS'}: sqlite ({len(fails)} failed)")
sys.exit(1 if fails else 0)
