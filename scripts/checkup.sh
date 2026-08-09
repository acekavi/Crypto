#!/usr/bin/env bash
# Performance checkup for the live testnet bot.
#
# Reads only the local journal — no exchange calls, no credentials, safe to run
# while the bot is running.
set -euo pipefail
cd "$(dirname "$0")/.."

DB=${1:-data/bot.db}
[ -f "$DB" ] || { echo "no journal at $DB — has the bot ever run?"; exit 1; }
q() { sqlite3 "$DB" "$1"; }

echo "================ CRYPTO BOT CHECKUP ================"
echo "journal : $DB"
echo "as of   : $(date -u '+%Y-%m-%d %H:%M:%S UTC')"

echo
echo "---- service ----"
if command -v systemctl >/dev/null 2>&1; then
  systemctl --user is-active crypto-bot 2>/dev/null | sed 's/^/  state       : /' || echo "  state       : not installed"
  systemctl --user show crypto-bot -p NRestarts --value 2>/dev/null | sed 's/^/  restarts    : /' || true
else
  pgrep -f "crypto-bot testnet" >/dev/null && echo "  state       : running (no systemd)" || echo "  state       : NOT RUNNING"
fi

echo
echo "---- equity ----"
# Decimals are TEXT: never ORDER BY them. Order by the integer timestamp.
first=$(q "SELECT equity FROM equity_snapshots ORDER BY at_ms ASC  LIMIT 1;")
last=$( q "SELECT equity FROM equity_snapshots ORDER BY at_ms DESC LIMIT 1;")
span=$( q "SELECT (MAX(at_ms)-MIN(at_ms))/86400000.0 FROM equity_snapshots;")
echo "  first       : ${first:-n/a}"
echo "  latest      : ${last:-n/a}"
printf "  observed    : %.2f days\n" "${span:-0}"
[ -n "${first:-}" ] && [ -n "${last:-}" ] && \
  python3 -c "print(f'  change      : {float('$last')-float('$first'):+.2f}  ({(float('$last')/float('$first')-1)*100:+.2f}%)')"

echo
echo "---- orders ----"
q "SELECT '  ' || state || ' : ' || COUNT(*) FROM orders GROUP BY state;" || true
[ "$(q 'SELECT COUNT(*) FROM orders;')" = "0" ] && echo "  (no orders yet)"

echo
echo "---- closed trades ----"
q "SELECT COUNT(*) FROM fills;" | sed 's/^/  fills       : /'

echo
echo "---- open protections ----"
q "SELECT '  ' || symbol || '  trigger=' || trigger ||
          '  breakeven=' || CASE moved_to_breakeven WHEN 1 THEN 'MOVED' ELSE 'not yet' END
   FROM stop_protections;" || true
[ "$(q 'SELECT COUNT(*) FROM stop_protections;')" = "0" ] && echo "  (flat)"

echo
echo "---- event tally ----"
q "SELECT '  ' || kind || ' : ' || COUNT(*) FROM trade_events GROUP BY kind ORDER BY COUNT(*) DESC;" || true
[ "$(q 'SELECT COUNT(*) FROM trade_events;')" = "0" ] && echo "  (no events — the bot has not traded)"

echo
echo "---- last 15 events ----"
q "SELECT '  ' || datetime(at_ms/1000,'unixepoch') || '  ' || symbol || '  ' || kind || '  ' || detail
   FROM trade_events ORDER BY at_ms DESC, id DESC LIMIT 15;" || true

echo
echo "---- halt ----"
q "SELECT '  HALTED: ' || reason || ' at ' || datetime(set_at_ms/1000,'unixepoch') FROM halt_state;" || true
[ "$(q 'SELECT COUNT(*) FROM halt_state;')" = "0" ] && echo "  not halted"

echo
echo "---- expectation ----"
echo "  liquidity_sweep_v2 averaged ~0.4 trades/day across 8 symbols on the"
echo "  research window, so ~2 trades in 5 days. At that sample nothing is"
echo "  statistically meaningful: it tells you the plumbing works, not whether"
echo "  the strategy has an edge. Research-window figures were 24.2% win rate"
echo "  and PF 1.457, and they have no out-of-sample validation."
