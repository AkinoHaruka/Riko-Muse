import datetime, json, pathlib, socket, sqlite3, subprocess, tempfile, time, urllib.request
root=pathlib.Path(__file__).resolve().parent.parent / 'riko-memory'
exe=root/'target/debug/memoryd'
with tempfile.TemporaryDirectory(prefix='riko-p2-smoke-') as td:
    p=pathlib.Path(td); db=p/'memory.db'; token=p/'token'; cfg=p/'config.toml'; log=p/'server.log'
    with socket.socket() as s:
        s.bind(('127.0.0.1',0)); port=s.getsockname()[1]
    cfg.write_text(f'listen_addr = "127.0.0.1:{port}"\ndb_path = "{db}"\nmigrations_dir = "{root / "migrations"}"\n[dream]\nenabled = false\n[generation]\nenabled = false\n[schedule]\ndriver_enabled = true\ndriver_interval_secs = 1\nupkeep_interval_secs = 1\nrelationships_interval_secs = 1\nquiet_idle_secs = 1\nquiet_max_per_day = 3\nnightly_enabled = false\n')
    subprocess.run([str(exe),'principal','add','--tenant','smoke','--user','p2','--token-out',str(token),'--db',str(db),'--migrations',str(root/'migrations')],check=True,capture_output=True,text=True)
    auth=token.read_text().strip()
    def call(method,path,payload=None):
        data=None if payload is None else json.dumps(payload).encode()
        req=urllib.request.Request(f'http://127.0.0.1:{port}{path}',data=data,method=method,headers={'Authorization':'Bearer '+auth,'Content-Type':'application/json'})
        with urllib.request.urlopen(req,timeout=5) as r: return json.load(r)
    with log.open('w') as f:
        server=subprocess.Popen([str(exe),'serve','--config',str(cfg)],cwd=root,stdout=f,stderr=subprocess.STDOUT)
        try:
            for _ in range(100):
                try:
                    call('GET','/v1/health'); break
                except Exception: time.sleep(.1)
            else: raise RuntimeError('server did not start: '+log.read_text()[-1000:])
            text='我妻子叫小雨'
            origin={'host_id':'dsh','agent_id':'a','session_id':'s1'}
            ev=call('POST','/v1/evidence/events',{'origin':origin,'event_seq':1,'role':'user','source_kind':'user','occurred_at':datetime.datetime.now(datetime.timezone.utc).isoformat(),'content':text})
            mem=call('POST','/v1/memories/remember',{'origin':origin,'user_evidence_id':ev['evidence_id'],'quote':text,'kind':'fact'})
            print('INGEST',ev['status'],'REMEMBER',mem['status'])
            status=None
            for _ in range(100):
                status=call('GET','/v1/tasks/status')
                kinds={r['task_kind'] for r in status['runs'] if r['outcome'] in ('ok','empty')}
                if {'upkeep','relationships','quiet'} <= kinds: break
                time.sleep(.2)
            else: print('WAIT_TIMEOUT',[(r['task_kind'],r['outcome'],r['detail']) for r in status['runs']]); print('SERVER_LOG',log.read_text()[-1500:])
            con=sqlite3.connect(db)
            for table in ['task_runs','processing_ledger','derived_items','relationship_entities','context_entries','task_claims']:
                print('COUNT',table,con.execute('select count(*) from '+table).fetchone()[0])
            print('RUNS',[(r['task_kind'],r['outcome'],r['detail']) for r in status['runs']])
            before=con.execute('select count(*) from task_runs').fetchone()[0]
            time.sleep(2.2)
            after=con.execute('select count(*) from task_runs').fetchone()[0]
            print('NO_DUPLICATE_RUNS',before,after)
            assert {'upkeep','relationships','quiet'} <= {r['task_kind'] for r in status['runs'] if r['outcome'] in ('ok','empty')}
            assert con.execute('select count(*) from derived_items').fetchone()[0] >= 1
            assert con.execute('select count(*) from relationship_entities').fetchone()[0] >= 1
            assert con.execute('select count(*) from context_entries').fetchone()[0] >= 1
            assert con.execute('select count(*) from processing_ledger where task_kind="upkeep"').fetchone()[0] == 1
            assert con.execute('select count(*) from processing_ledger where task_kind="quiet"').fetchone()[0] == 1
            assert before==after
            server.terminate(); server.wait(timeout=10)
            server=subprocess.Popen([str(exe),'serve','--config',str(cfg)],cwd=root,stdout=f,stderr=subprocess.STDOUT)
            time.sleep(2.2)
            restarted=con.execute('select count(*) from task_runs').fetchone()[0]
            print('RESTART_NO_DUPLICATE_RUNS',before,restarted)
            assert restarted==before
            con.close()
        finally:
            server.terminate(); server.wait(timeout=10)
