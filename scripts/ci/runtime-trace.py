#!/usr/bin/env python3
"""Record command spans and runner-wide CPU/network deltas without collecting arguments."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def counters():
    fields = list(map(int, Path('/proc/stat').read_text().splitlines()[0].split()[1:9]))
    interfaces = {row.split()[0] for row in Path('/proc/net/route').read_text().splitlines()[1:]
                  if row.split()[1] == '00000000'}
    links = [line.split(':')[1].split() for line in Path('/proc/net/dev').read_text().splitlines()[2:]
             if line.split(':')[0].strip() in interfaces]
    return sum(fields), fields[3] + fields[4], sum(int(f[0]) for f in links), sum(int(f[8]) for f in links)


def record(path, label, command):
    before = counters()
    start = time.monotonic_ns() // 1000
    rc = 127
    try:
        rc = subprocess.run(command).returncode
    finally:
        end = time.monotonic_ns() // 1000
        total, idle, rx, tx = [b-a for a,b in zip(before, counters())]
        event = dict(name=label, ph='X', pid=os.getpid(), tid=1, ts=start, dur=end-start,
                     args=dict(exit_code=rc, runner_busy_pct=round(100*(total-idle)/total,1) if total else None,
                               runner_rx_bytes=rx, runner_tx_bytes=tx))
        with path.open('a') as stream:
            stream.write(json.dumps(event)+'\n')
    return rc if rc >= 0 else 128-rc


def summarize(path):
    events = [json.loads(line) for line in path.read_text().splitlines()]
    for event in events:
        event['pid'] = 1
        event['tid'] = 1
    trace = dict(traceEvents=events, repository=os.getenv('GITHUB_REPOSITORY'), commit=os.getenv('GITHUB_SHA'),
                 run=os.getenv('GITHUB_RUN_ID'), attempt=os.getenv('GITHUB_RUN_ATTEMPT'), cpus=os.cpu_count())
    path.with_suffix('.json').write_text(json.dumps(trace)+'\n')
    report = ['### Longest instrumented Linux commands', '', '|Command|Seconds|Runner busy|Network received (MiB)|Exit|', '|---|---:|---:|---:|---:|']
    for event in sorted(events,key=lambda e:-e['dur'])[:20]:
        args = event['args']
        busy = 'n/a' if args['runner_busy_pct'] is None else f"{args['runner_busy_pct']}%"
        report.append(f"|{event['name']}|{event['dur']/1e6:.2f}|{busy}|{args['runner_rx_bytes']/2**20:.2f}|{args['exit_code']}|")
    print('\n'.join(report))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('path',type=Path)
    parser.add_argument('label',nargs='?')
    parser.add_argument('command',nargs=argparse.REMAINDER)
    options = parser.parse_args()
    if options.command:
        sys.exit(record(options.path,options.label,options.command))
    summarize(options.path)
