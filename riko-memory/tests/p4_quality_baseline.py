#!/usr/bin/env python3
"""P4 real-model baseline. Runs only on fresh temporary databases; no credentials in output."""
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
FIXTURE = ROOT / 'tests/fixtures/p4_extract_quality.tsv'
EXE = ROOT / 'target/debug/memoryd'
OUT = pathlib.Path(os.environ.get('P4_OUTPUT_DIR', str(ROOT.parent / 'doc-handoff/p4-baseline')))
with FIXTURE.open(encoding='utf-8') as f:
    CASES = list(csv.DictReader(f, delimiter='\t'))
assert len(CASES) == 120 and len({c['id'] for c in CASES}) == 120


def run_model(label, endpoint, model, key, extra=''):
    with tempfile.TemporaryDirectory(prefix=f'riko-p4-{label}-') as td:
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
        subprocess.run([str(EXE), 'principal', 'add', '--tenant', 'p4', '--user', label,
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
                for window_idx in range(15):
                    session = f'p4-{window_idx:02d}'
                    for seq, case in enumerate(CASES[window_idx * 8:(window_idx + 1) * 8], 1):
                        origin = {'host_id': 'p4', 'agent_id': 'quality', 'session_id': session}
                        ev = call('POST', '/v1/evidence/events', {
                            'origin': origin, 'event_seq': seq, 'role': 'user', 'source_kind': 'user',
                            'occurred_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                            'content': case['text']})
                        event_ids[case['id']] = ev['evidence_id']
                    call('POST', '/v1/extraction/flush', {
                        'host_id': 'p4', 'session_id': session, 'through_event_seq': 8})
                con = sqlite3.connect(db)
                deadline = time.monotonic() + 1800
                while time.monotonic() < deadline:
                    statuses = Counter(dict(con.execute('SELECT status, COUNT(*) FROM extraction_jobs GROUP BY status')))
                    finished = statuses['succeeded'] + statuses['dead']
                    if finished == 15:
                        break
                    time.sleep(2)
                else:
                    raise RuntimeError(f'timed out: {statuses}; server log tail: {log.read_text()[-800:]}')
                jobs = [dict(zip(('session', 'status', 'attempts', 'error_code', 'prompt_version', 'admission_version'), row))
                        for row in con.execute('SELECT session_id,status,attempts,error_code,prompt_version,admission_version FROM extraction_jobs')]
                rows = {}
                for case in CASES:
                    candidates = [dict(zip(('kind', 'quote', 'claim', 'status', 'reason_code'), r))
                                  for r in con.execute('SELECT kind,quote,claim,status,reason_code FROM memory_candidates '
                                                       'WHERE primary_evidence_id=? ORDER BY created_at,id',
                                                       (event_ids[case['id']],))]
                    # Newly promoted rows become rejected/PROMOTED. An admitted
                    # duplicate-active row stays candidate and links its evidence
                    # to the existing active memory (jobs.rs rule 8a).
                    active = [x for x in candidates if x['reason_code'] == 'PROMOTED' or x['status'] == 'candidate']
                    actual = 'active' if active else 'held' if any(x['status'] == 'held' for x in candidates) else 'reject'
                    points = [x for x in case['claim_points'].split(';') if x]
                    claims = ' '.join(x['claim'] for x in active).lower()
                    points_match = all(any(alt.lower() in claims for alt in x.split('/')) for x in points) if active else False
                    kind_match = any(x['kind'] == case['kind'] for x in active)
                    expected = case['expected']
                    mismatch = expected != 'disputed' and (actual != expected or expected == 'active' and not (points_match and kind_match))
                    rows[case['id']] = {**case, 'actual': actual, 'points_match': points_match,
                                        'kind_match': kind_match, 'mismatch': bool(mismatch), 'candidates': candidates}
                held = Counter(x['reason_code'] for row in rows.values() for x in row['candidates'] if x['status'] == 'held')
                leaks = [case_id for case_id, row in rows.items()
                         if row['bucket'] in ('third_party', 'policy') and row['actual'] == 'active']
                result = {'model': model, 'endpoint': endpoint, 'schema': con.execute('SELECT MAX(version) FROM schema_migrations').fetchone()[0],
                          'jobs': jobs, 'counts': dict(Counter(row['actual'] for row in rows.values())),
                          'held_reasons': dict(held), 'policy_leaks': leaks,
                          'mismatches': [k for k, v in rows.items() if v['mismatch']],
                          'disputed': [k for k, v in rows.items() if v['expected'] == 'disputed'], 'cases': rows}
                OUT.mkdir(parents=True, exist_ok=True)
                (OUT / f'{label}.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
                return label, result
            finally:
                server.terminate()
                server.wait(timeout=15)


def recount_file(path):
    """Recompute summaries from saved candidate rows without another model call."""
    result = json.loads(path.read_text())
    annotations = {case['id']: case for case in CASES}
    for case_id, row in result['cases'].items():
        assert row['text'] == annotations[case_id]['text'], 'corpus text changed after model run'
        row.update(annotations[case_id])
        active = [x for x in row['candidates'] if x['reason_code'] == 'PROMOTED' or x['status'] == 'candidate']
        row['actual'] = 'active' if active else 'held' if any(x['status'] == 'held' for x in row['candidates']) else 'reject'
        points = [x for x in row['claim_points'].split(';') if x]
        claims = ' '.join(x['claim'] for x in active).lower()
        row['points_match'] = bool(active) and all(any(alt.lower() in claims for alt in x.split('/')) for x in points)
        row['kind_match'] = any(x['kind'] == row['kind'] for x in active)
        row['mismatch'] = row['expected'] != 'disputed' and (
            row['actual'] != row['expected'] or row['expected'] == 'active' and not (row['points_match'] and row['kind_match']))
    rows = result['cases']
    result['counts'] = dict(Counter(row['actual'] for row in rows.values()))
    result['policy_leaks'] = [k for k, row in rows.items()
                              if row['bucket'] in ('third_party', 'policy') and row['actual'] == 'active']
    result['mismatches'] = [k for k, row in rows.items() if row['mismatch']]
    path.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    return result


if __name__ == '__main__':
    if '--recount' in sys.argv:
        for path in sorted(OUT.glob('*.json')):
            result = recount_file(path)
            print(path.name, result['counts'], 'policy_leaks', result['policy_leaks'])
        raise SystemExit(0)
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
                  'policy_leaks', result['policy_leaks'], 'mismatches', len(result['mismatches']), flush=True)
