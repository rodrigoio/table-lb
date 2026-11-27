"""Per-burst summary of a periodic-burst campaign (scenario c6): short vs long term.

The first burst shows the short term, the last one the long term; the steady
producer p99 is measured in the quiet period before the first burst and in the
one before the last burst. Needs the raw data of the runs (raw/ is not versioned).

usage: python3 bench/burst_report.py results/<campaign> [label ...]
"""
import collections
import csv
import glob
import os
import re
import sys


def pct(values, p):
    values = sorted(values)
    return values[min(len(values) - 1, int(p / 100 * len(values)))] if values else float("nan")


def meta(run):
    out = {}
    for line in open(os.path.join(run, "meta.env")):
        if "=" in line and not line.startswith("#"):
            k, v = line.rstrip("\n").split("=", 1)
            out[k] = v.strip('"')
    return out


def pgbench_log(raw, role):
    """(end_ms, latency_ms) of every sampled transaction of <role>[.-]*."""
    tx = []
    for path in glob.glob(os.path.join(raw, f"{role}.[0-9]*")):
        for line in open(path):
            p = line.split()
            if len(p) >= 6 and p[2].isdigit():
                tx.append((int(p[4]) * 1000 + int(p[5]) / 1000, int(p[2]) / 1000))
    return tx


def tps_of(raw, role):
    path = os.path.join(raw, f"{role}.out")
    if not os.path.exists(path):
        return float("nan"), 0
    text = open(path).read()
    tps = re.search(r"^tps = ([\d.]+)", text, re.M)
    done = re.search(r"actually processed: (\d+)", text)
    return (float(tps.group(1)) if tps else float("nan")), (int(done.group(1)) if done else 0)


def table_series(raw):
    by_ts = collections.defaultdict(dict)
    for r in csv.DictReader(open(os.path.join(raw, "collector", "tables.csv"))):
        by_ts[int(r["ts_ms"])][r["table"]] = r
    return by_ts


def analyse(run):
    m = meta(run)
    raw = os.path.join(run, "raw")
    t0 = int(m["load_start_ms"])
    t_end = int(m["load_end_ms"])
    at, every, count, dur = (int(m.get(k, d)) for k, d in
                             (("BURST_AT", 120), ("BURST_EVERY", 300), ("BURST_COUNT", 4), ("BURST_DURATION", 60)))
    res = {"label": m["label"]}

    prod = pgbench_log(raw, "producer")
    batches = list(csv.DictReader(open(os.path.join(raw, "collector", "batch_log.csv"))))
    res["prod_tps"], prod_n = tps_of(raw, "producer")
    res["prod_p50"] = pct([l for _, l in prod], 50)
    res["prod_p99"] = pct([l for _, l in prod], 99)

    bursts = []
    total_burst = 0
    for i in range(1, count + 1):
        role = "burst" if count == 1 else f"burst-{i}"
        tps, n = tps_of(raw, role)
        total_burst += n
        blog = pgbench_log(raw, role)
        if blog:
            b0 = min(e - l for e, l in blog)
            b1 = max(e for e, _ in blog)
        else:
            b0 = t0 + (at + (i - 1) * every) * 1000
            b1 = b0 + dur * 1000
        # the burst period: from burst start until the next burst (or end of load)
        p_end = t0 + (at + i * every) * 1000 if i < count else t_end
        in_burst = [l for e, l in prod if b0 <= e <= b1]
        lag = [float(r["lag_p95_ms"]) / 1000 for r in batches
               if b0 <= int(r["finished_ms"]) <= p_end]
        bursts.append({
            "tps": tps,
            "burst_p50": pct([l for _, l in blog], 50),
            "burst_p99": pct([l for _, l in blog], 99),
            "prod_p99_in_burst": pct(in_burst, 99),
            "lag_p95_max": max(lag) if lag else float("nan"),
        })
    res["bursts"] = bursts
    res["events"] = prod_n + total_burst

    # steady producer latency: first quiet period (60 s .. first burst) vs the
    # quiet period before the last burst
    def quiet(a, b):
        return pct([l for e, l in prod if t0 + a * 1000 <= e < t0 + b * 1000], 99)
    res["steady_p99_first"] = quiet(60, at)
    last = at + (count - 1) * every
    res["steady_p99_last"] = quiet(last - every + dur + 30, last)

    # e2e lag outside bursts (whole run) and steady
    lags = [float(r["lag_p95_ms"]) / 1000 for r in batches]
    res["lag_p95_median_batch"] = pct(lags, 50)
    res["lag_p95_max"] = max(lags) if lags else float("nan")
    res["cycles"] = len(batches)

    # readers
    reader = pgbench_log(raw, "reader")
    res["reader_p99"] = pct([l for _, l in reader], 99)

    # tables
    series = table_series(raw)
    ts = sorted(series)
    inbound = ["inbox"] if "inbox" in series[ts[0]] else ["buffer_0", "buffer_1"]
    def inb(k, col):
        return sum(int(series[k][t][col]) for t in inbound if t in series[k])
    res["inbound_mb_max"] = max(inb(k, "total_bytes") for k in ts) / 1e6
    res["inbound_mb_end"] = inb(ts[-1], "total_bytes") / 1e6
    res["inbound_dead_max"] = max(inb(k, "n_dead_tup") for k in ts)
    res["orders_dead_max"] = max(int(series[k]["orders"]["n_dead_tup"]) for k in ts)
    av = series[ts[-1]]
    res["autovac_s"] = sum(float(v["autovacuum_time_ms"]) for v in av.values()) / 1000

    db = list(csv.DictReader(open(os.path.join(raw, "collector", "database.csv"))))
    res["wal_mb"] = (int(db[-1]["wal_bytes"]) - int(db[0]["wal_bytes"])) / 1e6
    res["wal_per_event"] = (int(db[-1]["wal_bytes"]) - int(db[0]["wal_bytes"])) / max(res["events"], 1)

    cpu = collections.defaultdict(float)
    secs = set()
    for r in csv.DictReader(open(os.path.join(raw, "docker_stats.csv")), delimiter=";"):
        t = int(r["ts_ms"])
        if t0 <= t <= t_end and "postgres" in r["name"]:
            cpu[t // 1000] += float(r["cpu_perc"].rstrip("%") or 0)
            secs.add(t // 1000)
    res["cpu_pg"] = sum(cpu.values()) / max(len(secs), 1)
    res["drain_s"] = (int(m["drained_ms"]) - t_end) / 1000
    return res


def main():
    campaign = sys.argv[1]
    labels = sys.argv[2:] or sorted(os.listdir(os.path.join(campaign, "c6")))
    runs = [analyse(os.path.join(campaign, "c6", l, "rep-1")) for l in labels
            if os.path.exists(os.path.join(campaign, "c6", l, "rep-1", "meta.env"))]
    w = 12
    head = f"{'':34s}" + "".join(f"{r['label']:>{w}s}" for r in runs)
    print(head)

    def row(name, fn, fmt="{:.1f}"):
        print(f"{name:34s}" + "".join(f"{fmt.format(fn(r)):>{w}s}" for r in runs))

    row("events ingested", lambda r: r["events"], "{:,.0f}")
    row("producer tx/s", lambda r: r["prod_tps"], "{:.0f}")
    row("producer p50 (ms)", lambda r: r["prod_p50"], "{:.2f}")
    row("producer p99 (ms)", lambda r: r["prod_p99"])
    row("steady p99, start (ms)", lambda r: r["steady_p99_first"])
    row("steady p99, end (ms)", lambda r: r["steady_p99_last"])
    n = max(len(r["bursts"]) for r in runs)
    for i in range(n):
        row(f"burst {i+1}: tx/s", lambda r: r["bursts"][i]["tps"], "{:.0f}")
        row(f"burst {i+1}: burst p99 (ms)", lambda r: r["bursts"][i]["burst_p99"], "{:.0f}")
        row(f"burst {i+1}: producer p99 (ms)", lambda r: r["bursts"][i]["prod_p99_in_burst"], "{:.0f}")
        row(f"burst {i+1}: lag p95 max (s)", lambda r: r["bursts"][i]["lag_p95_max"])
    row("lag p95 per batch, median (s)", lambda r: r["lag_p95_median_batch"], "{:.2f}")
    row("lag p95 per batch, max (s)", lambda r: r["lag_p95_max"])
    row("batches / cycles", lambda r: r["cycles"], "{:.0f}")
    row("reader p99 (ms)", lambda r: r["reader_p99"])
    row("inbound size max (MB)", lambda r: r["inbound_mb_max"], "{:.0f}")
    row("inbound size end (MB)", lambda r: r["inbound_mb_end"], "{:.0f}")
    row("inbound dead tuples max", lambda r: r["inbound_dead_max"], "{:,.0f}")
    row("orders dead tuples max", lambda r: r["orders_dead_max"], "{:,.0f}")
    row("autovacuum (s)", lambda r: r["autovac_s"])
    row("WAL total (MB)", lambda r: r["wal_mb"], "{:.0f}")
    row("WAL per event (B)", lambda r: r["wal_per_event"], "{:.0f}")
    row("CPU postgres (% of 1 core)", lambda r: r["cpu_pg"], "{:.0f}")
    row("drain after load (s)", lambda r: r["drain_s"])


if __name__ == "__main__":
    main()
