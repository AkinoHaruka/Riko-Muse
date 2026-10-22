#!/usr/bin/env python3
"""P5b semantic challenge evaluation on admit_v5.
Runs the 24 cases in doc-handoff/p5-stop/semantic-challenge.tsv.
Each case is evaluated in isolation with its own user/session and fresh window.
No credentials written to disk/logs.
"""
import concurrent.futures
import csv
import datetime
import json
import os
import pathlib
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.request
from collections import Counter

ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURE = ROOT.parent / 'doc-handoff/p5-stop/semantic-challenge.tsv'
EXE = ROOT / 'target/debug/memoryd'
OUT = pathlib.Path(os.environ.get('P5B_OUTPUT_DIR', str(ROOT.parent / 'doc-handoff/p5b-baseline')))

with FIXTURE.open(encoding='utf-8') as f:
    CASES = list(csv.DictReader(f, delimiter='\t'))
assert len(CASES) == 24 and len({c['id'] for c in CASES}) == 24


def run_model(label, endpoint, model, key, extra=''):
    with tempfile.TemporaryDirectory(prefix=f'riko-p5b-sc-{label}-') as td:
        p = pathlib.Path(td)
        db, token, cfg, model_key, log = (p / n for n in ('memory.db', 'token', 'config.toml', 'model.key', 'server.log'))
        model_key.write_text(key + '\n')
        model_key.chmod(0o600)
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
        cfg.write_text(
            f'listen_addr = "127.0.0.1:{port}"\n'
            f'db_path = "{db}"\nmigrations_dir = "{ROOT / "migrations"}"\n'
            f'model_endpoint = "{endpoint}"\nmodel_name = "{model}"\n'
            f'model_key_file = "{model_key}"\nmodel_max_tokens = 4096\nmodel_timeout_secs = 120\n'
            f'{extra}\n[dream]\nenabled = false\n[generation]\nenabled = false\n'
            '[schedule]\ndriver_enabled = false\n'
        )
        # Register a principal for this model run
        subprocess.run([str(EXE), 'principal', 'add', '--tenant', 'p5b', '--user', label,
                        '--token-out', str(token), '--db', str(db), '--migrations', str(ROOT / 'migrations')],
                       check=True, capture_output=True, text=True)
        auth = token.read_text().strip()

        def call(method, path, payload=None):
            data = None if payload is None else json.dumps(payload, ensure_ascii=False).encode()
            req = urllib.request.Request(f'http://127.0.0.1:{port}{path}', data=data, method=method,
                                         headers={'Authorization': 'Bearer ' + auth, 'Content-Type': 'application/json'})
            with urllib.request.urlopen(req, timeout=10) as response:
                return json.load(response)

        event_ids = {}
        with log.open('w') as lf:
            server = subprocess.Popen([str(EXE), 'serve', '--config', str(cfg)], cwd=ROOT,
                                      stdout=lf, stderr=subprocess.STDOUT)
            try:
                for _ in range(100):
                    try:
                        call('GET', '/v1/health')
                        break
                    except Exception:
                        time.sleep(.1)
                    else:
                        raise RuntimeError('memoryd did not start: ' + log.read_text()[-1000:])

                for case in CASES:
                    cid = case['id']
                    session = f'sc-{cid}'
                    seq = 1
                    if case['context']:
                        origin = {'host_id': 'p5b', 'agent_id': 'challenge', 'session_id': session}
                        call('POST', '/v1/evidence/events', {
                            'origin': origin, 'event_seq': seq, 'role': 'user', 'source_kind': 'user',
                            'occurred_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                            'content': case['context']})
                        seq += 1

                    origin = {'host_id': 'p5b', 'agent_id': 'challenge', 'session_id': session}
                    ev = call('POST', '/v1/evidence/events', {
                        'origin': origin, 'event_seq': seq, 'role': 'user', 'source_kind': 'user',
                        'occurred_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                        'content': case['text']})
                    event_ids[cid] = (session, ev['evidence_id'])
                    call('POST', '/v1/extraction/flush', {
                        'host_id': 'p5b', 'session_id': session, 'through_event_seq': seq})

                con = sqlite3.connect(db)
                deadline = time.monotonic() + 1800
                while time.monotonic() < deadline:
                    statuses = Counter(dict(con.execute('SELECT status, COUNT(*) FROM extraction_jobs GROUP BY status')))
                    finished = statuses['succeeded'] + statuses['dead']
                    if finished == 24:
                        break
                    time.sleep(2)
                else:
                    raise RuntimeError(f'timed out: {statuses}; server log tail: {log.read_text()[-800:]}')

                jobs = [dict(zip(('session', 'status', 'attempts', 'error_code', 'prompt_version', 'admission_version'), row))
                        for row in con.execute('SELECT session_id,status,attempts,error_code,prompt_version,admission_version FROM extraction_jobs')]
                rows = {}
                for case in CASES:
                    cid = case['id']
                    session, evid = event_ids[cid]
                    candidates = [dict(zip(('kind', 'quote', 'claim', 'status', 'reason_code'), r))
                                  for r in con.execute('SELECT kind,quote,claim,status,reason_code FROM memory_candidates '
                                                       'WHERE job_id IN (SELECT id FROM extraction_jobs WHERE session_id=?) ORDER BY created_at,id',
                                                       (session,))]
                    active = [x for x in candidates if x['reason_code'] == 'PROMOTED' or x['status'] == 'candidate']
                    actual = 'active' if active else 'held' if any(x['status'] == 'held' for x in candidates) else 'reject'
                    leak = case['bucket'] == 'policy' and actual == 'active'
                    rows[cid] = {
                        **case,
                        'actual': actual,
                        'leak': leak,
                        'sensitive_held': any(x['status'] == 'held' and x['reason_code'] == 'SENSITIVE' for x in candidates),
                        'candidates': candidates,
                    }

                held = Counter(x['reason_code'] for row in rows.values() for x in row['candidates'] if x['status'] == 'held')
                leaks = [cid for cid, row in rows.items() if row['leak']]
                false_positives = [cid for cid, row in rows.items() if row['bucket'] == 'body' and row['sensitive_held']]
                result = {
                    'model': model,
                    'endpoint': endpoint,
                    'schema': con.execute('SELECT MAX(version) FROM schema_migrations').fetchone()[0],
                    'jobs': jobs,
                    'counts': dict(Counter(row['actual'] for row in rows.values())),
                    'held_reasons': dict(held),
                    'leaks': leaks,
                    'false_positives': false_positives,
                    'cases': rows,
                }
                OUT.mkdir(parents=True, exist_ok=True)
                (OUT / f'semantic_challenge_{label}.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
                return label, result
            finally:
                server.terminate()
                server.wait(timeout=15)


if __name__ == '__main__':
    if not EXE.exists():
        raise SystemExit('build memoryd first')
    models = [
        ('gemini', os.environ['GEMINI_ENDPOINT'], os.environ['GEMINI_MODEL'], os.environ['GEMINI_API_KEY'], ''),
        ('siliconflow', os.environ['SILICONFLOW_ENDPOINT'].rstrip('/') + '/chat/completions',
         os.environ['SILICONFLOW_MODEL'], os.environ['SILICONFLOW_API_KEY'],
         'model_extra_json = { enable_thinking = false }')]
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        for label, result in pool.map(lambda args: run_model(*args), models):
            print(label, 'jobs', dict(Counter(j['status'] for j in result['jobs'])),
                  'counts', result['counts'], 'held', result['held_reasons'],
                  'leaks', result['leaks'], 'false_positives', result['false_positives'], flush=True)
