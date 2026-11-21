#!/usr/bin/env python3
import argparse
import json
from pathlib import Path
from statistics import median

MIB = 1024 * 1024
DATA = (
    ('cold-read-MiBs', 'Cold read, MiB/s'),
    ('warm-read-MiBs', 'Repeated read, MiB/s'),
    ('write-with-flush-MiBs', 'Write with flush, MiB/s'),
    ('mixed-with-flush-IOPS', '4 KiB mixed with flush, IOPS'),
)


def spread(values, places=0):
    return f'{median(values):,.{places}f} ({min(values):,.{places}f}–{max(values):,.{places}f})'


def main():
    cli = argparse.ArgumentParser()
    cli.add_argument('results', type=Path)
    args = cli.parse_args()
    metadata = json.loads((args.results / 'metadata.json').read_text())
    rows = [json.loads(row) for row in (args.results / 'runs.jsonl').read_text().splitlines()]
    groups = {}
    for row in rows:
        key = row['io_engine'], row['direct_io']
        groups.setdefault(key, []).append(row)
    if len(rows) != sum(map(len, metadata['orders'])):
        raise SystemExit('Incomplete run')
    print('# Local Firecracker/KVM block I/O')
    print()
    print(f"Revision `{metadata['revision']}`")
    print(f"  · Binary SHA-256 `{metadata['binary_sha256']}`")
    print()
    print('Both configurations ran the same release executable.')
    print(f"The RAW file was on {metadata['backing_mount']}.")
    print('Rates show the median and min–max across the randomized repetitions.')
    print()
    print('| Engine | direct_io | Trials | ' + ' | '.join(label for _, label in DATA) + ' |')
    print('|---|---|---:|' + '---:|' * len(DATA))
    for (engine, direct), trials in sorted(groups.items()):
        rates = [spread([row['guest_rates'][key] for row in trials]) for key, _ in DATA]
        print(f'| {engine} | {str(direct).lower()} | {len(trials)} | ' + ' | '.join(rates) + ' |')
    print()
    print('| Engine | direct_io | Cold read p99, µs | Repeated read p99, µs | Sequential write p99, µs | 4 KiB mixed read p99, µs | 4 KiB mixed write p99, µs |')
    print('|---|---|---:|---:|---:|---:|---:|')
    for (engine, direct), trials in sorted(groups.items()):
        latency_keys = (
            ('cold-read-MiBs', 'read'), ('warm-read-MiBs', 'read'),
            ('write-with-flush-MiBs', 'write'), ('mixed-with-flush-IOPS', 'read'),
            ('mixed-with-flush-IOPS', 'write'),
        )
        latencies = [
            spread([row['guest_p99_us'][stage][direction] for row in trials])
            for stage, direction in latency_keys
        ]
        print(f'| {engine} | {str(direct).lower()} | ' + ' | '.join(latencies) + ' |')
    print()
    print('| Engine | direct_io | Cached read region after first pass, MiB | Cached write region after flush, MiB | Cached mixed region after flush, MiB | Host storage read bytes during repeat, MiB |')
    print('|---|---|---:|---:|---:|---:|')
    for (engine, direct), trials in sorted(groups.items()):
        regions = [
            ('cold-read-MiBs', 'sequential_read'),
            ('write-with-flush-MiBs', 'sequential_write'),
            ('mixed-with-flush-IOPS', 'mixed'),
        ]
        cached = [
            spread([row['page_cache_resident_bytes'][stage][region] / MIB for row in trials])
            for stage, region in regions
        ]
        counters = [row['host_process_storage_counters'] for row in trials]
        baseline = [
            (row['warm-read-MiBs']['read_bytes'] - row['cold-read-MiBs']['read_bytes']) / MIB
            for row in counters
        ]
        print(f'| {engine} | {str(direct).lower()} | ' + ' | '.join([*cached, spread(baseline)]) + ' |')
    print()
    write_bytes = [
        row['host_process_storage_counters']['write-with-flush-MiBs']['write_bytes']
        - row['host_process_storage_counters']['warm-read-MiBs']['write_bytes']
        for row in rows
    ]
    if set(write_bytes) == {128 * MIB}:
        print('Linux charged 128 MiB of storage writes to the VMM during the sequential write and flush in every trial.')
    if all(row['guest_readback_ok'] for row in rows):
        print(f'All {len(rows)} guests verified read data byte for byte and recorded two successful flushes.')
    checksums = sorted({row['full_file_sha256'] for row in rows})
    if len(checksums) == 1:
        print(f'All post-stop file checksums matched the independently calculated SHA-256 `{checksums[0]}`.')
    before = [row['host_load_before'][0] for row in rows]
    after = [row['host_load_after'][0] for row in rows]
    print(f"The host's one-minute system load ran from {min(before + after):.2f} to {max(before + after):.2f} across trials.")
    print()
    print('The [method](method.md) describes the workload and the captured data.')
    print(f"[Per-run data]({args.results.name}/runs.jsonl) · [Run metadata]({args.results.name}/metadata.json)")


if __name__ == '__main__':
    main()
