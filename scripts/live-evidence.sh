#!/usr/bin/env bash
# Live acceptance run for n2link that keeps every output as evidence.
#
#   scripts/live-evidence.sh                 # all steps
#   scripts/live-evidence.sh branding compat # only some steps
#
# Steps: env build branding compat credentials wasm tests
# Evidence: artifacts/live-evidence/<UTC time>-<commit>/ (override with N2LINK_EVIDENCE_DIR)
#   NN-<step>/...   raw outputs, logs, HTTP bodies and status codes
#   results.tsv     one line per check: step, check, PASS/FAIL, detail
#   SUMMARY.md      human-readable result table
#   SHA256SUMS      hashes of every evidence file
#
# Every server runs from a fresh temporary home on its own port (default 18890), so a server you
# already have running and your own ~/.n2linkd are not touched. Secrets are never written to the
# evidence: the credential key is read from .env (N2LINK_CREDENTIAL_KEY) or generated, and the
# test password is random and only checked for absence.
# Set N2LINK_EVIDENCE_PYTEST=/path/to/python to include the pytest suite in the `tests` step.

set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT" || exit 1

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
COMMIT="$(git rev-parse --short HEAD 2>/dev/null || echo nogit)"
OUT="${N2LINK_EVIDENCE_DIR:-artifacts/live-evidence/$STAMP-$COMMIT}"
PORT="${N2LINK_EVIDENCE_PORT:-18890}"
STEPS="${*:-env build branding compat credentials wasm tests}"
BIN="$ROOT/target/debug/n2linkd"
WASM_TARGET="$ROOT/target/evidence-wasm"
WASM_BIN="$WASM_TARGET/debug/n2linkd"
LEGACY_PLUGIN="${N2LINK_EVIDENCE_LEGACY_PLUGIN:-$ROOT/../edgelinkd/target/wasm-examples/uppercase.wasm}"

# The run must not inherit names from the caller's shell.
for var in $(env | grep -o -E '^(N2LINK|EDGELINK)_[A-Z_]+' | sort -u); do
  case "$var" in N2LINK_EVIDENCE_*) ;; *) unset "$var" ;; esac
done

mkdir -p "$OUT"
# Absolute, because some steps run commands from inside temporary directories.
OUT="$(cd "$OUT" && pwd)"
RESULTS="$OUT/results.tsv"
printf 'step\tcheck\tresult\tdetail\n' > "$RESULTS"
SERVER_PID=""
TMP_ROOT="$(mktemp -d)"
trap 'stop_server; rm -rf "$TMP_ROOT"' EXIT

say() { printf '\n== %s\n' "$*"; }
record() { printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$3" "${4:-}" >> "$RESULTS"; printf '  [%s] %s %s\n' "$3" "$2" "${4:-}"; }
check_exit() { if [ "$3" -eq 0 ]; then record "$1" "$2" PASS "exit 0"; else record "$1" "$2" FAIL "exit $3"; fi; }
check_grep() { # step check file regex
  if grep -q -E -- "$4" "$3" 2>/dev/null; then record "$1" "$2" PASS "matches /$4/ in $(basename "$3")"
  else record "$1" "$2" FAIL "no /$4/ in $(basename "$3")"; fi
}
check_no_grep() { # step check file literal
  if grep -q -F -- "$4" "$3" 2>/dev/null; then record "$1" "$2" FAIL "found forbidden text in $(basename "$3")"
  else record "$1" "$2" PASS "absent from $(basename "$3")"; fi
}
run_logged() { # dir name cmd... ; writes <name>.log (stdout+stderr) and <name>.exit
  local dir="$1" name="$2"; shift 2
  { printf '$'; printf ' %q' "$@"; printf '\n'; } > "$dir/$name.cmd"
  "$@" > "$dir/$name.log" 2>&1
  local rc=$?
  echo "$rc" > "$dir/$name.exit"
  return "$rc"
}
http_get() { # url file ; body to file, status to file.status
  curl -s -o "$2" -w '%{http_code} %{content_type}\n' "http://127.0.0.1:$PORT$1" > "$2.status"
}
http_post_json() { # path json-file out-file
  curl -s -o "$3" -w '%{http_code}\n' -X POST -H 'Content-Type: application/json' \
    -H 'Node-RED-API-Version: v2' -H 'Node-RED-Deployment-Type: full' \
    --data-binary @"$2" "http://127.0.0.1:$PORT$1" > "$3.status"
}
start_server() { # bin home log
  N2LINK_HOME="$2" "$1" run --bind "127.0.0.1:$PORT" > "$3" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 60); do
    curl -s -o /dev/null "http://127.0.0.1:$PORT/api/health" && return 0
    kill -0 "$SERVER_PID" 2>/dev/null || return 1
    sleep 0.5
  done
  return 1
}
stop_server() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null; wait "$SERVER_PID" 2>/dev/null; SERVER_PID=""; fi
}
wait_for_log() { # file regex seconds
  for _ in $(seq 1 $(( $3 * 2 ))); do grep -q -E -- "$2" "$1" 2>/dev/null && return 0; sleep 0.5; done
  return 1
}
json_get() { python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); print(eval(sys.argv[2], {"d": d}))' "$1" "$2" 2>/dev/null; }

has_step() { case " $STEPS " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }

# ---------------------------------------------------------------- env
if has_step env; then
  say "00 env"; D="$OUT/00-env"; mkdir -p "$D"
  {
    echo "utc: $STAMP"; echo "host: $(hostname)"; echo "kernel: $(uname -srm)"
    grep -E '^PRETTY_NAME=' /etc/os-release 2>/dev/null
    echo "commit: $(git rev-parse HEAD)"; echo "branch: $(git branch --show-current)"
    echo "rustc: $(rustc -V)"; echo "cargo: $(cargo -V)"
    echo "node: $(node -v 2>/dev/null || echo none)"; echo "python: $(python3 -V)"
    echo "evidence port: $PORT"
  } > "$D/env.txt"
  git log --oneline -12 > "$D/git-log.txt"
  git status --porcelain -- . ":(exclude)artifacts/live-evidence" > "$D/git-status.txt"
  record env recorded PASS "env.txt, git-log.txt, git-status.txt"
  if [ -s "$D/git-status.txt" ]; then record env clean-tree FAIL "uncommitted changes, see git-status.txt"
  else record env clean-tree PASS "working tree matches $COMMIT"; fi
fi

# ---------------------------------------------------------------- build
if has_step build; then
  say "01 build"; D="$OUT/01-build"; mkdir -p "$D"
  run_logged "$D" cargo-build cargo build; check_exit build cargo-build $?
  if [ -x "$BIN" ]; then
    sha256sum "$BIN" | sed "s|$ROOT/||" > "$D/n2linkd.sha256"
    stat -c '%s bytes' "$BIN" > "$D/n2linkd.size"
    run_logged "$D" version "$BIN" --version; check_grep build version "$D/version.log" '[0-9]+\.[0-9]+\.[0-9]+'
    run_logged "$D" help "$BIN" --help; check_grep build help-names-n2link "$D/help.log" 'n2link'
    check_no_grep build help-free-of-edgelink "$D/help.log" 'EdgeLink'
  else
    record build binary FAIL "missing $BIN"
  fi
fi

# ---------------------------------------------------------------- branding (+ a first flow)
if has_step branding; then
  say "02 branding"; D="$OUT/02-branding"; mkdir -p "$D"; H="$TMP_ROOT/branding-home"; mkdir -p "$H"
  if start_server "$BIN" "$H" "$D/server.log"; then
    record branding server-start PASS "port $PORT"
    http_get /api/health "$D/health.json";  check_grep branding health "$D/health.json" '"service":"n2link-web"'
    http_get /api/info "$D/info.json";      check_grep branding info "$D/info.json" '"name":"n2link Web API"'
    http_get /theme "$D/theme.json";        check_grep branding theme-title "$D/theme.json" '"title":"n2link"'
    check_grep branding theme-logo "$D/theme.json" 'n2link/n2link-logo.svg'
    http_get / "$D/index.html";             check_grep branding editor-title "$D/index.html" '<title>n2link</title>'
    http_get /favicon.ico "$D/favicon.ico"
    if cmp -s "$D/favicon.ico" assets/brand/favicon.ico; then record branding favicon PASS "same bytes as assets/brand/favicon.ico"
    else record branding favicon FAIL "served favicon differs from assets/brand/favicon.ico"; fi
    http_get /n2link/n2link-logo.svg "$D/n2link-logo.svg"; check_grep branding logo-served "$D/n2link-logo.svg.status" '^200 image/svg'
    http_get /n2link/n2link-icon.svg "$D/n2link-icon.svg"; check_grep branding icon-served "$D/n2link-icon.svg.status" '^200 image/svg'
    http_get /client/ "$D/client.html";     check_grep branding client-title "$D/client.html" '<title>n2link</title>'
    curl -s -H 'Accept: application/json' "http://127.0.0.1:$PORT/plugins" > "$D/plugins.json"
    check_grep branding editor-plugins "$D/plugins.json" 'n2link-config/config-editor'
    http_get /status "$D/status.json"
    cat > "$D/flow-ground-truth.json" <<'JSON'
{"flows":[
 {"id":"e0000000000000a1","type":"tab","label":"evidence"},
 {"id":"e0000000000000a2","type":"inject","z":"e0000000000000a1","name":"","props":[{"p":"payload"}],"repeat":"","crontab":"","once":true,"onceDelay":0.2,"topic":"","payload":"ground truth n2link","payloadType":"str","wires":[["e0000000000000a3"]]},
 {"id":"e0000000000000a3","type":"debug","z":"e0000000000000a1","name":"evidence","active":true,"tosidebar":true,"console":true,"tostatus":false,"complete":"payload","targetType":"msg","wires":[]}
]}
JSON
    http_post_json /flows "$D/flow-ground-truth.json" "$D/deploy-response.json"
    check_grep branding deploy-accepted "$D/deploy-response.json.status" '^(200|204)'
    if wait_for_log "$D/server.log" 'ground truth n2link' 10; then record branding flow-runs PASS "debug node logged the injected payload"
    else record branding flow-runs FAIL "payload not seen in server.log"; fi
    cp "$H/flows.json" "$D/flows.json" 2>/dev/null
    stop_server
  else
    record branding server-start FAIL "see server.log"; stop_server
  fi
fi

# ---------------------------------------------------------------- compat (old names)
if has_step compat; then
  say "03 compat"; D="$OUT/03-compat"; mkdir -p "$D"; C="$TMP_ROOT/compat"
  mkdir -p "$C/user/.edgelinkd" "$C/env-home" "$C/fresh"
  printf '[runtime]\n' > "$C/user/.edgelinkd/edgelinkd.toml"
  (cd "$C" && HOME="$C/user" run_logged "$D" legacy-home "$BIN" credentials status)
  check_grep compat legacy-home-warning "$D/legacy-home.log" 'legacy home directory .*\.edgelinkd; move it with: mv'
  check_grep compat legacy-config-warning "$D/legacy-home.log" 'legacy config file .*edgelinkd\.toml'
  (cd "$C" && HOME="$C/fresh" EDGELINK_HOME="$C/env-home" run_logged "$D" edgelink-home "$BIN" credentials status)
  check_grep compat old-variable-warning "$D/edgelink-home.log" 'EDGELINK_HOME is deprecated; rename it to N2LINK_HOME'
  check_no_grep compat no-stray-home-warning "$D/edgelink-home.log" 'legacy home directory'
  (cd "$C" && HOME="$C/fresh" EDGELINK_HOME="$C/a" N2LINK_HOME="$C/b" run_logged "$D" conflict "$BIN" credentials status)
  check_grep compat conflict-stops "$D/conflict.log" 'N2LINK_HOME and EDGELINK_HOME are both set to different values'
  rm -rf "$C/fresh"; mkdir -p "$C/fresh"
  (cd "$C" && HOME="$C/fresh" run_logged "$D" fresh-home "$BIN" credentials status)
  ls -a "$C/fresh/.n2linkd" > "$D/fresh-home.ls" 2>&1
  check_grep compat fresh-home-created "$D/fresh-home.ls" '^n2linkd\.toml$'
  check_no_grep compat fresh-home-quiet "$D/fresh-home.log" 'deprecated'
fi

# ---------------------------------------------------------------- credentials
if has_step credentials; then
  say "04 credentials"; D="$OUT/04-credentials"; mkdir -p "$D"; H="$TMP_ROOT/cred-home"; mkdir -p "$H"
  KEY="$(grep -E '^(export )?N2LINK_CREDENTIAL_KEY=' .env 2>/dev/null | tail -1 | sed -E 's/^(export )?N2LINK_CREDENTIAL_KEY=//; s/^["'"'"']//; s/["'"'"']$//')"
  if [ -n "$KEY" ]; then echo "key source: .env (value not recorded)" > "$D/key-source.txt"
  else KEY="$(openssl rand 32 | basenc --base64url | tr -d '=')"; echo "key source: generated for this run (value not recorded)" > "$D/key-source.txt"; fi
  SECRET="evidence-$(openssl rand -hex 12)"
  cat > "$TMP_ROOT/flow-credentials.json" <<JSON
{"flows":[
 {"id":"e0000000000000b1","type":"tab","label":"credentials"},
 {"id":"e0000000000000b2","type":"mqtt-broker","name":"evidence broker","broker":"127.0.0.1","port":1883,"protocolVersion":4,"autoConnect":false,"credentials":{"user":"evidence-user","password":"$SECRET"}}
]}
JSON
  sed "s/$SECRET/<random, not recorded>/" "$TMP_ROOT/flow-credentials.json" > "$D/flow-credentials.redacted.json"
  export N2LINK_CREDENTIAL_KEY="$KEY"
  if start_server "$BIN" "$H" "$D/server-1.log"; then
    http_post_json /flows "$TMP_ROOT/flow-credentials.json" "$D/deploy-response.json"
    check_grep credentials deploy-accepted "$D/deploy-response.json.status" '^(200|204)'
    stop_server
    # Sidecars stay plaintext until an explicit `credentials migrate` (docs/operations/credential-lifecycle.md).
    (N2LINK_HOME="$H" run_logged "$D" status-before-migrate "$BIN" credentials status)
    if grep -q '"format": "plaintext"' "$D/status-before-migrate.log"; then
      record credentials default-is-plaintext INFO "a new installation writes plaintext until \`credentials migrate\`"
    else
      record credentials default-is-plaintext INFO "sidecar was not plaintext before migrate, see status-before-migrate.log"
    fi
    mkdir -m 700 "$TMP_ROOT/cred-backup"
    (N2LINK_HOME="$H" run_logged "$D" credentials-migrate "$BIN" credentials migrate --backup-dir "$TMP_ROOT/cred-backup")
    check_exit credentials migrate "$(cat "$D/credentials-migrate.exit")"
    ls -l "$TMP_ROOT/cred-backup" | sed "s|$TMP_ROOT|<tmp>|" > "$D/migrate-backup.ls"
    if [ -f "$H/flows_cred.json" ]; then
      python3 -I -c 'import json,sys; d=json.load(open(sys.argv[1])); print(json.dumps({k: d[k] for k in ("format","version","algorithm") if k in d}))' \
        "$H/flows_cred.json" > "$D/sidecar-header.json"
      check_grep credentials sidecar-format "$D/sidecar-header.json" '"format": "n2link-credentials"'
      check_no_grep credentials password-not-plaintext "$H/flows_cred.json" "$SECRET"
      check_no_grep credentials user-not-plaintext "$H/flows_cred.json" "evidence-user"
    else
      record credentials sidecar-written FAIL "no flows_cred.json"
    fi
    (N2LINK_HOME="$H" run_logged "$D" credentials-status "$BIN" credentials status)
    check_exit credentials status-command "$(cat "$D/credentials-status.exit")"
    if start_server "$BIN" "$H" "$D/server-2.log"; then
      http_get /credentials/mqtt-broker/e0000000000000b2 "$D/credentials-after-restart.json"
      check_grep credentials survives-restart "$D/credentials-after-restart.json" '"user":"evidence-user"'
      check_grep credentials password-flagged "$D/credentials-after-restart.json" '"has_password":true'
      check_no_grep credentials password-not-returned "$D/credentials-after-restart.json" "$SECRET"
      stop_server
    else
      record credentials restart FAIL "see server-2.log"; stop_server
    fi
    L="$TMP_ROOT/legacy-cred-home"; mkdir -p "$L"
    cp "$H/flows.json" "$H/flows_cred.json" "$L/" 2>/dev/null
    cp "$H/flows_cred.key" "$L/" 2>/dev/null
    sed -i 's/"n2link-credentials"/"edgelink-credentials"/' "$L/flows_cred.json"
    (N2LINK_HOME="$L" run_logged "$D" legacy-sidecar-status "$BIN" credentials status)
    check_grep credentials edgelinkd-sidecar-not-decryptable "$D/legacy-sidecar-status.log" '"decryptable": false'
    start_server "$BIN" "$L" "$D/legacy-sidecar-server.log"
    wait_for_log "$D/legacy-sidecar-server.log" 'encrypted by EdgeLinkd' 10
    stop_server
    check_grep credentials edgelinkd-sidecar-refused "$D/legacy-sidecar-server.log" 'encrypted by EdgeLinkd'
  else
    record credentials server-start FAIL "see server-1.log"; stop_server
  fi
  unset N2LINK_CREDENTIAL_KEY KEY SECRET
fi

# ---------------------------------------------------------------- wasm
if has_step wasm; then
  say "05 wasm"; D="$OUT/05-wasm"; mkdir -p "$D"; H="$TMP_ROOT/wasm-home"; mkdir -p "$H"
  run_logged "$D" cargo-build-nodes-wasm cargo build --features nodes_wasm --target-dir "$WASM_TARGET"
  check_exit wasm cargo-build "$(cat "$D/cargo-build-nodes-wasm.exit")"
  run_logged "$D" wasm-examples scripts/wasm-examples.sh; check_exit wasm examples-build "$(cat "$D/wasm-examples.exit")"
  sha256sum target/wasm-examples/*.wasm > "$D/packages.sha256" 2>/dev/null
  for f in target/wasm-examples/*.wasm; do
    printf '%s: %s\n' "$(basename "$f")" "$(grep -a -o -E 'n2link:node/v1|edgelink:node/v1|n2link\.manifest|edgelink\.manifest' "$f" | sort -u | tr '\n' ' ')"
  done > "$D/package-names.txt"
  check_no_grep wasm packages-n2link-only "$D/package-names.txt" 'edgelink'
  printf '[runtime.wasm]\nenabled = true\n' > "$H/n2linkd.dev.toml"
  (N2LINK_HOME="$H" run_logged "$D" plugin-stage "$WASM_BIN" plugin stage target/wasm-examples/uppercase.wasm)
  check_exit wasm plugin-stage "$(cat "$D/plugin-stage.exit")"
  SHA="$(python3 -I -c 'import json,re,sys; t=open(sys.argv[1]).read(); m=re.search(r"\{.*\}", t, re.S); d=json.loads(m.group(0)) if m else {}; print(d.get("sha256") or d.get("digest") or "")' "$D/plugin-stage.log" 2>/dev/null)"
  [ -z "$SHA" ] && SHA="$(sha256sum target/wasm-examples/uppercase.wasm | cut -d' ' -f1)"
  (N2LINK_HOME="$H" run_logged "$D" plugin-activate "$WASM_BIN" plugin activate --sha256 "$SHA" n2link/uppercase)
  check_exit wasm plugin-activate "$(cat "$D/plugin-activate.exit")"
  (N2LINK_HOME="$H" run_logged "$D" plugin-list "$WASM_BIN" plugin list)
  check_grep wasm plugin-listed "$D/plugin-list.log" 'n2link/uppercase'
  if [ -f "$LEGACY_PLUGIN" ]; then
    sha256sum "$LEGACY_PLUGIN" > "$D/legacy-package.sha256"
    (N2LINK_HOME="$H" run_logged "$D" legacy-plugin-stage "$WASM_BIN" plugin stage "$LEGACY_PLUGIN")
    check_grep wasm edgelinkd-plugin-refused "$D/legacy-plugin-stage.log" 'built for EdgeLinkd'
  else
    record wasm edgelinkd-plugin-refused SKIP "no EdgeLinkd-built package at $LEGACY_PLUGIN"
  fi
  if start_server "$WASM_BIN" "$H" "$D/server.log"; then
    curl -s -H 'Accept: application/json' "http://127.0.0.1:$PORT/nodes" > "$D/nodes.json"
    check_grep wasm node-registered "$D/nodes.json" 'wasm-n2link-uppercase'
    http_get /assistant/catalog "$D/copilot-catalog.json"
    check_grep wasm copilot-catalog "$D/copilot-catalog.json" 'wasm-n2link-uppercase'
    cat > "$D/flow-wasm.json" <<'JSON'
{"flows":[
 {"id":"e0000000000000c1","type":"tab","label":"wasm"},
 {"id":"e0000000000000c2","type":"inject","z":"e0000000000000c1","name":"","props":[{"p":"payload"}],"repeat":"","crontab":"","once":true,"onceDelay":0.2,"topic":"","payload":"hello n2link","payloadType":"str","wires":[["e0000000000000c3"]]},
 {"id":"e0000000000000c3","type":"wasm-n2link-uppercase","z":"e0000000000000c1","name":"","wires":[["e0000000000000c4"]]},
 {"id":"e0000000000000c4","type":"debug","z":"e0000000000000c1","name":"evidence","active":true,"tosidebar":true,"console":true,"tostatus":false,"complete":"payload","targetType":"msg","wires":[]}
]}
JSON
    http_post_json /flows "$D/flow-wasm.json" "$D/deploy-response.json"
    check_grep wasm deploy-accepted "$D/deploy-response.json.status" '^(200|204)'
    if wait_for_log "$D/server.log" 'HELLO N2LINK' 10; then record wasm plugin-runs PASS "uppercase plugin output reached the debug node"
    else record wasm plugin-runs FAIL "HELLO N2LINK not seen in server.log"; fi
    stop_server
  else
    record wasm server-start FAIL "see server.log"; stop_server
  fi
fi

# ---------------------------------------------------------------- tests
if has_step tests; then
  say "06 tests"; D="$OUT/06-tests"; mkdir -p "$D"
  run_logged "$D" cargo-test-full cargo test --workspace --features full
  check_exit tests workspace-full "$(cat "$D/cargo-test-full.exit")"
  grep -E '^test result' "$D/cargo-test-full.log" > "$D/cargo-test-full.summary"
  if [ -n "${N2LINK_EVIDENCE_PYTEST:-}" ]; then
    run_logged "$D" pytest "$N2LINK_EVIDENCE_PYTEST" -m pytest ./tests -q -p no:cacheprovider
    tail -1 "$D/pytest.log" > "$D/pytest.summary"
    if [ "$(cat "$D/pytest.exit")" -eq 0 ]; then
      record tests pytest PASS "$(cat "$D/pytest.summary")"
    else
      record tests pytest FAIL "$(cat "$D/pytest.summary"), see pytest.log"
    fi
  else
    record tests pytest SKIP "set N2LINK_EVIDENCE_PYTEST=/path/to/python to include it"
  fi
fi

# ---------------------------------------------------------------- summary
PASS=$(awk -F'\t' 'NR>1 && $3=="PASS"' "$RESULTS" | wc -l)
FAIL=$(awk -F'\t' 'NR>1 && $3=="FAIL"' "$RESULTS" | wc -l)
SKIP=$(awk -F'\t' 'NR>1 && $3=="SKIP"' "$RESULTS" | wc -l)
INFO=$(awk -F'\t' 'NR>1 && $3=="INFO"' "$RESULTS" | wc -l)
{
  echo "# n2link live evidence $STAMP"
  echo
  echo "Commit \`$(git rev-parse HEAD)\` on \`$(git branch --show-current)\`, host \`$(hostname)\`, steps: $STEPS."
  echo
  echo "**$PASS passed, $FAIL failed, $SKIP skipped, $INFO observations.**"
  echo
  echo "| Step | Check | Result | Detail |"
  echo "|---|---|---|---|"
  awk -F'\t' 'NR>1 {printf "| %s | %s | %s | %s |\n", $1, $2, $3, $4}' "$RESULTS"
} > "$OUT/SUMMARY.md"
(cd "$OUT" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
say "evidence: $OUT ($PASS passed, $FAIL failed, $SKIP skipped)"
[ "$FAIL" -eq 0 ]
