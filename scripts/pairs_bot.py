from __future__ import annotations

import argparse
import dataclasses
import enum
import hmac
import hashlib
import json
import logging
import math
import os
from logging.handlers import RotatingFileHandler
import sqlite3
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass
from decimal import Decimal, ROUND_DOWN, ROUND_UP, getcontext
from pathlib import Path
from typing import Iterable

getcontext().prec = 28

TF_MS = 60 * 60 * 1000


class PairSide(str, enum.Enum):
    LONG_SPREAD = "long_spread"  # long A, short B
    SHORT_SPREAD = "short_spread"  # short A, long B


@dataclass(frozen=True)
class PairParams:
    leg_a: str = "DOGEUSDT"
    leg_b: str = "XRPUSDT"
    timeframe: str = "60"
    rolling_window: int = 240
    entry_z: float = 3.5
    stop_z: float = 4.5
    target_z: float = 0.5
    max_hold_bars: int = 72
    fee_per_leg: float = 0.0002
    per_leg_notional_usdt: Decimal = Decimal("25")
    risk_pct_of_equity: Decimal = Decimal("0.02")
    cap_per_leg_to_available_equity: bool = True
    enable_breakeven: bool = True
    breakeven_r_multiple: Decimal = Decimal("2")

    def reward_risk_ratio(self) -> float:
        return abs(self.entry_z - self.target_z) / abs(self.stop_z - self.entry_z)

    def display_pair(self) -> str:
        return f"{self.leg_a}/{self.leg_b}"


def pair_slug(params: PairParams) -> str:
    def norm(symbol: str) -> str:
        return symbol.removesuffix("USDT").lower()
    return f"{norm(params.leg_a)}_{norm(params.leg_b)}"


ACTIVE_BOT_PROFILES = [
    {
        'name': 'AAVE/ETH',
        'bot_id': 'aave_eth',
        'priority': 100,
        'service': 'crypto-bot-aave-eth.service',
        'params': PairParams(
            leg_a='AAVEUSDT',
            leg_b='ETHUSDT',
            timeframe='60',
            rolling_window=180,
            entry_z=3.0,
            stop_z=4.0,
            target_z=0.0,
            max_hold_bars=48,
            risk_pct_of_equity=Decimal('0.03'),
            enable_breakeven=False,
        ),
        'state_path': Path('/home/acekavi/Projects/Crypto/data/pairs_bot_aave_eth_state.json'),
        'log_path': Path('/home/acekavi/Projects/Crypto/logs/pairs-bot-aave-eth.log'),
    },
    {
        'name': 'ENA/XRP',
        'bot_id': 'ena_xrp',
        'priority': 90,
        'service': 'crypto-bot-ena-xrp.service',
        'params': PairParams(
            leg_a='ENAUSDT',
            leg_b='XRPUSDT',
            timeframe='60',
            rolling_window=336,
            entry_z=3.25,
            stop_z=4.5,
            target_z=-0.5,
            max_hold_bars=96,
            risk_pct_of_equity=Decimal('0.03'),
            enable_breakeven=False,
        ),
        'state_path': Path('/home/acekavi/Projects/Crypto/data/pairs_bot_ena_xrp_state.json'),
        'log_path': Path('/home/acekavi/Projects/Crypto/logs/pairs-bot-ena-xrp.log'),
    },
    {
        'name': 'BNB/XAUT',
        'bot_id': 'bnb_xaut',
        'priority': 80,
        'service': 'crypto-bot-bnb-xaut.service',
        'params': PairParams(
            leg_a='BNBUSDT',
            leg_b='XAUTUSDT',
            timeframe='60',
            rolling_window=240,
            entry_z=3.0,
            stop_z=4.5,
            target_z=-4.5,
            max_hold_bars=96,
            risk_pct_of_equity=Decimal('0.03'),
            enable_breakeven=False,
        ),
        'state_path': Path('/home/acekavi/Projects/Crypto/data/pairs_bot_bnb_xaut_state.json'),
        'log_path': Path('/home/acekavi/Projects/Crypto/logs/pairs-bot-bnb-xaut.log'),
    },
]


def active_bot_profiles() -> list[dict]:
    return [{**profile} for profile in ACTIVE_BOT_PROFILES]


def default_active_profile() -> dict:
    return {**ACTIVE_BOT_PROFILES[0]}


def active_portfolio_symbols() -> set[str]:
    return {sym for profile in ACTIVE_BOT_PROFILES for sym in (profile['params'].leg_a, profile['params'].leg_b)}


def params_from_args(args: argparse.Namespace) -> PairParams:
    def boolish(value, default: bool) -> bool:
        if value is None:
            return default
        if isinstance(value, bool):
            return value
        return str(value).strip().lower() in {"1", "true", "yes", "on"}

    return PairParams(
        leg_a=args.leg_a,
        leg_b=args.leg_b,
        timeframe=args.timeframe,
        rolling_window=args.rolling_window,
        entry_z=args.entry_z,
        stop_z=args.stop_z,
        target_z=args.target_z,
        max_hold_bars=args.max_hold_bars,
        fee_per_leg=args.fee_per_leg,
        per_leg_notional_usdt=Decimal(str(args.per_leg_notional_usdt)),
        risk_pct_of_equity=Decimal(str(getattr(args, 'risk_pct_of_equity', '0.02'))),
        cap_per_leg_to_available_equity=boolish(getattr(args, 'cap_per_leg_to_available_equity', True), True),
        enable_breakeven=boolish(getattr(args, 'enable_breakeven', True), True),
        breakeven_r_multiple=Decimal(str(getattr(args, 'breakeven_r_multiple', '2'))),
    )


@dataclass
class PositionState:
    side: PairSide
    opened_at_ms: int
    entry_z: float
    a_qty: str
    b_qty: str
    a_entry: str
    b_entry: str
    a_order_id: str
    b_order_id: str
    breakeven_armed: bool = False
    per_leg_notional_usdt: str | None = None


@dataclass
class RuntimeState:
    params: PairParams
    last_bar_ms: int | None = None
    position: PositionState | None = None
    last_loop_wall_time: float | None = None
    last_seen_z: float | None = None
    last_seen_signal: str | None = None
    last_guard_reason: str | None = None

    @classmethod
    def from_file(cls, path: Path, params: PairParams) -> "RuntimeState":
        if not path.exists():
            return cls(params=params)
        data = json.loads(path.read_text())
        position = None
        if data.get("position"):
            pdata = dict(data["position"])
            pdata.setdefault("breakeven_armed", False)
            pdata.setdefault("per_leg_notional_usdt", None)
            position = PositionState(**pdata)
        return cls(
            params=params,
            last_bar_ms=data.get("last_bar_ms"),
            position=position,
            last_loop_wall_time=data.get("last_loop_wall_time"),
            last_seen_z=data.get("last_seen_z"),
            last_seen_signal=data.get("last_seen_signal"),
            last_guard_reason=data.get("last_guard_reason"),
        )

    def save(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "last_bar_ms": self.last_bar_ms,
            "position": dataclasses.asdict(self.position) if self.position else None,
            "last_loop_wall_time": self.last_loop_wall_time,
            "last_seen_z": self.last_seen_z,
            "last_seen_signal": self.last_seen_signal,
            "last_guard_reason": self.last_guard_reason,
        }
        path.write_text(json.dumps(payload, indent=2, sort_keys=True))


class PairSignalEngine:
    def __init__(self, params: PairParams):
        self.params = params

    def entry_signal(self, zscore: float) -> PairSide | None:
        if zscore >= self.params.entry_z:
            return PairSide.SHORT_SPREAD
        if zscore <= -self.params.entry_z:
            return PairSide.LONG_SPREAD
        return None

    def should_arm_breakeven(self, side: PairSide, zscore: float) -> bool:
        if not self.params.enable_breakeven:
            return False
        risk_band = self.params.stop_z - self.params.entry_z
        arm_multiple = float(self.params.breakeven_r_multiple)
        if side == PairSide.SHORT_SPREAD:
            return zscore <= (self.params.entry_z - arm_multiple * risk_band)
        return zscore >= (-self.params.entry_z + arm_multiple * risk_band)

    def exit_reason(
        self,
        side: PairSide,
        zscore: float,
        age_bars: int,
        breakeven_armed: bool = False,
        pnl_fraction: Decimal | None = None,
    ) -> str | None:
        if breakeven_armed and pnl_fraction is not None and pnl_fraction <= 0:
            return "breakeven"
        if age_bars > self.params.max_hold_bars:
            return "time"
        if side == PairSide.SHORT_SPREAD:
            if zscore <= self.params.target_z:
                return "target"
            if zscore >= self.params.stop_z:
                return "stop"
        else:
            if zscore >= -self.params.target_z:
                return "target"
            if zscore <= -self.params.stop_z:
                return "stop"
        return None


def unrealized_pair_pnl_fraction(
    side: PairSide,
    a_entry: Decimal,
    b_entry: Decimal,
    a_now: Decimal,
    b_now: Decimal,
    fee_per_leg: float,
) -> Decimal:
    a_ret = (a_now / a_entry) - Decimal("1")
    b_ret = (b_now / b_entry) - Decimal("1")
    gross = (a_ret - b_ret) if side == PairSide.LONG_SPREAD else (b_ret - a_ret)
    return gross - Decimal(str(4 * fee_per_leg))


def risk_based_per_leg_notional(
    params: PairParams,
    total_equity: Decimal,
    available_equity: Decimal,
    spread_sigma: float,
) -> Decimal:
    sigma = Decimal(str(spread_sigma))
    if sigma <= 0:
        raise ValueError("spread_sigma must be positive")
    desired = (total_equity * params.risk_pct_of_equity) / sigma
    if params.cap_per_leg_to_available_equity:
        desired = min(desired, available_equity)
    if desired <= 0:
        raise ValueError("per-leg notional must be positive")
    return desired


def rolling_mean_stddev(values: list[float], window: int) -> tuple[list[float | None], list[float | None]]:
    means: list[float | None] = [None] * len(values)
    sds: list[float | None] = [None] * len(values)
    if window <= 0:
        raise ValueError("window must be positive")
    for i in range(window, len(values)):
        hist = values[i - window : i]
        mean = sum(hist) / window
        var = sum((x - mean) ** 2 for x in hist) / window
        means[i] = mean
        sds[i] = math.sqrt(max(var, 1e-12))
    return means, sds


def rolling_zscores(values: list[float], window: int) -> list[float | None]:
    means, sds = rolling_mean_stddev(values, window)
    out: list[float | None] = [None] * len(values)
    for i in range(window, len(values)):
        mean = means[i]
        sd = sds[i]
        if mean is None or sd is None:
            continue
        out[i] = (values[i] - mean) / sd
    return out


def load_pair_series_from_db(db_path: str, params: PairParams) -> tuple[list[int], list[float], list[float]]:
    conn = sqlite3.connect(f"file:{db_path}?mode=ro&immutable=1", uri=True)
    rows = conn.execute(
        """
        SELECT symbol, open_time_ms, CAST(close AS REAL)
        FROM candles
        WHERE timeframe = ? AND symbol IN (?, ?)
        ORDER BY symbol, open_time_ms
        """,
        (params.timeframe, params.leg_a, params.leg_b),
    ).fetchall()
    by_sym: dict[str, dict[int, float]] = {params.leg_a: {}, params.leg_b: {}}
    for sym, ms, close in rows:
        by_sym[sym][ms] = float(close)
    common = sorted(set(by_sym[params.leg_a]).intersection(by_sym[params.leg_b]))
    a = [by_sym[params.leg_a][ms] for ms in common]
    b = [by_sym[params.leg_b][ms] for ms in common]
    return common, a, b


def spread_series(a: Iterable[float], b: Iterable[float]) -> list[float]:
    return [math.log(x) - math.log(y) for x, y in zip(a, b)]


def backtest(db_path: str, params: PairParams, start_ms: int | None = None, end_ms: int | None = None) -> dict:
    times, a_prices, b_prices = load_pair_series_from_db(db_path, params)
    spreads = spread_series(a_prices, b_prices)
    spread_means, spread_sds = rolling_mean_stddev(spreads, params.rolling_window)
    zscores = rolling_zscores(spreads, params.rolling_window)
    engine = PairSignalEngine(params)

    wins = 0
    losses = 0
    gross_profit = 0.0
    gross_loss = 0.0
    equity = 1.0
    position = None
    trades = []

    for i, ms in enumerate(times):
        if start_ms is not None and ms < start_ms:
            continue
        if end_ms is not None and ms >= end_ms:
            continue
        z = zscores[i]
        sigma = spread_sds[i]
        if z is None or sigma is None:
            continue
        if position is None:
            sig = engine.entry_signal(z)
            if sig is None:
                continue
            if params.risk_pct_of_equity > 0:
                per_leg_notional = float(
                    risk_based_per_leg_notional(
                        params=params,
                        total_equity=Decimal(str(equity)),
                        available_equity=Decimal(str(equity)),
                        spread_sigma=float(sigma),
                    )
                )
            else:
                per_leg_notional = float(params.per_leg_notional_usdt)
            position = {
                "side": sig,
                "entry_ms": ms,
                "entry_z": z,
                "a0": a_prices[i],
                "b0": b_prices[i],
                "age": 0,
                "breakeven_armed": False,
                "per_leg_notional": per_leg_notional,
            }
            continue

        position["age"] += 1
        if not position["breakeven_armed"] and engine.should_arm_breakeven(position["side"], z):
            position["breakeven_armed"] = True
        pnl_fraction = float(
            unrealized_pair_pnl_fraction(
                position["side"],
                Decimal(str(position["a0"])),
                Decimal(str(position["b0"])),
                Decimal(str(a_prices[i])),
                Decimal(str(b_prices[i])),
                params.fee_per_leg,
            )
        )
        reason = engine.exit_reason(
            position["side"],
            z,
            position["age"],
            breakeven_armed=position["breakeven_armed"],
            pnl_fraction=Decimal(str(pnl_fraction)),
        )
        if reason is None:
            continue

        pnl = pnl_fraction * position["per_leg_notional"]
        equity += pnl
        if pnl > 0:
            wins += 1
            gross_profit += pnl
        elif pnl < 0:
            losses += 1
            gross_loss += -pnl
        trades.append(
            {
                "entry_ms": position["entry_ms"],
                "exit_ms": ms,
                "side": position["side"].value,
                "entry_z": position["entry_z"],
                "exit_z": z,
                "reason": reason,
                "net": pnl,
                "per_leg_notional": position["per_leg_notional"],
            }
        )
        position = None

    trade_count = len(trades)
    equity_curve = 1.0
    peak = 1.0
    max_dd = 0.0
    for trade in trades:
        equity_curve += trade["net"]
        peak = max(peak, equity_curve)
        dd = (peak - equity_curve) / peak if peak > 0 else 0.0
        max_dd = max(max_dd, dd)

    net = equity - 1.0
    return {
        "pair": f"{params.leg_a}/{params.leg_b}",
        "trades": trade_count,
        "wins": wins,
        "losses": losses,
        "win_rate": (wins / trade_count) if trade_count else 0.0,
        "profit_factor": (gross_profit / gross_loss) if gross_loss > 0 else (float("inf") if gross_profit > 0 else 0.0),
        "net": net,
        "avg_trade": (net / trade_count) if trade_count else 0.0,
        "max_drawdown_pct": max_dd * 100.0,
        "trades_detail": trades,
    }


class BybitClient:
    def __init__(self, env_path: str, base_url: str = "https://api-testnet.bybit.com"):
        self.base_url = base_url.rstrip("/")
        self.env_path = env_path
        self._load_env()
        self.api_key = os.environ["BYBIT_API_KEY"]
        self.api_secret = os.environ["BYBIT_API_SECRET"]
        self.recv_window = "20000"

    def _load_env(self) -> None:
        for line in Path(self.env_path).read_text().splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            os.environ.setdefault(key, value)

    def _sign(self, ts: str, payload: str) -> str:
        raw = f"{ts}{self.api_key}{self.recv_window}{payload}".encode()
        return hmac.new(self.api_secret.encode(), raw, hashlib.sha256).hexdigest()

    def request(self, method: str, path: str, params: dict | None = None, body: dict | None = None) -> dict:
        params = params or {}
        body = body or {}
        ts = str(int(time.time() * 1000))
        if method == "GET":
            payload = urllib.parse.urlencode(params)
            sig = self._sign(ts, payload)
            url = self.base_url + path + ("?" + payload if payload else "")
            data = None
        else:
            payload = json.dumps(body, separators=(",", ":"))
            sig = self._sign(ts, payload)
            url = self.base_url + path
            data = payload.encode()
        req = urllib.request.Request(
            url,
            data=data,
            method=method,
            headers={
                "X-BAPI-API-KEY": self.api_key,
                "X-BAPI-TIMESTAMP": ts,
                "X-BAPI-RECV-WINDOW": self.recv_window,
                "X-BAPI-SIGN": sig,
                "Content-Type": "application/json",
                "User-Agent": "hermes-pairs-bot/0.1",
            },
        )
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.loads(resp.read().decode())
        if data.get("retCode") != 0:
            raise RuntimeError(f"Bybit error {data.get('retCode')}: {data.get('retMsg')}")
        return data

    def get_closed_klines(self, symbol: str, interval: str, limit: int) -> list[dict]:
        res = self.request("GET", "/v5/market/kline", {"category": "linear", "symbol": symbol, "interval": interval, "limit": str(limit)})
        candles = []
        for row in res["result"]["list"]:
            open_ms = int(row[0])
            if open_ms + TF_MS > int(time.time() * 1000):
                continue
            candles.append({
                "open_time_ms": open_ms,
                "open": Decimal(row[1]),
                "high": Decimal(row[2]),
                "low": Decimal(row[3]),
                "close": Decimal(row[4]),
            })
        candles.sort(key=lambda c: c["open_time_ms"])
        return candles

    def ticker(self, symbol: str) -> dict:
        res = self.request("GET", "/v5/market/tickers", {"category": "linear", "symbol": symbol})
        row = res["result"]["list"][0]
        return {
            "bid1": Decimal(row["bid1Price"]),
            "ask1": Decimal(row["ask1Price"]),
            "last": Decimal(row["lastPrice"]),
        }

    def instrument(self, symbol: str) -> dict:
        res = self.request("GET", "/v5/market/instruments-info", {"category": "linear", "symbol": symbol})
        row = res["result"]["list"][0]
        return {
            "tick_size": Decimal(row["priceFilter"]["tickSize"]),
            "qty_step": Decimal(row["lotSizeFilter"]["qtyStep"]),
            "min_qty": Decimal(row["lotSizeFilter"]["minOrderQty"]),
            "min_notional": Decimal(row["lotSizeFilter"].get("minNotionalValue", "0")),
        }

    def place_limit_order(self, *, symbol: str, side: str, qty: str, price: str, reduce_only: bool, order_link_id: str) -> dict:
        body = {
            "category": "linear",
            "symbol": symbol,
            "side": side,
            "orderType": "Limit",
            "qty": qty,
            "price": price,
            "timeInForce": "GTC",
            "reduceOnly": reduce_only,
            "orderLinkId": order_link_id,
        }
        return self.request("POST", "/v5/order/create", body=body)

    def cancel_order(self, *, symbol: str, order_link_id: str) -> dict:
        return self.request("POST", "/v5/order/cancel", body={"category": "linear", "symbol": symbol, "orderLinkId": order_link_id})

    def order_realtime(self, *, order_link_id: str) -> list[dict]:
        res = self.request("GET", "/v5/order/realtime", {"category": "linear", "orderLinkId": order_link_id})
        return res["result"].get("list", [])

    def order_history(self, *, order_link_id: str) -> list[dict]:
        res = self.request("GET", "/v5/order/history", {"category": "linear", "orderLinkId": order_link_id})
        return res["result"].get("list", [])

    def wallet_balance(self) -> dict:
        res = self.request("GET", "/v5/account/wallet-balance", {"accountType": "UNIFIED", "coin": "USDT"})
        account = res["result"]["list"][0]
        coin = account["coin"][0]
        return {
            "total_equity": Decimal(str(account.get("totalEquity") or coin.get("equity") or "0")),
            "available_equity": Decimal(str(account.get("totalAvailableBalance") or account.get("totalWalletBalance") or coin.get("walletBalance") or "0")),
            "wallet_balance": Decimal(str(account.get("totalWalletBalance") or coin.get("walletBalance") or "0")),
        }

    def positions(self, symbols: tuple[str, str]) -> list[dict]:
        res = self.request("GET", "/v5/position/list", {"category": "linear", "settleCoin": "USDT"})
        out = []
        for row in res["result"].get("list", []):
            size = Decimal(str(row.get("size", "0")))
            if row.get("symbol") in symbols and size != 0:
                out.append(row)
        return out


def floor_step(x: Decimal, step: Decimal) -> Decimal:
    return (x / step).to_integral_value(rounding=ROUND_DOWN) * step


def ceil_step(x: Decimal, step: Decimal) -> Decimal:
    return (x / step).to_integral_value(rounding=ROUND_UP) * step


def fmt_decimal(x: Decimal) -> str:
    s = format(x.normalize(), "f")
    return s.rstrip("0").rstrip(".") if "." in s else s


def json_ready(value):
    if isinstance(value, Decimal):
        return fmt_decimal(value)
    if isinstance(value, PairSide):
        return value.value
    if isinstance(value, dict):
        return {k: json_ready(v) for k, v in value.items()}
    if isinstance(value, list):
        return [json_ready(v) for v in value]
    if isinstance(value, tuple):
        return [json_ready(v) for v in value]
    return value


def configure_logging(log_file: str | None) -> None:
    handlers: list[logging.Handler] = [logging.StreamHandler()]
    if log_file:
        path = Path(log_file)
        path.parent.mkdir(parents=True, exist_ok=True)
        handlers.append(RotatingFileHandler(path, maxBytes=1_000_000, backupCount=3))
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
        handlers=handlers,
        force=True,
    )


def should_defer_to_higher_priority(current_signal: PairSide | None, current_symbols: set[str], latest_ms: int | None, higher_peers: list[dict]) -> str | None:
    if current_signal is None:
        return None
    for peer in higher_peers:
        if not current_symbols.intersection(peer['shared_symbols']):
            continue
        if peer.get('has_local_position'):
            return f"deferred to higher-priority {peer['display_name']} local position on shared symbol"
        if latest_ms is not None and peer.get('latest_ms') == latest_ms and peer.get('signal'):
            return f"deferred to higher-priority {peer['display_name']} same bar signal={peer['signal']}"
    return None


BOT_PROFILES = {
    profile['bot_id']: {
        'display_name': profile['name'],
        'priority': profile['priority'],
        'params': profile['params'],
        'state_path': str(profile['state_path']),
        'shared_symbols': {profile['params'].leg_a, profile['params'].leg_b},
        'higher_priority_peers': [],
    }
    for profile in ACTIVE_BOT_PROFILES
}


def higher_priority_snapshot(client: BybitClient, bot_id: str, latest_ms: int | None) -> list[dict]:
    profile = BOT_PROFILES.get(bot_id)
    if not profile:
        return []
    peers = []
    for peer_id in profile['higher_priority_peers']:
        peer = BOT_PROFILES[peer_id]
        peer_params = peer['params']
        peer_state = RuntimeState.from_file(Path(peer['state_path']), peer_params)
        peer_latest_ms = None
        peer_signal = None
        try:
            peer_latest_ms, _peer_z, peer_sig = compute_latest_live_signal(client, peer_params)
            peer_signal = peer_sig.value if peer_sig else None
        except Exception:
            peer_latest_ms = peer_state.last_bar_ms
            peer_signal = peer_state.last_seen_signal
        peers.append({
            'display_name': peer['display_name'],
            'priority': peer['priority'],
            'shared_symbols': set(peer['shared_symbols']),
            'has_local_position': peer_state.position is not None,
            'latest_ms': peer_latest_ms if peer_latest_ms is not None else latest_ms,
            'signal': peer_signal,
        })
    return peers


def aggressive_limit_price(side: str, bid: Decimal, ask: Decimal, tick: Decimal) -> Decimal:
    if side == "Buy":
        base = ask + tick * Decimal("5")
        return ceil_step(base, tick)
    base = bid - tick * Decimal("5")
    out = floor_step(base, tick)
    if out <= 0:
        out = tick
    return out


def sized_qty(notional: Decimal, price: Decimal, qty_step: Decimal, min_qty: Decimal, min_notional: Decimal) -> Decimal:
    qty = notional / price
    if min_notional > 0 and qty * price < min_notional:
        qty = min_notional / price
    qty = ceil_step(qty, qty_step)
    if qty < min_qty:
        qty = min_qty
    return qty


def compute_latest_live_snapshot(client: BybitClient, params: PairParams) -> dict:
    a = client.get_closed_klines(params.leg_a, params.timeframe, params.rolling_window + 5)
    b = client.get_closed_klines(params.leg_b, params.timeframe, params.rolling_window + 5)
    by_a = {c["open_time_ms"]: c for c in a}
    by_b = {c["open_time_ms"]: c for c in b}
    common = sorted(set(by_a).intersection(by_b))
    if len(common) < params.rolling_window + 1:
        raise RuntimeError(f"not enough closed candles: {len(common)}")
    closes_a = [float(by_a[ms]["close"]) for ms in common]
    closes_b = [float(by_b[ms]["close"]) for ms in common]
    spreads = spread_series(closes_a, closes_b)
    means, sds = rolling_mean_stddev(spreads, params.rolling_window)
    zscores = rolling_zscores(spreads, params.rolling_window)
    latest_ms = common[-1]
    latest_z = zscores[-1]
    latest_sigma = sds[-1]
    if latest_z is None or latest_sigma is None:
        raise RuntimeError("latest zscore or sigma is missing")
    return {
        "latest_ms": latest_ms,
        "latest_z": float(latest_z),
        "signal": PairSignalEngine(params).entry_signal(float(latest_z)),
        "spread_sigma": float(latest_sigma),
        "a_close": by_a[latest_ms]["close"],
        "b_close": by_b[latest_ms]["close"],
    }


def compute_latest_live_signal(client: BybitClient, params: PairParams) -> tuple[int, float, PairSide | None]:
    snap = compute_latest_live_snapshot(client, params)
    return snap["latest_ms"], snap["latest_z"], snap["signal"]


def wait_for_terminal_state(client: BybitClient, symbol: str, order_link_id: str, timeout_s: int = 20) -> dict | None:
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        open_rows = client.order_realtime(order_link_id=order_link_id)
        if open_rows:
            status = open_rows[0].get("orderStatus")
            if status in {"Filled", "Cancelled", "Rejected"}:
                return open_rows[0]
            if status in {"New", "PartiallyFilled"}:
                time.sleep(1)
                continue
        hist = client.order_history(order_link_id=order_link_id)
        if hist:
            return hist[0]
        time.sleep(1)
    return None


def place_pair_entry(
    client: BybitClient,
    params: PairParams,
    side: PairSide,
    latest_z: float,
    opened_at_ms: int,
    per_leg_notional_usdt: Decimal,
) -> PositionState:
    slug = pair_slug(params)
    a_ticker = client.ticker(params.leg_a)
    b_ticker = client.ticker(params.leg_b)
    a_instr = client.instrument(params.leg_a)
    b_instr = client.instrument(params.leg_b)

    if side == PairSide.LONG_SPREAD:
        a_side, b_side = "Buy", "Sell"
    else:
        a_side, b_side = "Sell", "Buy"

    a_price = aggressive_limit_price(a_side, a_ticker["bid1"], a_ticker["ask1"], a_instr["tick_size"])
    b_price = aggressive_limit_price(b_side, b_ticker["bid1"], b_ticker["ask1"], b_instr["tick_size"])
    a_qty = sized_qty(per_leg_notional_usdt, a_ticker["last"], a_instr["qty_step"], a_instr["min_qty"], a_instr["min_notional"])
    b_qty = sized_qty(per_leg_notional_usdt, b_ticker["last"], b_instr["qty_step"], b_instr["min_qty"], b_instr["min_notional"])

    ts = int(time.time())
    a_link = f"{slug}-a-{ts}"
    b_link = f"{slug}-b-{ts}"

    logging.info("placing pair entry side=%s z=%.4f per_leg_notional=%s", side.value, latest_z, per_leg_notional_usdt)
    a_res = client.place_limit_order(symbol=params.leg_a, side=a_side, qty=fmt_decimal(a_qty), price=fmt_decimal(a_price), reduce_only=False, order_link_id=a_link)
    try:
        b_res = client.place_limit_order(symbol=params.leg_b, side=b_side, qty=fmt_decimal(b_qty), price=fmt_decimal(b_price), reduce_only=False, order_link_id=b_link)
    except Exception:
        try:
            client.cancel_order(symbol=params.leg_a, order_link_id=a_link)
        except Exception:
            pass
        raise

    a_state = wait_for_terminal_state(client, params.leg_a, a_link)
    b_state = wait_for_terminal_state(client, params.leg_b, b_link)
    if not a_state or not b_state:
        raise RuntimeError("timed out waiting for entry order states")
    if a_state.get("orderStatus") != "Filled" or b_state.get("orderStatus") != "Filled":
        try:
            client.cancel_order(symbol=params.leg_a, order_link_id=a_link)
        except Exception:
            pass
        try:
            client.cancel_order(symbol=params.leg_b, order_link_id=b_link)
        except Exception:
            pass
        raise RuntimeError(f"entry did not fully fill: a={a_state.get('orderStatus')} b={b_state.get('orderStatus')}")

    return PositionState(
        side=side,
        opened_at_ms=opened_at_ms,
        entry_z=latest_z,
        a_qty=str(a_state.get("cumExecQty") or a_state.get("qty")),
        b_qty=str(b_state.get("cumExecQty") or b_state.get("qty")),
        a_entry=str(a_state.get("avgPrice") or a_state.get("price")),
        b_entry=str(b_state.get("avgPrice") or b_state.get("price")),
        a_order_id=a_res["result"]["orderId"],
        b_order_id=b_res["result"]["orderId"],
        breakeven_armed=False,
        per_leg_notional_usdt=fmt_decimal(per_leg_notional_usdt),
    )


def close_pair_position(client: BybitClient, params: PairParams, position: PositionState) -> None:
    slug = pair_slug(params)
    a_ticker = client.ticker(params.leg_a)
    b_ticker = client.ticker(params.leg_b)
    a_instr = client.instrument(params.leg_a)
    b_instr = client.instrument(params.leg_b)

    if position.side == PairSide.LONG_SPREAD:
        a_close_side, b_close_side = "Sell", "Buy"
    else:
        a_close_side, b_close_side = "Buy", "Sell"

    a_price = aggressive_limit_price(a_close_side, a_ticker["bid1"], a_ticker["ask1"], a_instr["tick_size"])
    b_price = aggressive_limit_price(b_close_side, b_ticker["bid1"], b_ticker["ask1"], b_instr["tick_size"])
    ts = int(time.time())
    a_link = f"{slug}-ca-{ts}"
    b_link = f"{slug}-cb-{ts}"

    logging.info("closing pair position side=%s", position.side.value)
    client.place_limit_order(symbol=params.leg_a, side=a_close_side, qty=position.a_qty, price=fmt_decimal(a_price), reduce_only=True, order_link_id=a_link)
    client.place_limit_order(symbol=params.leg_b, side=b_close_side, qty=position.b_qty, price=fmt_decimal(b_price), reduce_only=True, order_link_id=b_link)

    a_state = wait_for_terminal_state(client, params.leg_a, a_link)
    b_state = wait_for_terminal_state(client, params.leg_b, b_link)
    if not a_state or not b_state:
        raise RuntimeError("timed out waiting for close order states")
    if a_state.get("orderStatus") != "Filled" or b_state.get("orderStatus") != "Filled":
        raise RuntimeError(f"close did not fully fill: a={a_state.get('orderStatus')} b={b_state.get('orderStatus')}")


def run_live(params: PairParams, env_path: str, state_path: str, loop_seconds: int, bot_id: str) -> None:
    state_file = Path(state_path)
    state = RuntimeState.from_file(state_file, params)
    client = BybitClient(env_path=env_path)
    engine = PairSignalEngine(params)

    logging.info(
        "starting pair bot pair=%s rr=%.2f fixed_per_leg_notional=%s risk_pct=%s breakeven=%s bot_id=%s",
        params.display_pair(),
        params.reward_risk_ratio(),
        params.per_leg_notional_usdt,
        params.risk_pct_of_equity,
        params.enable_breakeven,
        bot_id,
    )
    while True:
        try:
            snapshot = compute_latest_live_snapshot(client, params)
            latest_ms = snapshot["latest_ms"]
            latest_z = snapshot["latest_z"]
            signal = snapshot["signal"]
            state.last_loop_wall_time = time.time()
            state.last_seen_z = latest_z
            state.last_seen_signal = signal.value if signal else None
            if state.last_bar_ms == latest_ms:
                state.save(state_file)
                time.sleep(loop_seconds)
                continue
            logging.info("new closed bar bar_ms=%s z=%.4f signal=%s", latest_ms, latest_z, signal.value if signal else None)
            state.last_bar_ms = latest_ms

            open_positions = client.positions((params.leg_a, params.leg_b))
            if state.position is None and open_positions:
                logging.warning("exchange shows open positions but local state is empty; refusing to proceed until human review")
                state.save(state_file)
                time.sleep(loop_seconds)
                continue

            if state.position is None:
                state.last_guard_reason = None
                if signal is not None:
                    guard_reason = should_defer_to_higher_priority(
                        current_signal=signal,
                        current_symbols={params.leg_a, params.leg_b},
                        latest_ms=latest_ms,
                        higher_peers=higher_priority_snapshot(client, bot_id, latest_ms),
                    )
                    if guard_reason is not None:
                        state.last_guard_reason = guard_reason
                        logging.info("entry skipped reason=%s", guard_reason)
                    else:
                        wallet = client.wallet_balance()
                        per_leg_notional = risk_based_per_leg_notional(
                            params=params,
                            total_equity=wallet["total_equity"],
                            available_equity=wallet["available_equity"],
                            spread_sigma=snapshot["spread_sigma"],
                        )
                        state.position = place_pair_entry(client, params, signal, latest_z, latest_ms, per_leg_notional)
                        logging.info(
                            "pair entry filled side=%s per_leg_notional=%s total_equity=%s available_equity=%s",
                            state.position.side.value,
                            per_leg_notional,
                            wallet["total_equity"],
                            wallet["available_equity"],
                        )
            else:
                state.last_guard_reason = None
                if not state.position.breakeven_armed and engine.should_arm_breakeven(state.position.side, latest_z):
                    state.position.breakeven_armed = True
                    logging.info("breakeven armed side=%s z=%.4f", state.position.side.value, latest_z)
                age_bars = max(0, (latest_ms - state.position.opened_at_ms) // TF_MS)
                pnl_fraction = unrealized_pair_pnl_fraction(
                    state.position.side,
                    Decimal(state.position.a_entry),
                    Decimal(state.position.b_entry),
                    snapshot["a_close"],
                    snapshot["b_close"],
                    params.fee_per_leg,
                )
                reason = engine.exit_reason(
                    state.position.side,
                    latest_z,
                    int(age_bars),
                    breakeven_armed=state.position.breakeven_armed,
                    pnl_fraction=pnl_fraction,
                )
                logging.info(
                    "position open age_bars=%s breakeven_armed=%s pnl_fraction=%s exit_reason=%s",
                    age_bars,
                    state.position.breakeven_armed,
                    pnl_fraction,
                    reason,
                )
                if reason is not None:
                    close_pair_position(client, params, state.position)
                    logging.info("pair position closed reason=%s", reason)
                    state.position = None

            state.save(state_file)
        except Exception as exc:
            logging.exception("loop failed: %s", exc)
        time.sleep(loop_seconds)


def main() -> None:
    parser = argparse.ArgumentParser(description="Pairs statistical arbitrage research harness and testnet bot")
    sub = parser.add_subparsers(dest="cmd", required=True)
    default_profile = default_active_profile()
    default_params = default_profile['params']

    p_backtest = sub.add_parser("backtest")
    p_backtest.add_argument("--db", default="/home/acekavi/Projects/Crypto/data/history.db")
    p_backtest.add_argument("--split-pct", type=float, default=0.70)

    p_live = sub.add_parser("live")
    p_live.add_argument("--env", default="/home/acekavi/Projects/Crypto/.env")
    p_live.add_argument("--state", default=str(default_profile['state_path']))
    p_live.add_argument("--loop-seconds", type=int, default=60)
    p_live.add_argument("--log-file", default=str(default_profile['log_path']))
    p_live.add_argument("--bot-id", default=default_profile['bot_id'])

    p_backtest.add_argument("--leg-a", default="DOGEUSDT")
    p_backtest.add_argument("--leg-b", default="XRPUSDT")
    p_backtest.add_argument("--timeframe", default="60")
    p_backtest.add_argument("--rolling-window", type=int, default=240)
    p_backtest.add_argument("--entry-z", type=float, default=3.5)
    p_backtest.add_argument("--stop-z", type=float, default=4.5)
    p_backtest.add_argument("--target-z", type=float, default=0.5)
    p_backtest.add_argument("--max-hold-bars", type=int, default=72)
    p_backtest.add_argument("--fee-per-leg", type=float, default=0.0002)
    p_backtest.add_argument("--per-leg-notional-usdt", default="25")
    p_backtest.add_argument("--risk-pct-of-equity", default="0.02")
    p_backtest.add_argument("--cap-per-leg-to-available-equity", default="true")
    p_backtest.add_argument("--enable-breakeven", default="true")
    p_backtest.add_argument("--breakeven-r-multiple", default="2")

    p_live.add_argument("--leg-a", default=default_params.leg_a)
    p_live.add_argument("--leg-b", default=default_params.leg_b)
    p_live.add_argument("--timeframe", default=default_params.timeframe)
    p_live.add_argument("--rolling-window", type=int, default=default_params.rolling_window)
    p_live.add_argument("--entry-z", type=float, default=default_params.entry_z)
    p_live.add_argument("--stop-z", type=float, default=default_params.stop_z)
    p_live.add_argument("--target-z", type=float, default=default_params.target_z)
    p_live.add_argument("--max-hold-bars", type=int, default=default_params.max_hold_bars)
    p_live.add_argument("--fee-per-leg", type=float, default=default_params.fee_per_leg)
    p_live.add_argument("--per-leg-notional-usdt", default=str(default_params.per_leg_notional_usdt))
    p_live.add_argument("--risk-pct-of-equity", default=str(default_params.risk_pct_of_equity))
    p_live.add_argument("--cap-per-leg-to-available-equity", default=str(default_params.cap_per_leg_to_available_equity).lower())
    p_live.add_argument("--enable-breakeven", default=str(default_params.enable_breakeven).lower())
    p_live.add_argument("--breakeven-r-multiple", default=str(default_params.breakeven_r_multiple))

    args = parser.parse_args()
    configure_logging(getattr(args, "log_file", None))
    params = params_from_args(args)

    if args.cmd == "backtest":
        times, _, _ = load_pair_series_from_db(args.db, params)
        split_idx = int(len(times) * args.split_pct)
        split_ms = times[split_idx]
        full = backtest(args.db, params)
        train = backtest(args.db, params, end_ms=split_ms)
        hold = backtest(args.db, params, start_ms=split_ms)
        summary = {
            "params": dataclasses.asdict(params),
            "pair": full["pair"],
            "train": {k: v for k, v in train.items() if k != "trades_detail"},
            "holdout": {k: v for k, v in hold.items() if k != "trades_detail"},
            "full": {k: v for k, v in full.items() if k != "trades_detail"},
        }
        print(json.dumps(json_ready(summary), indent=2, sort_keys=True))
        return

    if args.cmd == "live":
        run_live(params, args.env, args.state, args.loop_seconds, args.bot_id)


if __name__ == "__main__":
    main()
