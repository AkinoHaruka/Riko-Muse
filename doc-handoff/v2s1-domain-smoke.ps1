# V2-S1 临时库 HTTP 冒烟（doc7/04 §5）。只用临时目录与临时端口，不触碰 dana/realtest 原库。
$ErrorActionPreference = 'Continue'
Get-Process memoryd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500

$repo = 'C:\TRAE\Agent-Memory\agent-memory'
$tmp  = Join-Path $env:TEMP ('am-v2s1-smoke-' + [guid]::NewGuid().ToString('N').Substring(0,8))
New-Item -ItemType Directory -Path $tmp | Out-Null
Write-Output "TMP=$tmp"

$db     = Join-Path $tmp 'am.db'
$mig    = Join-Path $repo 'migrations'
$tokenF = Join-Path $tmp 'u.token'
$exe    = Join-Path $repo 'target\debug\memoryd.exe'
$dbT    = $db  -replace '\\','/'
$migT   = $mig -replace '\\','/'

& $exe principal add --tenant t --user u --token-out $tokenF --db $db --migrations $mig | Out-Null
$token = (Get-Content $tokenF -Raw).Trim()
Write-Output ("TOKEN_LEN=" + $token.Length)

function Write-Config([string]$path, [int]$port, [bool]$enabled) {
  @"
listen_addr = "127.0.0.1:$port"
db_path = "$dbT"
migrations_dir = "$migT"

[domains]
enabled = $($enabled.ToString().ToLower())
"@ | Set-Content -Encoding utf8 $path
}

function Call([string]$method, [string]$url, $body, [string]$domHeader) {
  $h = @{ Authorization = "Bearer $token" }
  if ($domHeader) { $h['X-Riko-Memory-Domain'] = $domHeader }
  try {
    if ($null -ne $body) {
      $r = Invoke-WebRequest -Method $method -Uri $url -Headers $h -ContentType 'application/json' -Body ($body | ConvertTo-Json -Compress -Depth 6) -UseBasicParsing -SkipHttpErrorCheck
    } else {
      $r = Invoke-WebRequest -Method $method -Uri $url -Headers $h -UseBasicParsing -SkipHttpErrorCheck
    }
    return @{ status = [int]$r.StatusCode; body = $r.Content }
  } catch { return @{ status = -1; body = $_.Exception.Message } }
}

function Start-Server([int]$port, [bool]$enabled, [string]$tag) {
  $cfg = Join-Path $tmp ($tag + '.toml')
  Write-Config $cfg $port $enabled
  $p = Start-Process -FilePath $exe -ArgumentList @('serve','--config',$cfg) -PassThru -RedirectStandardOutput (Join-Path $tmp ($tag + '.out')) -RedirectStandardError (Join-Path $tmp ($tag + '.err'))
  for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Milliseconds 400
    try { $r = Invoke-WebRequest -Uri "http://127.0.0.1:$port/v1/version" -UseBasicParsing -TimeoutSec 2; if ($r.StatusCode -eq 200) { return $p } } catch {}
  }
  Write-Output ("SERVER_START_FAILED_" + $tag)
  Get-Content (Join-Path $tmp ($tag + '.err')) -ErrorAction SilentlyContinue | Select-Object -First 6
  return $p
}

# ---- 阶段 1：enabled = false ----
$p1 = Start-Server 8801 $false 'off'
$b1 = 'http://127.0.0.1:8801'
$r = Call 'GET' "$b1/v1/domains" $null $null
Write-Output ("OFF_GET_DOMAINS=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b1/version" $null $null
$r = Call 'POST' "$b1/v1/domains" @{ domain_id='side_a' } $null
Write-Output ("OFF_CREATE_DOMAIN=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b1/v1/memories/none" $null 'side_a'
Write-Output ("OFF_HEADER_IGNORED=" + $r.status + " " + $r.body)
Stop-Process -Id $p1.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

# ---- 阶段 2：enabled = true ----
$p2 = Start-Server 8802 $true 'on'
$b = 'http://127.0.0.1:8802'

$r = Call 'GET' "$b/v1/domains" $null $null
Write-Output ("ON_LIST=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains" @{ domain_id='side_a'; reason='smoke' } $null
Write-Output ("ON_CREATE=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains" @{ domain_id='side_a'; reason='smoke' } $null
Write-Output ("ON_CREATE_IDEMPOTENT=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains" @{ domain_id='user_main' } $null
Write-Output ("ON_CREATE_RESERVED=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains/close" @{ domain_id='user_main' } $null
Write-Output ("ON_CLOSE_MAIN=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/domains" $null 'nonexistent_domain'
Write-Output ("ON_UNKNOWN_HEADER=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/domains" $null 'bad domain'
Write-Output ("ON_ILLEGAL_HEADER=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains/bindings" @{ host_id='dsh'; session_id='s1'; domain_id='side_a' } $null
Write-Output ("ON_BIND=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains/bindings" @{ host_id='dsh'; session_id='s1'; domain_id='user_main' } $null
Write-Output ("ON_REBIND_CONFLICT=" + $r.status + " " + $r.body)

$r = Call 'POST' "$b/v1/evidence/events" @{
  origin = @{ host_id='dsh'; agent_id='a'; session_id='s1' }
  event_seq = 1; role='user'; source_kind='user'
  occurred_at = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss.ffffffZ')
  content = '用户住在昆明'
} $null
Write-Output ("ON_INGEST=" + $r.status + " " + $r.body)
$evId = ($r.body | ConvertFrom-Json).evidence_id

$r = Call 'POST' "$b/v1/memories/remember" @{
  origin = @{ host_id='dsh'; agent_id='a'; session_id='s1' }
  user_evidence_id = $evId; quote='用户住在昆明'; kind='fact'
} $null
Write-Output ("ON_REMEMBER=" + $r.status + " " + $r.body)
$memId = ($r.body | ConvertFrom-Json).memory_id

$r = Call 'GET' "$b/v1/memories/$memId" $null 'side_a'
Write-Output ("ON_GET_SIDE=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/memories/$memId" $null 'user_main'
Write-Output ("ON_GET_MAIN_NO_LEAK=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/memories/search" @{ query='昆明' } 'user_main'
Write-Output ("ON_SEARCH_MAIN=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/memories/search" @{ query='昆明' } 'side_a'
Write-Output ("ON_SEARCH_SIDE=" + $r.status + " " + $r.body)
# V2-P1：精读端点（doc7/05 §4）
$r = Call 'GET' "$b/v1/memories/$memId/explain" $null 'side_a'
Write-Output ("P1_EXPLAIN_SIDE=" + $r.status + " " + $r.body)
$ref = ($r.body | ConvertFrom-Json).stable_ref
$enc = [uri]::EscapeDataString($ref)
$r = Call 'GET' "$b/v1/memories/$enc/explain" $null 'side_a'
Write-Output ("P1_EXPLAIN_BY_REF=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/memories/$memId/explain?history=1" $null 'side_a'
Write-Output ("P1_EXPLAIN_HISTORY=" + $r.status + " visible=" + (($r.body | ConvertFrom-Json).visible))
$r = Call 'GET' "$b/v1/memories/$memId/explain" $null 'user_main'
Write-Output ("P1_EXPLAIN_MAIN_NO_LEAK=" + $r.status + " " + $r.body)
$fake = [uri]::EscapeDataString('riko://memory/t/other-user/user_main/' + $memId + '@1')
$r = Call 'GET' "$b/v1/memories/$fake/explain" $null 'side_a'
Write-Output ("P1_EXPLAIN_FOREIGN_SCOPE=" + $r.status + " " + $r.body)

$r = Call 'GET' "$b/v1/domains/bindings" $null $null
Write-Output ("ON_BINDINGS=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/domains/grants" @{ reader_domain='user_main'; granted_domain='side_a'; reason='smoke' } $null
Write-Output ("ON_GRANT=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/memories/$memId" $null 'user_main'
Write-Output ("ON_GET_MAIN_AFTER_GRANT=" + $r.status + " " + $r.body)

# V2-D1：蒸馏视图端点（doc7/06 §5）
$r = Call 'POST' "$b/v1/derived/refresh" $null 'side_a'
Write-Output ("D1_REFRESH=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/compact" $null 'side_a'
Write-Output ("D1_COMPACT=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/facets" $null 'side_a'
Write-Output ("D1_FACETS_ALL=" + $r.status + " " + (($r.body | ConvertFrom-Json).facets.PSObject.Properties.Name -join ','))
$r = Call 'GET' "$b/v1/facets?kind=world" $null 'side_a'
Write-Output ("D1_FACET_WORLD=" + $r.status + " item_count=" + (($r.body | ConvertFrom-Json).item_count) + " skipped_stale=" + (($r.body | ConvertFrom-Json).skipped_stale))
$r = Call 'GET' "$b/v1/facets?kind=bogus" $null 'side_a'
Write-Output ("D1_FACET_BOGUS=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/compact" $null 'user_main'
# 此前的 ON_GRANT 已授权 main→side_a，所以主域此刻看得见 side 的 compact；
# 真正的越权拒绝由 ON_GET_MAIN_NO_LEAK / P1_EXPLAIN_MAIN_NO_LEAK 覆盖。
$r = Call 'GET' "$b/v1/compact" $null 'user_main'
Write-Output ("D1_COMPACT_MAIN_AFTER_GRANT=" + $r.status + " item_count=" + (($r.body | ConvertFrom-Json).item_count))

# V2-Q1：上下文条目（doc7/10 §5）。确定性切块，不调用任何模型。
$r = Call 'POST' "$b/v1/entries/refresh" @{ } 'side_a'
$er = $r.body | ConvertFrom-Json
Write-Output ("Q1_REFRESH=" + $r.status + " entries=" + $er.entries + " sources=" + $er.sources + " batch=" + $er.batch_version)
$r = Call 'POST' "$b/v1/entries/search" @{ query='昆明' } 'side_a'
$es = $r.body | ConvertFrom-Json
Write-Output ("Q1_SEARCH=" + $r.status + " count=" + $es.count + " stale=" + $es.skipped_stale + " lane=" + $es.lane + " semantic=" + $es.semantic_status)
if ($es.count -gt 0) {
  $eid = $es.hits[0].entry_id
  $r = Call 'GET' "$b/v1/entries/$eid" $null 'side_a'
  $eg = $r.body | ConvertFrom-Json
  Write-Output ("Q1_GET=" + $r.status + " sources=" + $eg.sources.Count + " generator=" + $eg.generator_version)
  Write-Output ("Q1_VERBATIM=" + ($eg.body -like '*昆明*'))
  Write-Output ("Q1_SPAN_OK=" + ($eg.sources[0].end_byte -gt 0))
  Write-Output ("Q1_ENTRY_DOMAIN=" + $eg.domain_id + " requested=side_a")
}
$r = Call 'GET' "$b/v1/entries/no-such-entry" $null 'side_a'
Write-Output ("Q1_GET_MISSING=" + $r.status)
$r = Call 'POST' "$b/v1/entries/search" @{ query='昆明' } $null
$esm = $r.body | ConvertFrom-Json
# 注意：本脚本更早处已授予主域读 side_a（跨域读取），所以这里是「授权后可见」，
# 不是域隔离失效；无授权时的隔离由 Rust 用例 entries_are_domain_scoped 断言。
Write-Output ("Q1_MAIN_AFTER_GRANT=" + $r.status + " count=" + $esm.count)

# V2-H1：bundle 分段（doc7/09 §2）
$r = Call 'POST' "$b/v1/context/bundle" @{ agent_id='a'; query='昆明' } 'side_a'
$seg = $r.body | ConvertFrom-Json
$order = ($seg.segments | ForEach-Object { $_.segment }) -join ','
$compactSeg = $seg.segments | Where-Object { $_.segment -eq 'compact' }
$relSeg = $seg.segments | Where-Object { $_.segment -eq 'relationships' }
$retSeg = $seg.segments | Where-Object { $_.segment -eq 'retrieved' }
Write-Output ("H1_BUNDLE=" + $r.status + " segments=" + $order)
Write-Output ("H1_BUDGET=" + $seg.budget.total_chars + " used=" + $seg.budget.used_chars + " within=" + ($seg.budget.used_chars -le $seg.budget.total_chars))
Write-Output ("H1_COMPACT=" + $compactSeg.char_count + " complete=" + $compactSeg.complete + " items=" + $compactSeg.items.Count)
Write-Output ("H1_RELATIONSHIPS=" + $relSeg.char_count + " complete=" + $relSeg.complete + " total=" + $relSeg.total)
Write-Output ("H1_RETRIEVED=" + $retSeg.char_count + " deduped=" + $retSeg.deduped)
Write-Output ("H1_LEGACY_KEYS=" + [bool]($seg.resident) + "/" + [bool]($seg.retrieved))

# V2-B1/A1：后台闭环（doc7/08 §5）。本段不触发任何模型调用。
$r = Call 'GET' "$b/v1/tasks/due" $null 'side_a'
$due1 = $r.body | ConvertFrom-Json
Write-Output ("B1_DUE_BEFORE=" + $r.status + " upkeep=" + $due1.tasks.upkeep.due + "/" + $due1.tasks.upkeep.reason + " quiet=" + $due1.tasks.quiet.due + "/" + $due1.tasks.quiet.reason + " nightly=" + $due1.tasks.nightly.due + "/" + $due1.tasks.nightly.reason)
$r = Call 'POST' "$b/v1/tasks/run" @{ task_kind='upkeep'; outcome='ok'; signal_count=1 } 'side_a'
Write-Output ("B1_TASK_RUN=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/tasks/run" @{ task_kind='bogus'; outcome='ok' } 'side_a'
Write-Output ("B1_TASK_RUN_BAD_KIND=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/tasks/due" $null 'side_a'
$due2 = $r.body | ConvertFrom-Json
Write-Output ("B1_DUE_AFTER_RUN=" + $r.status + " upkeep=" + $due2.tasks.upkeep.due + "/" + $due2.tasks.upkeep.reason)
$r = Call 'GET' "$b/v1/repair/actions" $null $null
Write-Output ("B1_ACTIONS_EMPTY=" + $r.status + " count=" + (($r.body | ConvertFrom-Json).count))
$r = Call 'POST' "$b/v1/repair/actions" @{ thread_id='no-such'; action='注意'; expected_behavior='小心'; proposed_by='model' } $null
Write-Output ("B1_PROPOSE_VAGUE=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/repair/actions" @{ thread_id='no-such'; action='先复述用户给的时间再回复'; expected_behavior='回复里引用用户原话的时间点'; proposed_by='model' } $null
Write-Output ("B1_PROPOSE_BAD_THREAD=" + $r.status + " " + $r.body)
$r = Call 'POST' "$b/v1/repair/actions/no-such/activate" @{ } $null
Write-Output ("B1_ACTIVATE_NO_AUTH=" + $r.status + " " + $r.body)

# V2-R1：关系图谱（doc7/07 §5）
$r = Call 'POST' "$b/v1/evidence/events" @{
  origin = @{ host_id='dsh'; agent_id='a'; session_id='s1' }
  event_seq = 2; role='user'; source_kind='user'
  occurred_at = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ss.ffffffZ')
  content = '我妻子叫小雨，她特别喜欢园艺'
} $null
$evR = ($r.body | ConvertFrom-Json).evidence_id
$r = Call 'POST' "$b/v1/memories/remember" @{
  origin = @{ host_id='dsh'; agent_id='a'; session_id='s1' }
  user_evidence_id = $evR; quote='我妻子叫小雨，她特别喜欢园艺'; kind='fact'
} $null
Write-Output ("R1_SEED_REMEMBER=" + $r.status)
$r = Call 'POST' "$b/v1/relationships/refresh" $null 'side_a'
Write-Output ("R1_REFRESH=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/relationships" $null 'side_a'
Write-Output ("R1_INDEX=" + $r.status + " total=" + (($r.body | ConvertFrom-Json).total) + " omitted=" + (($r.body | ConvertFrom-Json).omitted))
$entId = (($r.body | ConvertFrom-Json).entities | Select-Object -First 1).entity_id
Write-Output ("R1_ENTITY_ID_PRESENT=" + [bool]$entId)
$r = Call 'GET' "$b/v1/relationships/$entId" $null 'side_a'
Write-Output ("R1_GET=" + $r.status + " items=" + (($r.body | ConvertFrom-Json).items.Count) + " skipped_stale=" + (($r.body | ConvertFrom-Json).skipped_stale) + " first_section=" + (($r.body | ConvertFrom-Json).items[0].section))
$r = Call 'GET' "$b/v1/relationships/resolve?q=$([uri]::EscapeDataString('小雨'))" $null 'side_a'
Write-Output ("R1_RESOLVE_ONE=" + $r.status + " resolution=" + (($r.body | ConvertFrom-Json).resolution))
$r = Call 'GET' "$b/v1/relationships/resolve?q=nobody" $null 'side_a'
Write-Output ("R1_RESOLVE_NONE=" + $r.status + " resolution=" + (($r.body | ConvertFrom-Json).resolution))
$r = Call 'GET' "$b/v1/relationships/resolve?q=$([uri]::EscapeDataString('妻子'))" $null 'side_a'
Write-Output ("R1_RESOLVE_ROLE=" + $r.status + " resolution=" + (($r.body | ConvertFrom-Json).resolution))
$r = Call 'GET' "$b/v1/relationships" $null 'user_main'
Write-Output ("R1_INDEX_MAIN_AFTER_GRANT=" + $r.status + " total=" + (($r.body | ConvertFrom-Json).total))

# V2-D1：只读 Markdown 投影（CLI，临时目录）
$exportDir = Join-Path $tmp 'export'
& $exe export --config (Join-Path $tmp 'on.toml') --tenant t --user u --out $exportDir --domain side_a | Out-Null
$manifest = Join-Path $exportDir 't\u\side_a\manifest.json'
Write-Output ("D1_EXPORT_MANIFEST_EXISTS=" + (Test-Path $manifest))
if (Test-Path $manifest) {
  $m = Get-Content $manifest -Raw | ConvertFrom-Json
  Write-Output ("D1_EXPORT_OUTCOME=" + $m.outcome + " files=" + ($m.files | Measure-Object).Count + " batch=" + $m.batch_version)
  Write-Output ("D1_EXPORT_COMPACT_EXISTS=" + (Test-Path (Join-Path $exportDir 't\u\side_a\COMPACT.md')))
  Write-Output ("D1_EXPORT_WORLD_EXISTS=" + (Test-Path (Join-Path $exportDir 't\u\side_a\bank\world.md')))
}

$r = Call 'POST' "$b/v1/domains/close" @{ domain_id='side_a' } $null
Write-Output ("ON_CLOSE_SIDE=" + $r.status + " " + $r.body)
$r = Call 'GET' "$b/v1/domains" $null 'side_a'
Write-Output ("ON_CLOSED_HEADER=" + $r.status + " " + $r.body)

Stop-Process -Id $p2.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-Process memoryd -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
Write-Output "--- server stderr (tail) ---"
Get-Content (Join-Path $tmp 'on.err') -Tail 10 -ErrorAction SilentlyContinue
Write-Output "DONE"
