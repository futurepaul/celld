# probe.py DIR SECONDS: times a SQLite read, a write at synchronous=FULL and one
# at NORMAL, each on a fresh connection as the dev store makes them, every
# 200 ms for SECONDS, in DIR. Run it beside the same disk load as run.sh.
import sqlite3, sys, threading, time, os
d = sys.argv[1]; stop = time.time() + float(sys.argv[2])
def db(name, sync):
    c = sqlite3.connect(os.path.join(d, name), timeout=30); c.execute("pragma journal_mode=WAL"); c.execute(f"pragma synchronous={sync}")
    c.execute("create table if not exists t(k text primary key, v blob)"); c.execute("insert or replace into t values('lease', zeroblob(400))"); c.commit(); c.close()
res = {}
def probe(label, name, sync, write):
    worst = 0; n = 0; over = 0
    while time.time() < stop:
        t = time.time()
        c = sqlite3.connect(os.path.join(d, name), timeout=30); c.execute(f"pragma synchronous={sync}")
        if write:
            c.execute("begin immediate"); c.execute("update t set v = randomblob(400) where k='lease'"); c.commit()
        else:
            c.execute("select length(v) from t where k='lease'").fetchone()
        c.close()
        e = time.time() - t; worst = max(worst, e); n += 1; over += e > 0.5
        time.sleep(0.2)
    res[label] = f"{n} ops, worst {worst*1000:.0f} ms, {over} over 500 ms"
for name, sync in (("full.db", "FULL"), ("normal.db", "NORMAL")): db(name, sync)
ts = [threading.Thread(target=probe, args=a) for a in (("read (WAL)", "full.db", "FULL", False), ("write FULL", "full.db", "FULL", True), ("write NORMAL", "normal.db", "NORMAL", True))]
[t.start() for t in ts]; [t.join() for t in ts]
for k, v in res.items(): print(k + ": " + v)
