"""Exercise ASB's generated scope SQL against realistic scheduler identities."""
import sqlite3
import sys

query, case = sys.argv[1:]
db = sqlite3.connect(':memory:')
db.executescript('''
CREATE TABLE process(upid INTEGER, pid INTEGER, parent_upid INTEGER, start_ts INTEGER, end_ts INTEGER);
CREATE TABLE thread(utid INTEGER, tid INTEGER, upid INTEGER, start_ts INTEGER, end_ts INTEGER);
CREATE TABLE sched(utid INTEGER, ts INTEGER, dur INTEGER);
CREATE TABLE slice(name TEXT, ts INTEGER);
CREATE TABLE ftrace_event(ts INTEGER, name TEXT, utid INTEGER, arg_set_id INTEGER);
INSERT INTO process VALUES (1,42,NULL,0,100), (2,77,NULL,0,100), (3,500,NULL,0,100);
INSERT INTO thread VALUES (1,42,1,0,100), (2,77,2,0,100), (3,88,2,0,100), (5,500,3,0,100);
INSERT INTO sched VALUES (1,1,98);
''')
args = {1: {'prev_pid': 42, 'next_pid': 77}, 2: {'prev_pid': 77, 'next_pid': 88}, 3: {'pid': 42}, 4: {'buf': 'I|500|asb.agent.started.500-1'}, 5: {'buf': 'I|500|asb.agent.started.other'}}
db.create_function('EXTRACT_ARG', 2, lambda idx, key: args.get(idx, {}).get(key))
if case != 'empty':
    db.execute("INSERT INTO ftrace_event VALUES (10,'sched_switch',1,1)")
db.execute("INSERT INTO ftrace_event VALUES (11,'sys_enter',1,NULL)")
db.execute("INSERT INTO ftrace_event VALUES (12,'sched_waking',2,3)")
db.execute("INSERT INTO ftrace_event VALUES (12,'print',5,4)")
if case == 'unrelated':
    db.execute("INSERT INTO ftrace_event VALUES (13,'sched_switch',2,2)")
if case == 'foreign_syscall':
    db.execute("INSERT INTO ftrace_event VALUES (14,'sys_enter',2,NULL)")
if case == 'foreign_print':
    db.execute("INSERT INTO ftrace_event VALUES (14,'print',2,4)")
if case == 'foreign_marker':
    db.execute("INSERT INTO ftrace_event VALUES (14,'print',5,5)")
if case == 'reused_later':
    db.execute('INSERT INTO thread VALUES (4,42,2,101,200)')
if case == 'ambiguous':
    db.execute('INSERT INTO thread VALUES (4,42,2,0,100)')
row = db.execute(query).fetchone()
print('missing' if row is None else row[0])
