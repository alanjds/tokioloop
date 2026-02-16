#!/usr/bin/env python3
"""
Benchmark Results Analyzer for TokioLoop

Usage:
    python analyze_results.py [results_file.json]

If no file is specified, uses results/data.json
"""

import json
import sys
from pathlib import Path


def format_rps(rps):
    """Format RPS with commas."""
    return f'{rps:,.0f}'


def format_percent(pct):
    """Format percentage with one decimal."""
    return f'{pct:.1f}%'


def print_header(title, width=80):
    """Print a formatted header."""
    print()
    print('-' * width)
    print(f'# {title}')
    print('-' * width)


def print_subheader(title, width=80):
    """Print a formatted subheader."""
    print()
    print(f'## {title}')
    print('-' * width)


def analyze_benchmark_type(results, benchmark_name, width=80):
    """Analyze a single benchmark type (raw, proto, stream)."""
    if benchmark_name not in results:
        return None

    bench_data = results[benchmark_name]

    # Get baseline values
    asyncio_1k = bench_data.get('asyncio', {}).get('1', {}).get('1024', {}).get('rps', 0)
    rloop_1k = bench_data.get('rloop', {}).get('1', {}).get('1024', {}).get('rps', 0)

    if asyncio_1k == 0 or rloop_1k == 0:
        return None

    print_subheader(f'{benchmark_name.upper()} BENCHMARK', width)

    # Table header
    header = f'{"Event Loop":<15} {"1KB RPS":>12} {"10KB RPS":>12} {"100KB RPS":>12} {"Latency(ms)":>12} {"vs asyncio":>12} {"vs rloop":>12}'
    print(header)
    print('-' * width)

    rows = []
    for loop in ['asyncio', 'uvloop', 'rloop', 'tokioloop']:
        if loop in bench_data:
            data = bench_data[loop]
            if '1' not in data:
                continue

            r1k = data['1'].get('1024', {}).get('rps', 0)
            r10k = data['1'].get('10240', {}).get('rps', 0)
            r100k = data['1'].get('102400', {}).get('rps', 0)
            latency = data['1'].get('1024', {}).get('latency_mean', 0)

            vs_asyncio = (r1k / asyncio_1k * 100) if asyncio_1k > 0 else 0
            vs_rloop = (r1k / rloop_1k * 100) if rloop_1k > 0 else 0

            row = {
                'loop': loop,
                'r1k': r1k,
                'r10k': r10k,
                'r100k': r100k,
                'latency': latency,
                'vs_asyncio': vs_asyncio,
                'vs_rloop': vs_rloop,
            }
            rows.append(row)

            print(
                f'{loop:<10} {format_rps(r1k):>12} {format_rps(r10k):>12} {format_rps(r100k):>12} '
                f'{latency:>12.3f} {format_percent(vs_asyncio):>12} {format_percent(vs_rloop):>12}'
            )

    return rows


def print_tokio_summary(rows_by_benchmark, width=80):
    """Print TokioLoop specific summary."""
    print_subheader('TOKIOLOOP PERFORMANCE SUMMARY', width)

    total_rps = 0
    total_rloop_rps = 0
    count = 0

    for benchmark_name, rows in rows_by_benchmark.items():
        if not rows:
            continue

        tokioloop = next((r for r in rows if r['loop'] == 'tokioloop'), None)
        rloop = next((r for r in rows if r['loop'] == 'rloop'), None)

        if tokioloop and rloop:
            avg_tokio = (tokioloop['r1k'] + tokioloop['r10k'] + tokioloop['r100k']) / 3
            avg_rloop = (rloop['r1k'] + rloop['r10k'] + rloop['r100k']) / 3

            print(f'\n{benchmark_name.upper()}:')
            print(
                f'  1KB:   {format_rps(tokioloop["r1k"]):>10} RPS ({format_percent(tokioloop["r1k"] / rloop["r1k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r1k"] * 0.8):>10} RPS'
            )
            print(
                f'  10KB:  {format_rps(tokioloop["r10k"]):>10} RPS ({format_percent(tokioloop["r10k"] / rloop["r10k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r10k"] * 0.8):>10} RPS'
            )
            print(
                f'  100KB: {format_rps(tokioloop["r100k"]):>10} RPS ({format_percent(tokioloop["r100k"] / rloop["r100k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r100k"] * 0.8):>10} RPS'
            )
            print(
                f'  Latency: {tokioloop["latency"]:>8.3f} ms  ({tokioloop["latency"] / rloop["latency"]:.1f}x vs rloop)'
            )

            total_rps += avg_tokio
            total_rloop_rps += avg_rloop
            count += 1

    if count > 0:
        print(
            f'\n  Overall Average: {format_rps(total_rps / count):>10} RPS ({format_percent(total_rps / total_rloop_rps * 100)} of rloop)'
        )


def load_and_merge_results():
    """Load baseline.json and data.json, merging results from both."""
    results_dir = Path(__file__).parent / 'results'
    baseline_path = results_dir / 'baseline.json'
    data_path = results_dir / 'data.json'

    # Load baseline data (asyncio, rloop, uvloop)
    baseline_data = {}
    if baseline_path.exists():
        with open(baseline_path) as f:
            baseline_data = json.load(f)
    else:
        print(f'Warning: Baseline file not found: {baseline_path}')
        print('Run: python benchmarks.py --baseline')

    # Load current data (tokioloop)
    current_data = {}
    if data_path.exists():
        with open(data_path) as f:
            current_data = json.load(f)
    else:
        print(f'Warning: Data file not found: {data_path}')
        print('Run: python benchmarks.py')

    # Merge results: baseline loops + tokioloop
    merged_results = {}
    baseline_results = baseline_data.get('results', {})
    current_results = current_data.get('results', {})

    all_benchmarks = set(baseline_results.keys()) | set(current_results.keys())

    for benchmark_name in all_benchmarks:
        merged_results[benchmark_name] = {}

        # Add baseline loops (asyncio, rloop, uvloop)
        if benchmark_name in baseline_results:
            for loop in ['asyncio', 'rloop', 'uvloop']:
                if loop in baseline_results[benchmark_name]:
                    merged_results[benchmark_name][loop] = baseline_results[benchmark_name][loop]

        # Add tokioloop from current data
        if benchmark_name in current_results:
            if 'tokioloop' in current_results[benchmark_name]:
                merged_results[benchmark_name]['tokioloop'] = current_results[benchmark_name]['tokioloop']

    # Use metadata from current data if available, otherwise from baseline
    metadata = current_data if current_data else baseline_data

    return merged_results, metadata


def main():
    """Main analysis function."""
    # Load and merge results from baseline.json and data.json
    results, data = load_and_merge_results()

    if not results:
        print('Error: No benchmark data found!')
        print('Run baseline: python benchmarks.py --baseline')
        print('Run current: python benchmarks.py')
        sys.exit(1)

    # Print header
    print_header('TOKIOLOOP BENCHMARK ANALYSIS REPORT', 80)
    print(f'Python: {data.get("pyver", "unknown")}')
    print(f'CPU: {data.get("cpu", "unknown")} cores')
    print(f'Run at: {data.get("run_at", "unknown")}')
    print(f'rloop version: {data.get("rloop", "unknown")}')

    # Analyze each benchmark type
    rows_by_benchmark = {}
    for benchmark_name in ['raw', 'proto', 'stream', 'concurrency']:
        rows = analyze_benchmark_type(results, benchmark_name, 80)
        if rows:
            rows_by_benchmark[benchmark_name] = rows

    if not rows_by_benchmark:
        print('\n❌ No benchmark data found in results file!')
        sys.exit(1)

    # Print summaries
    print_tokio_summary(rows_by_benchmark, 80)

    # Footer
    print()
    print('=' * 80)


if __name__ == '__main__':
    main()
