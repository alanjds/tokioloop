#!/usr/bin/env python3
"""
Benchmark Results Analyzer for TokioLoop

Usage:
    python analyze.py

Loads and analyzes results from results/baseline.json and results/data.json
"""

import json
from pathlib import Path

import click


def format_rps(rps):
    """Format RPS with commas."""
    return f'{rps:,.0f}'


def format_percent(pct):
    """Format percentage with one decimal."""
    return f'{pct:.1f}%'


def print_header(title, width=80):
    """Print a formatted header."""
    click.echo()
    click.echo('-' * width)
    click.echo(f'# {title}')
    click.echo('-' * width)


def print_subheader(title, width=80):
    """Print a formatted subheader."""
    click.echo()
    click.echo(f'## {title}')
    click.echo('-' * width)


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
    header = f'{"Event Loop":<20} {"1KB RPS":>12} {"10KB RPS":>12} {"100KB RPS":>12} {"Latency(ms)":>12} {"vs asyncio":>12} {"vs prev/rloop":>14}'
    click.echo(header)
    click.echo('-' * width)

    rows = []
    all_loops = [
        'asyncio',
        'uvloop',
        'rloop',
        'tokioloop:gil:prev',
        'tokioloop:gil',
        'tokioloop:nogil:prev',
        'tokioloop:nogil',
    ]

    for loop in all_loops:
        if loop not in bench_data:
            continue

        data = bench_data[loop]
        if '1' not in data:
            continue

        r1k = data['1'].get('1024', {}).get('rps', 0)
        r10k = data['1'].get('10240', {}).get('rps', 0)
        r100k = data['1'].get('102400', {}).get('rps', 0)
        latency = data['1'].get('1024', {}).get('latency_mean', 0)

        vs_asyncio = (r1k / asyncio_1k * 100) if asyncio_1k > 0 else 0

        # Determine comparison for second column
        # Compare with :prev version if exists, otherwise compare with rloop
        if ':prev' in loop:
            vs_prev_rloop = (r1k / rloop_1k * 100) if rloop_1k > 0 else 0
        else:
            prev_loop = f'{loop}:prev'
            if prev_loop in bench_data:
                prev_data = bench_data[prev_loop]
                prev_1k = prev_data['1'].get('1024', {}).get('rps', 0)
                vs_prev_rloop = (r1k / prev_1k * 100) if prev_1k > 0 else 0
            else:
                vs_prev_rloop = (r1k / rloop_1k * 100) if rloop_1k > 0 else 0

        row = {
            'loop': loop.strip(),
            'r1k': r1k,
            'r10k': r10k,
            'r100k': r100k,
            'latency': latency,
            'vs_asyncio': vs_asyncio,
            'vs_prev_rloop': vs_prev_rloop,
        }
        rows.append(row)

        click.echo(
            f'{loop:<20} {format_rps(r1k):>12} {format_rps(r10k):>12} {format_rps(r100k):>12} '
            f'{latency:>12.3f} {format_percent(vs_asyncio):>12} {format_percent(vs_prev_rloop):>14}'
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

        # Prefer tokioloop:nogil, fallback to tokioloop:gil
        tokioloop = next((r for r in rows if r['loop'] == 'tokioloop:nogil'), None)
        if tokioloop is None:
            tokioloop = next((r for r in rows if r['loop'] == 'tokioloop:gil'), None)
        rloop = next((r for r in rows if r['loop'] == 'rloop'), None)

        if tokioloop and rloop:
            avg_tokio = (tokioloop['r1k'] + tokioloop['r10k'] + tokioloop['r100k']) / 3
            avg_rloop = (rloop['r1k'] + rloop['r10k'] + rloop['r100k']) / 3

            click.echo(f'\n{benchmark_name.upper()}:')
            click.echo(
                f'  1KB:   {format_rps(tokioloop["r1k"]):>10} RPS ({format_percent(tokioloop["r1k"] / rloop["r1k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r1k"] * 0.8):>10} RPS'
            )
            click.echo(
                f'  10KB:  {format_rps(tokioloop["r10k"]):>10} RPS ({format_percent(tokioloop["r10k"] / rloop["r10k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r10k"] * 0.8):>10} RPS'
            )
            click.echo(
                f'  100KB: {format_rps(tokioloop["r100k"]):>10} RPS ({format_percent(tokioloop["r100k"] / rloop["r100k"] * 100)} of rloop)  '
                f'Target: {format_rps(rloop["r100k"] * 0.8):>10} RPS'
            )
            click.echo(
                f'  Latency: {tokioloop["latency"]:>8.3f} ms  ({tokioloop["latency"] / rloop["latency"]:.1f}x vs rloop)'
            )

            total_rps += avg_tokio
            total_rloop_rps += avg_rloop
            count += 1

    if count > 0:
        click.echo(
            f'\n  Overall Average: {format_rps(total_rps / count):>10} RPS ({format_percent(total_rps / total_rloop_rps * 100)} of rloop)'
        )


def load_and_merge_results():
    """Load baseline.json, prev.json and data.json, merging results from all."""
    results_dir = Path(__file__).parent / 'results'
    baseline_path = results_dir / 'baseline.json'
    prev_path = results_dir / 'prev.json'
    data_path = results_dir / 'data.json'

    # Load baseline data (asyncio, rloop, uvloop)
    baseline_data = {}
    if baseline_path.exists():
        with open(baseline_path) as f:
            baseline_data = json.load(f)
    else:
        click.echo(f'Warning: Baseline file not found: {baseline_path}', err=True)
        click.echo('Run: python benchmarks.py --baseline', err=True)

    # Load previous run data
    prev_data = {}
    if prev_path.exists():
        with open(prev_path) as f:
            prev_data = json.load(f)

    # Load current data (tokioloop)
    current_data = {}
    if data_path.exists():
        with open(data_path) as f:
            current_data = json.load(f)
    else:
        click.echo(f'Warning: Data file not found: {data_path}', err=True)
        click.echo('Run: python benchmarks.py', err=True)

    # Merge results: baseline loops + previous results + tokioloop
    merged_results = {}
    baseline_results = baseline_data.get('results', {})
    prev_results = prev_data.get('results', {})
    current_results = current_data.get('results', {})

    all_benchmarks = set(baseline_results.keys()) | set(prev_results.keys()) | set(current_results.keys())

    for benchmark_name in all_benchmarks:
        merged_results[benchmark_name] = {}

        # Add baseline loops (asyncio, rloop, uvloop)
        if benchmark_name in baseline_results:
            for loop in ['asyncio', 'rloop', 'uvloop']:
                if loop in baseline_results[benchmark_name]:
                    merged_results[benchmark_name][loop] = baseline_results[benchmark_name][loop]

        # Add prev data with :prev suffix
        if benchmark_name in prev_results:
            for loop_variant in ['tokioloop:gil', 'tokioloop:nogil']:
                if loop_variant in prev_results[benchmark_name]:
                    prev_loop_name = f'{loop_variant}:prev'
                    merged_results[benchmark_name][prev_loop_name] = prev_results[benchmark_name][loop_variant]

        # Add current tokioloop variants
        if benchmark_name in current_results:
            for loop_variant in ['tokioloop:gil', 'tokioloop:nogil']:
                if loop_variant in current_results[benchmark_name]:
                    merged_results[benchmark_name][loop_variant] = current_results[benchmark_name][loop_variant]

    # Use metadata from current data if available, otherwise from baseline
    metadata = current_data if current_data else baseline_data

    return merged_results, metadata


@click.command()
@click.option(
    '--width',
    type=int,
    default=100,
    show_default=True,
    help='Output width for the report',
)
def main(width):
    """Analyze TokioLoop benchmark results.

    Loads and analyzes results from results/baseline.json and results/data.json
    """
    # Load and merge results from baseline.json and data.json
    results, data = load_and_merge_results()

    if not results:
        raise click.ClickException(
            'No benchmark data found!\nRun baseline: python benchmarks.py --baseline\nRun current: python benchmarks.py'
        )

    # Print header
    print_header('TOKIOLOOP BENCHMARK ANALYSIS REPORT', width)
    click.echo(f'Python: {data.get("pyver", "unknown")}')
    click.echo(f'CPU: {data.get("cpu", "unknown")} cores')
    click.echo(f'Run at: {data.get("run_at", "unknown")}')
    click.echo(f'rloop version: {data.get("rloop", "unknown")}')

    # Analyze each benchmark type
    rows_by_benchmark = {}
    for benchmark_name in ['raw', 'proto', 'stream', 'concurrency']:
        rows = analyze_benchmark_type(results, benchmark_name, width)
        if rows:
            rows_by_benchmark[benchmark_name] = rows

    if not rows_by_benchmark:
        raise click.ClickException('No benchmark data found in results file!')

    # Print summaries
    print_tokio_summary(rows_by_benchmark, width)

    # Footer
    click.echo()
    click.echo('=' * width)


if __name__ == '__main__':
    main()
