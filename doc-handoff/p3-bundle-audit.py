"""P3 临时库 bundle 审计与硬上限回归。

只在 /tmp/riko-p3-audit 创建新库。语料用 SQL 写入可定位的 synthetic user
evidence 与 active memory，测量使用真实 memoryd HTTP；不代表模型准入质量。
运行前构建 riko-memory/target/debug/memoryd，确保 8899 端口空闲。
"""

import sqlite3, hashlib, uuid, json, pathlib, subprocess, time, urllib.request
ROOT=pathlib.Path(__file__).resolve().parents[1]; D=pathlib.Path('/tmp/riko-p3-audit'); D.mkdir(exist_ok=True)
db=D/'memory.db'; token=D/'user.token'; config=D/'config.toml'
for p in (db,token,config):
 if p.exists(): p.unlink()
subprocess.run([str(ROOT/'riko-memory/target/debug/memoryd'),'principal','add','--tenant','p3','--user','audit','--token-out',str(token),'--db',str(db),'--migrations',str(ROOT/'riko-memory/migrations')],check=True,stdout=subprocess.DEVNULL)
con=sqlite3.connect(db); con.execute('PRAGMA foreign_keys=ON'); t='2026-10-22T12:00:00Z'
subjects=['coffee','travel','rust','music','garden']; kinds=['fact','preference','instruction','episode']
claims={}
for i in range(120):
 topic=subjects[i%5]; kind=kinds[(i//5)%4]; n=i//20
 if kind=='fact': claim=f'我的 {topic} 项目编号是 P3-{i:03d}，记录在本地笔记第 {n} 页。'
 elif kind=='preference': claim=f'讨论 {topic} 时我偏好先看简短摘要，再看证据编号 P3-{i:03d}。'
 elif kind=='instruction': claim=f'回答 {topic} 问题时请先给结论，再列出处，规则编号 P3-{i:03d}。'
 else: claim=f'上周我完成了 {topic} 练习 P3-{i:03d}，并记录了复盘要点。'
 if i%10==0: claim=f"小{'甲乙丙丁戊己庚辛壬癸子丑'[i//10]}是我的朋友"
 mid=str(uuid.uuid5(uuid.NAMESPACE_URL,'p3-memory-'+str(i))); eid=str(uuid.uuid5(uuid.NAMESPACE_URL,'p3-evidence-'+str(i)))
 sha=hashlib.sha256(claim.encode()).hexdigest(); claims[mid]={'kind':kind,'topic':topic,'claim':claim,'i':i}
 con.execute('INSERT INTO evidence_events VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)',(eid,'p3','audit','p3','riko','s-'+str(i),1,'user','p3_fixture',t,t,claim,sha))
 con.execute('INSERT INTO memories (id,tenant_id,user_id,kind,claim,normalized_claim,claim_sha256,source_class,status,version,occurred_at,valid_from,valid_until,origin_host_id,origin_agent_id,created_at,updated_at,domain_id) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)',(mid,'p3','audit',kind,claim,claim,sha,'user_explicit','active',1,t,None,None,'p3','riko',t,t,'user_main'))
 con.execute('INSERT INTO memory_evidence VALUES (?,?,?,?,?,?,?)',(str(uuid.uuid4()),'p3','audit',mid,eid,0,len(claim.encode())))
 con.execute('INSERT INTO memory_fts(memory_id,claim) VALUES (?,?)',(mid,claim))
con.commit(); con.close()
config.write_text(f'listen_addr = "127.0.0.1:8899"\ndb_path = "{db}"\nmigrations_dir = "{ROOT}/riko-memory/migrations"\n[dream]\nenabled = false\n[schedule]\ndriver_enabled = false\n')
proc=subprocess.Popen([str(ROOT/'riko-memory/target/debug/memoryd'),'serve','--config',str(config)],stdout=(D/'server.log').open('w'),stderr=subprocess.STDOUT)
base='http://127.0.0.1:8899'; auth='Bearer '+token.read_text().strip()
def req(path,obj):
 b=json.dumps(obj).encode(); q=urllib.request.Request(base+path,data=b,headers={'Authorization':auth,'Content-Type':'application/json'})
 with urllib.request.urlopen(q,timeout=30) as r:return json.load(r)
try:
 for _ in range(50):
  try: urllib.request.urlopen(base+'/v1/health',timeout=1);break
  except: time.sleep(.1)
 else: raise RuntimeError('server did not start')
 for p in ['/v1/derived/refresh','/v1/relationships/refresh']: print('refresh',p,req(p,{}))
 queries=['coffee','travel','rust','music','garden','coffee 规则','travel 摘要','rust 项目','music 练习','garden 出处','P3','不存在的话题']
 rows=[]; omitted_sample=[]
 for q in queries:
  for mode,opts in [('default',{}),('wide',{'retrieved_max_items':100,'retrieved_max_chars':24000})]:
   st=time.perf_counter(); out=req('/v1/context/bundle',{'agent_id':'riko','query':q,**opts}); ms=(time.perf_counter()-st)*1000
   segs=out['segments']; om=out['retrieved']['omitted']; inc=out['retrieved']['items'];
   for x in om:
    mid=x.get('memory_id');
    if mid in claims: omitted_sample.append({'query':q,'mode':mode,'reason':x['reason'],**claims[mid]})
   rows.append({'query':q,'mode':mode,'ms':round(ms,2),'chars':[s['char_count'] for s in segs],'complete':[s['complete'] for s in segs],'omitted':[s.get('omitted',0) for s in segs],'deduped':segs[3]['deduped'],'used':out['budget']['used_chars'],'retrieved_kinds':{k:sum(1 for x in inc if x.get('memory_kind')==k) for k in kinds},'omitted_kinds':{k:sum(1 for x in om if x.get('memory_id') in claims and claims[x['memory_id']]['kind']==k) for k in kinds},'included_ids':[x.get('memory_id') for x in inc if x.get('memory_id')]})
 # A deliberately enlarged alignment segment checks the actual hard cap with the
 # same ranked corpus. This is a separate stress case, not an observed synthesis.
 with sqlite3.connect(db) as stress_db:
  stress_db.execute("INSERT INTO alignment_synthesis (tenant_id,user_id,domain_id,version,window_since,window_until,rupture_turns,user_turns,correction_free_rate,open_repair_threads,body,source_refs_json,generated_at) VALUES ('p3','audit','user_main',100,?,?,0,0,1,0,?,'{}',?)",(t,t,'A'*600,t))
 st=time.perf_counter(); stress=req('/v1/context/bundle',{'agent_id':'riko','query':'P3','retrieved_max_items':100,'retrieved_max_chars':24000});stress_ms=round((time.perf_counter()-st)*1000,2)
 assert stress['budget']['used_chars'] <= stress['budget']['total_chars']
 assert stress['segments'][3]['complete'] is False
 assert any(x['reason']=='TOTAL_CHAR_LIMIT' for x in stress['retrieved']['omitted'])
 assert stress['segments'][3]['deduped'] > 0
 assert [x['segment'] for x in stress['segments']] == ['compact','alignment','relationships','retrieved']
 assert stress['segments'][3]['omitted'] == len(stress['retrieved']['omitted'])
 assert all(x['used'] <= stress['budget']['total_chars'] for x in rows)
 (D/'raw.json').write_text(json.dumps({'rows':rows,'omitted_sample':omitted_sample,'claims':claims,'stress':{'budget':stress['budget'],'segments':[{'name':x['segment'],'char_count':x['char_count'],'complete':x['complete'],'omitted':x.get('omitted',0),'deduped':x.get('deduped',0)} for x in stress['segments']],'omission_reasons':[x['reason'] for x in stress['retrieved']['omitted']],'ms':stress_ms}},ensure_ascii=False,indent=2))
 print('stress',json.dumps({'budget':stress['budget'],'chars':[x['char_count'] for x in stress['segments']],'omitted':stress['segments'][3]['omitted'],'reasons':[x['reason'] for x in stress['retrieved']['omitted']],'deduped':stress['segments'][3]['deduped'],'ms':stress_ms},ensure_ascii=False))
 for x in rows:print(json.dumps({k:v for k,v in x.items() if k!='included_ids'},ensure_ascii=False))
finally: proc.terminate();proc.wait(timeout=5)
