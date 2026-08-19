# M5 Range Breakout with Pullback Entry — REJECTED

**Status: REJECTED.** No configuration is viable. The single profitable cell is a lone peak
surrounded by failures on both sides, and it breaches the drawdown limit anyway.

## The rules tested

Written out because "breakout pullback" names a family of ideas, and this result validates these
definitions only.

1. Range = highest high and lowest low of the previous **20 M5 candles**, excluding the current one.
2. Breakout = a **close** beyond that range by at least `min_break_atr` ATRs (ATR period 14).
   Requiring displacement rather than a bare touch is what separates a breakout from a wick.
3. Entry rests at **the broken level itself** — the retest. A limit order by construction, so a fill
   pays the maker fee (0.02%).
4. Stop = the breakout candle's **opposite extreme**. A candle entirely clear of the range leaves the
   stop on the wrong side of the entry and is refused rather than resized.
5. One breakout produces at most one entry. Entry expires after 12 candles.

Risk 1% of equity, max 8 concurrent, max 5 entries/day, 8 Bybit perps, 1,100 days, maker 0.02%.

## Result

Best cell per filter width:

| min_break_atr | trades | win % | PF | maxDD | net |
|---|---|---|---|---|---|
| 0.25 | 1,973 | 14.2 | 0.895 | 76.8% | −7,148 |
| 0.50 | 1,174 | 14.7 | 0.958 | 52.9% | −2,803 |
| **1.00** | **419** | **16.7** | **1.078** | **31.6%** | **+2,461** |
| 1.50 | 174 | 14.9 | 0.887 | 25.8% | −1,396 |
| 2.00 | 86 | 11.6 | 0.606 | 27.4% | −2,244 |
| 3.00 | 30 | 16.7 | 0.929 | 8.0% | −147 |

## Why the one positive cell is not a finding

**PF peaks at exactly one filter width and collapses on both sides.** A filter capturing something
real would improve or plateau as it tightened. Instead 1.0 spikes to 1.078 while 0.5 sits at 0.958
and 1.5 at 0.887. That is the shape of a lucky cell, not a relationship.

At 419 trades with 1:5 payoffs the variance is large enough that PF 1.078 is comfortably inside
noise. And it carries **31.6% max drawdown against a 20% limit**, so it fails the risk envelope on
its own terms even if the number were believed.

## Mechanism

The pattern matches the Supertrend M5 study exactly: the tighter the filter, the fewer the trades and
the less bad the result, up to the point where the sample gets too small to mean anything. What looks
like improvement is mostly reduced turnover, not better selection. At 0.25 ATR the strategy takes
~2,000 trades and loses 73% of the account.

Entering on a limit retest earns the maker fee rather than paying taker, which is the one structural
advantage this design has over a market-entry breakout — and it still is not enough.

## Ruled out

Sweeping further filter widths, other lookbacks, or other reward multiples in search of a second
positive cell. The peak-and-collapse shape is the answer; more cells would only produce another peak
of the same kind.
