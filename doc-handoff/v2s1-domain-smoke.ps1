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
