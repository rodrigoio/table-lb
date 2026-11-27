#!/bin/bash
# table-lb — single entry point to reproduce the whole study on a machine.
#
#   ./start.sh [command] [--detach]
#
# Commands (default: all)
#   setup      (root) install Docker if missing, apply kernel limits for C5
#   check      verify prerequisites and print the resolved configuration
#   build      build the images (Rust tools, pgbench, PgBouncer)
#   test       correctness tests            → results/<campaign>/tests/
#   calibrate  saturation pilot of A and B  → results/<campaign>/calibration.env
#   campaign   benchmark matrix             → results/<campaign>/<scenario>/...
#   report     summary.md, runs.csv, charts → results/<campaign>/
#   archive    results/<campaign> with raw logs → results/<campaign>.tar.gz
#   all        check, build, test, calibrate, campaign, report, archive
#   smoke      all, with short runs and small concurrency (pipeline check)
#   stop       stop a running (detached) study and clean up
#   clean      remove every table-lb container, network and volume
#
#   --detach   run in the background (nohup); output in results/<campaign>/start.log
#
# Configuration (precedence: environment > host.env > profile defaults):
#   CAMPAIGN   name of the results directory. all/smoke start a new one
#              (campaign-<UTC date-time>, smoke-<...>); the other commands
#              continue the latest one (results/latest) unless CAMPAIGN is set.
#   HOST_ENV   configuration file (default ./host.env, see host.env.example)
#   PROFILE    auto (size PostgreSQL from this host) | reference (2 vCPU VM limits)
#   FORCE      1 = resource checks only warn (run on an undersized host anyway)
#   SKIP_TESTS 1 = skip the correctness tests in 'all'
#   SCENARIOS, REPS_*, <SCENARIO>_<VAR>, PG_*, PGBOUNCER_*: see host.env.example
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

# ------------------------------------------------------------------ helpers
info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; }
warn() { printf '  \033[33mwarn\033[0m  %s\n' "$*"; WARNINGS=$((WARNINGS + 1)); }
fail() { printf '  \033[31mfail\033[0m  %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
# Resource checks (CPU, RAM, kernel limits): FORCE=1 turns failures into warnings.
short() { if [[ "${FORCE:-0}" == "1" ]]; then warn "$* (FORCE=1)"; else fail "$*"; fi; }
die()  { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
WARNINGS=0
FAILURES=0

# Sets VAR=value only when VAR is not set yet (environment wins).
default() { [[ -n "${!1+x}" ]] || export "$1=$2"; }

# Loads KEY=VALUE lines without overriding variables already set.
load_env_file() {
  local file="$1" line key val
  [[ -f "$file" ]] || return 0
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" =~ ^[[:space:]]*(#|$) ]] && continue
    if [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$ ]]; then
      key="${BASH_REMATCH[2]}"
      val="${BASH_REMATCH[3]}"
      val="${val%%[[:space:]]#*}"                       # inline comment
      val="${val%"${val##*[![:space:]]}"}"              # trailing blanks
      val="${val%\"}"; val="${val#\"}"; val="${val%\'}"; val="${val#\'}"
      default "$key" "$val"
    else
      echo "ignoring line of $file: $line" >&2
    fi
  done <"$file"
}

# Value of VAR for a scenario: <SCENARIO>_<VAR> override, else the scenario file.
scenario_value() {
  local scenario="$1" var="$2" override
  override="$(tr '[:lower:]-' '[:upper:]_' <<<"$scenario")_$var"
  if [[ -n "${!override:-}" ]]; then
    echo "${!override}"
  else
    grep -E "^$var=" "bench/scenarios/$scenario.env" 2>/dev/null | tail -1 | cut -d= -f2- | tr -d '"'
  fi
}

compose_projects() { docker compose ls -a -q 2>/dev/null | grep '^tlb-' || true; }

# ------------------------------------------------------------ configuration
CMD="all"
DETACH=0
for arg in "$@"; do
  case "$arg" in
    --detach|-d) DETACH=1 ;;
    -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
    -*) die "unknown option $arg" ;;
    *) CMD="$arg" ;;
  esac
done

case "$CMD" in
  smoke) default CAMPAIGN "smoke-$(date -u +%Y%m%d-%H%M)"; QUICK=1 ;;
  all)   default CAMPAIGN "campaign-$(date -u +%Y%m%d-%H%M)" ;;
  *)     [[ -L results/latest ]] && default CAMPAIGN "$(readlink results/latest)"
         default CAMPAIGN "campaign-$(date -u +%Y%m%d-%H%M)" ;;
esac
load_env_file "${HOST_ENV:-$ROOT/host.env}"

CPUS="$(nproc)"
MEM_MB="$(awk '/MemTotal/ { print int($2 / 1024) }' /proc/meminfo)"

# Quick profile (smoke): every run ≈ 1 min, small seed, small concurrency.
if [[ "${QUICK:-0}" == "1" ]]; then
  for s in PILOT C1 C2 C3 C5; do
    default "${s}_DURATION" 60
    default "${s}_WARMUP" 15
    default "${s}_SEED_ORDERS" 100000
  done
  default C3_BURST_AT 20
  default C3_BURST_DURATION 20
  default C5_PRODUCER_CLIENTS 2000
  default C5_PRODUCER_REPLICAS 2
  default C5_PRODUCER_THREADS 2
  default C5_PGBOUNCER_PROCESSES 2
  default C5_PGBOUNCER_POOL_SIZE 8
  default C5_LOCK_SLOTS 256
  default C5_DRAIN_TIMEOUT 600
  for r in C1 C2 C3 C4 C5 C5_VARIANTS; do default "REPS_$r" 1; done
fi

# PostgreSQL container size. "reference" = limits used on the 2 vCPU / 2 GB
# development VM (pilot numbers); "auto" = half of the CPUs and a quarter of
# the RAM of this host, the rest is left to load generators and consumers.
default PROFILE auto
if [[ "$PROFILE" == "auto" && ( "$CPUS" -lt 4 || "$MEM_MB" -lt 6000 ) ]]; then
  PROFILE=reference
fi
case "$PROFILE" in
  reference)
    default PG_CPUS 1.5
    default PG_MEM 1g
    default PG_SHARED_BUFFERS 256MB
    default PG_EFFECTIVE_CACHE_SIZE 768MB
    ;;
  auto)
    pg_mem_mb=$(( MEM_MB / 4 ))
    (( pg_mem_mb > 65536 )) && pg_mem_mb=65536
    default PG_CPUS $(( CPUS / 2 ))
    default PG_MEM "${pg_mem_mb}m"
    default PG_SHARED_BUFFERS "$(( pg_mem_mb / 4 ))MB"
    default PG_EFFECTIVE_CACHE_SIZE "$(( pg_mem_mb * 3 / 4 ))MB"
    ;;
  *) die "PROFILE must be auto or reference" ;;
esac

default SCENARIOS "c1 c2 c3 c5"
default CALIBRATION_FRACTION 0.6

# C5: one pgbench replica per 50k connections, enough PgBouncer client slots.
C5_CLIENTS="$(scenario_value c5 PRODUCER_CLIENTS)"
if [[ -z "${C5_PRODUCER_REPLICAS:-}" ]]; then
  file_replicas="$(scenario_value c5 PRODUCER_REPLICAS)"
  needed=$(( (C5_CLIENTS + 49999) / 50000 ))
  (( needed > file_replicas )) && export C5_PRODUCER_REPLICAS="$needed"
fi
C5_REPLICAS="$(scenario_value c5 PRODUCER_REPLICAS)"
default PGBOUNCER_MAX_CLIENTS $(( C5_CLIENTS > 200000 ? C5_CLIENTS : 200000 ))
export CAMPAIGN

RESULTS="$ROOT/results/$CAMPAIGN"
# Creates the campaign directory and points results/latest at it.
use_results() {
  mkdir -p "$RESULTS"
  ln -sfn "$CAMPAIGN" "$ROOT/results/latest"
}

# ---------------------------------------------------------------- detach
if [[ "$DETACH" == "1" ]]; then
  use_results
  nohup env CAMPAIGN="$CAMPAIGN" QUICK="${QUICK:-0}" TLB_DETACHED=1 "$0" "$CMD" >>"$RESULTS/start.log" 2>&1 &
  echo "started '$CMD' in the background (pid $!)"
  echo "  log:     $RESULTS/start.log"
  echo "  follow:  tail -f ${RESULTS#$ROOT/}/start.log"
  exit 0
fi

# ---------------------------------------------------------------- commands
estimate_minutes() {
  local total=0 s d runs long=0
  # long-cycle B labels (bench/campaign.sh): b-1rt-<n>s in c1/c2/c3/c5, b-<n>s in c2/c3
  (( ${LONG_CYCLE_MS:-30000} > 0 )) && long=1
  for s in $SCENARIOS; do
    d="$(scenario_value "$s" DURATION)"
    case "$s" in
      c1) runs=$(( (3 + long) * ${REPS_C1:-3} )) ;;
      c2) runs=$(( (3 + 2 * long) * ${REPS_C2:-3} + ${REPS_C4:-2} )) ;;
      c3) runs=$(( (2 + 2 * long) * ${REPS_C3:-2} )) ;;
      c5) runs=$(( (3 + long) * ${REPS_C5:-2} + ${REPS_C5_VARIANTS:-1} )) ;;
      *)  runs=2 ;;
    esac
    # + ~2 min per run: fresh database, seed, drain, teardown
    total=$(( total + runs * (${d:-60} + 120) ))
  done
  echo $(( total / 60 ))
}

cmd_check() {
  info "host"
  ok "$CPUS CPUs, $(( MEM_MB / 1024 )) GB RAM, $(uname -m), kernel $(uname -r)"
  (( CPUS >= 8 )) || warn "fewer than 8 CPUs: load generators and PostgreSQL will compete for CPU"
  (( MEM_MB >= 15000 )) || warn "less than 16 GB RAM"

  info "docker"
  if ! command -v docker >/dev/null; then
    fail "docker not found (sudo ./start.sh setup installs it)"
  elif ! docker info >/dev/null 2>&1; then
    fail "cannot talk to the Docker daemon (is it running? is $USER in the docker group?)"
  else
    ok "docker $(docker version -f '{{.Server.Version}}')"
    if docker compose version >/dev/null 2>&1; then
      ok "$(docker compose version | head -1)"
    else
      fail "docker compose v2 plugin missing"
    fi
    local root free_gb
    root="$(docker info -f '{{.DockerRootDir}}' 2>/dev/null || echo /var/lib/docker)"
    free_gb="$(df -BG --output=avail "$root" 2>/dev/null | tail -1 | tr -dc '0-9')"
    if [[ -n "$free_gb" ]]; then
      (( free_gb >= 40 )) && ok "${free_gb} GB free under $root" || warn "only ${free_gb} GB free under $root (40 GB recommended)"
    fi
    if docker run --rm --ulimit nofile=1048576:1048576 --sysctl net.ipv4.ip_local_port_range="1024 65535" \
        postgres:18 true >/dev/null 2>&1; then
      ok "containers accept nofile=1048576 and ip_local_port_range"
    else
      fail "containers cannot get nofile=1048576 / ip_local_port_range (needed by pgbench and PgBouncer)"
    fi
  fi
  command -v git >/dev/null && ok "git $(git --version | awk '{print $3}')" || warn "git not found (runs record git_rev=unknown)"
  [[ -z "$(git status --porcelain 2>/dev/null | grep -v '^?? results/' || true)" ]] || warn "working tree has local changes: runs record the HEAD revision only"

  if [[ " $SCENARIOS " == *" c5 "* ]]; then
    info "C5 — $C5_CLIENTS concurrent producer connections over $C5_REPLICAS pgbench containers"
    local per=$(( (C5_CLIENTS + C5_REPLICAS - 1) / C5_REPLICAS ))
    (( per <= 60000 )) && ok "$per connections per container" || short "$per connections per container (> 60000 ephemeral ports): raise C5_PRODUCER_REPLICAS"
    local need_mb pg_mb
    case "$PG_MEM" in *g|*G) pg_mb=$(( ${PG_MEM%[gG]} * 1024 )) ;; *m|*M) pg_mb=${PG_MEM%[mM]} ;; *) pg_mb=1024 ;; esac
    need_mb=$(( C5_CLIENTS * 48 / 1024 + pg_mb + 2048 ))
    (( MEM_MB >= need_mb )) && ok "RAM: ~${need_mb} MB needed (≈48 KB per connection in pgbench + PgBouncer, PostgreSQL, consumers)" \
      || short "RAM: ~${need_mb} MB needed, $MEM_MB MB available: lower C5_PRODUCER_CLIENTS"
    local conntrack file_max
    conntrack="$(cat /proc/sys/net/netfilter/nf_conntrack_max 2>/dev/null || echo)"
    if [[ -n "$conntrack" ]]; then
      (( conntrack >= 2 * C5_CLIENTS )) && ok "nf_conntrack_max $conntrack" \
        || short "nf_conntrack_max $conntrack < $(( 2 * C5_CLIENTS )): connections will be dropped (sudo ./start.sh setup)"
    fi
    file_max="$(cat /proc/sys/fs/file-max)"
    (( file_max >= 4 * C5_CLIENTS )) && ok "fs.file-max $file_max" || short "fs.file-max $file_max < $(( 4 * C5_CLIENTS )) (sudo ./start.sh setup)"
  fi

  info "configuration"
  echo "  campaign      results/$CAMPAIGN"
  echo "  profile       $PROFILE (PostgreSQL: cpus=$PG_CPUS mem=$PG_MEM shared_buffers=$PG_SHARED_BUFFERS effective_cache_size=$PG_EFFECTIVE_CACHE_SIZE)"
  echo "  scenarios     $SCENARIOS   (reps c1=${REPS_C1:-3} c2=${REPS_C2:-3} c4=${REPS_C4:-2} c3=${REPS_C3:-2} c5=${REPS_C5:-2}+${REPS_C5_VARIANTS:-1})"
  echo "  long cycle    $( (( ${LONG_CYCLE_MS:-30000} > 0 )) && echo "B labels with MIN_CYCLE_MS=${LONG_CYCLE_MS:-30000} (b-$(( ${LONG_CYCLE_MS:-30000} / 1000 ))s, b-1rt-$(( ${LONG_CYCLE_MS:-30000} / 1000 ))s)" || echo "off (LONG_CYCLE_MS=0)")"
  echo "  c5            clients=$C5_CLIENTS replicas=$C5_REPLICAS pgbouncer=$(scenario_value c5 PGBOUNCER_PROCESSES)×$(scenario_value c5 PGBOUNCER_POOL_SIZE) lock_slots=$(scenario_value c5 LOCK_SLOTS)"
  echo "  calibration   C2/C3 rate = ${CALIBRATION_FRACTION} × min(A, B) pilot throughput$( [[ -n "${C2_PRODUCER_RATE:-}" ]] && echo " — fixed: C2_PRODUCER_RATE=$C2_PRODUCER_RATE")"
  echo "  estimate      campaign ≈ $(estimate_minutes) min (+ ~15 min tests, ~6 min calibration)"
  local overrides
  overrides="$(compgen -e | grep -E '^(PILOT|C[0-9]+)_' | sort | while read -r v; do printf '%s=%s ' "$v" "${!v}"; done)"
  [[ -z "$overrides" ]] || echo "  overrides     $overrides"

  if (( FAILURES > 0 )); then
    die "$FAILURES check(s) failed"
  fi
  (( WARNINGS == 0 )) || echo "  ($WARNINGS warning(s): results remain valid for A × B comparison, absolute numbers depend on the host)"
}

cmd_build() {
  info "building images (first build compiles the Rust workspace: a few minutes)"
  docker pull -q postgres:18 >/dev/null
  docker compose -f project-a/docker-compose.yml --profile load --profile burst build
  docker compose -f project-b/docker-compose.yml --profile load --profile burst build
  ok "images: $(docker images --format '{{.Repository}}' | grep -E '^tlb-(rust|pgbench|pgbouncer)$' | sort | tr '\n' ' ')"
}

write_host_info() {
  use_results
  {
    echo "date=$(date -u +%FT%TZ)"
    echo "git_rev=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "cpu_model=$(awk -F': ' '/model name/ { print $2; exit }' /proc/cpuinfo)"
    echo "cpus=$CPUS"
    echo "mem_mb=$MEM_MB"
    echo "arch=$(uname -m)"
    echo "kernel=$(uname -r)"
    echo "docker=$(docker version -f '{{.Server.Version}}' 2>/dev/null)"
    echo "compose=$(docker compose version --short 2>/dev/null)"
    echo "profile=$PROFILE"
    for v in PG_CPUS PG_MEM PG_SHARED_BUFFERS PG_EFFECTIVE_CACHE_SIZE SCENARIOS \
             REPS_C1 REPS_C2 REPS_C3 REPS_C4 REPS_C5 REPS_C5_VARIANTS LONG_CYCLE_MS QUICK; do
      [[ -n "${!v:-}" ]] && echo "$v=${!v}"
    done
    compgen -e | grep -E '^(PILOT|C[0-9]+)_' | sort | while read -r v; do echo "$v=${!v}"; done
  } >"$RESULTS/host.txt"
}

cmd_test() {
  info "correctness tests → results/$CAMPAIGN/tests/"
  use_results
  mkdir -p "$RESULTS/tests"
  if TEST_RESULTS="$RESULTS/tests" tests/run-all.sh; then
    ok "all correctness tests passed"
  else
    die "a correctness test failed (see results/$CAMPAIGN/tests/*/summary.txt); set SKIP_TESTS=1 to benchmark anyway"
  fi
}

pilot_tps() {
  awk '/^tps = / { print int($3); exit }' "$RESULTS/pilot/$1/rep-1/raw/producer.out" 2>/dev/null
}

cmd_calibrate() {
  local cal="$RESULTS/calibration.env"
  if [[ -n "${C2_PRODUCER_RATE:-}" ]]; then
    info "calibration skipped: C2_PRODUCER_RATE=$C2_PRODUCER_RATE is set"
    return
  fi
  if [[ -f "$cal" && "${RECALIBRATE:-0}" != "1" ]]; then
    info "calibration: reusing ${cal#$ROOT/}"
    return
  fi
  info "calibration: saturation pilot of A and B ($(scenario_value pilot DURATION) s each)"
  write_host_info
  bench/run.sh a pilot 1 2>&1 | grep -v '^\s*Image ' | tee -a "$RESULTS/campaign.log"
  bench/run.sh b pilot 1 2>&1 | grep -v '^\s*Image ' | tee -a "$RESULTS/campaign.log"
  local a b min rate
  a="$(pilot_tps a)"; b="$(pilot_tps b)"
  [[ -n "$a" && -n "$b" ]] || die "calibration failed: no pgbench throughput in results/$CAMPAIGN/pilot/*/rep-1/raw/producer.out"
  min=$(( a < b ? a : b ))
  rate="$(awk -v m="$min" -v f="$CALIBRATION_FRACTION" 'BEGIN { r = int(m * f / 100) * 100; print (r < 100 ? 100 : r) }')"
  {
    echo "# Calibrated $(date -u +%FT%TZ): pilot saturation throughput A=$a tps, B=$b tps."
    echo "# Sustained rate = $CALIBRATION_FRACTION × min(A, B); burst adds 2× that rate (3× in total)."
    echo "C2_PRODUCER_RATE=$rate"
    echo "C3_PRODUCER_RATE=$rate"
    echo "C3_BURST_RATE=$(( 2 * rate ))"
  } >"$cal"
  ok "A=$a tps, B=$b tps → C2/C3 rate $rate tps, C3 burst $(( 2 * rate )) tps (${cal#$ROOT/})"
}

cmd_campaign() {
  load_env_file "$RESULTS/calibration.env"
  write_host_info
  info "campaign $CAMPAIGN: $SCENARIOS (≈ $(estimate_minutes) min) → results/$CAMPAIGN/"
  NO_REPORT=1 bench/campaign.sh
}

cmd_report() {
  info "report → results/$CAMPAIGN/{summary.md,runs.csv,charts/}"
  docker run --rm -u "$(id -u):$(id -g)" -v "$ROOT/results:/results" tlb-rust:latest \
    /usr/local/bin/report "/results/$CAMPAIGN"
}

cmd_archive() {
  [[ -d "$RESULTS" ]] || die "nothing to archive: results/$CAMPAIGN does not exist"
  info "archive → results/$CAMPAIGN.tar.gz"
  tar -C "$ROOT/results" -czf "$ROOT/results/$CAMPAIGN.tar.gz" "$CAMPAIGN"
  ok "$(du -h "$ROOT/results/$CAMPAIGN.tar.gz" | cut -f1) (raw logs included)"
}

cmd_clean() {
  info "removing table-lb stacks (containers, networks, volumes)"
  local p
  for p in $(compose_projects); do
    docker compose -p "$p" --profile load --profile burst down -v --remove-orphans >/dev/null 2>&1 || true
    ok "$p"
  done
}

cmd_stop() {
  info "stopping start.sh / bench / tests processes"
  local pids
  pids="$(pgrep -f 'start\.sh( (all|smoke|calibrate|campaign|test|-d|--detach))*$|bench/(campaign|run)\.sh|tests/[a-z0-9-]+\.sh' || true)"
  if [[ -n "$pids" ]]; then
    # shellcheck disable=SC2086
    kill $pids 2>/dev/null || true
    ok "stopped: $(tr '\n' ' ' <<<"$pids")"
    sleep 3
  else
    ok "nothing running"
  fi
  cmd_clean
}

cmd_setup() {
  [[ "$(id -u)" == "0" ]] || die "setup changes the system: run it as root (sudo ./start.sh setup)"
  info "docker"
  if command -v docker >/dev/null; then
    ok "docker already installed: $(docker --version)"
  else
    # Official convenience script (Debian, Ubuntu, Fedora, CentOS, RHEL, ...).
    curl -fsSL https://get.docker.com | sh
    systemctl enable --now docker 2>/dev/null || true
  fi
  if [[ -n "${SUDO_USER:-}" ]] && ! id -nG "$SUDO_USER" | grep -qw docker; then
    usermod -aG docker "$SUDO_USER"
    warn "$SUDO_USER added to the docker group: log out and in again"
  fi
  info "kernel limits for hundreds of thousands of connections (/etc/sysctl.d/99-table-lb.conf)"
  modprobe nf_conntrack 2>/dev/null || true
  cat >/etc/sysctl.d/99-table-lb.conf <<'EOF'
# table-lb benchmark host (C5: hundreds of thousands of TCP connections)
fs.file-max = 4194304
net.netfilter.nf_conntrack_max = 1048576
net.core.somaxconn = 65535
net.ipv4.tcp_max_syn_backlog = 65535
net.ipv4.ip_local_port_range = 1024 65535
EOF
  sysctl -q -p /etc/sysctl.d/99-table-lb.conf
  ok "applied: $(grep -v '^#' /etc/sysctl.d/99-table-lb.conf | tr '\n' ' ')"
}

run_all() {
  local quick="${QUICK:-0}"
  info "table-lb: '$CMD' → results/$CAMPAIGN/ (started $(date -u +%FT%TZ))"
  cmd_check
  cmd_build
  if [[ "${SKIP_TESTS:-0}" != "1" && "$quick" != "1" ]]; then cmd_test; fi
  cmd_calibrate
  cmd_campaign
  cmd_report
  cmd_archive
  info "finished $(date -u +%FT%TZ)"
  echo "  report:   results/$CAMPAIGN/summary.md"
  echo "  charts:   results/$CAMPAIGN/charts/"
  echo "  archive:  results/$CAMPAIGN.tar.gz"
}

case "$CMD" in
  setup)     cmd_setup ;;
  check)     cmd_check ;;
  build)     cmd_build ;;
  test)      cmd_test ;;
  calibrate) cmd_calibrate ;;
  campaign)  cmd_campaign ;;
  report)    cmd_report ;;
  archive)   cmd_archive ;;
  clean)     cmd_clean ;;
  stop)      cmd_stop ;;
  all|smoke)
    use_results
    if [[ "${TLB_DETACHED:-0}" == "1" ]]; then   # output already goes to start.log
      run_all
    else
      run_all 2>&1 | tee -a "$RESULTS/start.log"
      exit "${PIPESTATUS[0]}"
    fi ;;
  *) die "unknown command '$CMD' (./start.sh --help)" ;;
esac
