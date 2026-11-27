"""Measure title Present intervals ending in a verified playable time window."""

import argparse
from datetime import datetime
import json
import math
from pathlib import Path
import re
from statistics import median


PRESENT = re.compile(
    r"(\d{4}-\d\d-\d\dT\S+Z)\s+INFO victory_gpu::queue: "
    r"title present interval frame_interval_us=(\d+)\b"
)


def timestamp(value):
    moment = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if moment.tzinfo is None:
        raise ValueError("supply a UTC timestamp or an explicit offset")
    return moment


def measure(lines, start, end):
    """Select whole log records by the time the title reported each interval.

    >>> stamps = [f"2026-11-27T00:00:0{i}Z" for i in range(4)]
    >>> prefix = " INFO victory_gpu::queue: title present interval frame_interval_us="
    >>> lines = [s + prefix + str(v) for s, v in zip(stamps, (1, 20000, 30000, 99999))]
    >>> result = measure(lines, timestamp(stamps[1]), timestamp(stamps[2]))
    >>> result["intervals"], result["median_fps"], result["p95_ms"]
    (2, 40.0, 30.0)
    """
    if end <= start:
        raise ValueError("end must be later than start")
    intervals = []
    first = last = None
    for line in lines:
        if match := PRESENT.search(line):
            moment = timestamp(match[1])
            if first is None:
                first = moment
            last = moment
            if start <= moment <= end:
                intervals.append(int(match[2]))
    if first is None or first > start or last < end:
        raise ValueError("the title Present log does not span the requested window")
    if len(intervals) < 2 or min(intervals) <= 0:
        raise ValueError("expected at least two positive title Present intervals")
    intervals.sort()
    middle = median(intervals)
    return {
        "start": start.isoformat(),
        "end": end.isoformat(),
        "intervals": len(intervals),
        "median_fps": round(1_000_000 / middle, 3),
        "median_ms": round(middle / 1000, 3),
        "p95_ms": intervals[math.ceil(len(intervals) * 0.95) - 1] / 1000,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("start", type=timestamp)
    parser.add_argument("end", type=timestamp)
    args = parser.parse_args()
    try:
        with args.log.open(encoding="utf-8", errors="replace") as log:
            result = measure(log, args.start, args.end)
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")
    print(json.dumps(result, indent=2))
