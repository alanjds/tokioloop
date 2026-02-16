#!/usr/bin/env python3
import datetime
import json
import multiprocessing
import os
import signal
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path
import logging
import click

logger = logging.getLogger(__name__)


WD = Path(__file__).resolve().parent
CPU = multiprocessing.cpu_count()
ALL_LOOPS = ['asyncio', 'rloop', 'tokioloop', 'uvloop']
BASELINE_LOOPS = ['asyncio', 'rloop', 'uvloop']
TOKIOLOOP_ONLY = ['tokioloop']
MSGS = [1024, 1024 * 10, 1024 * 100]
CONCURRENCIES = sorted({1, max(CPU / 2, 1), max(CPU - 1, 1)})


@contextmanager
def server(loop, streams=False, proto=False, gil=None):
    exc_prefix = os.environ.get('BENCHMARK_EXC_PREFIX')
    py = 'python'
    if exc_prefix:
        py = f'{exc_prefix}/{py}'
    target = WD / 'server.py'
    proc_cmd = f'{py} {target} --loop {loop}'
    if streams:
        proc_cmd += ' --streams'
    if proto:
        proc_cmd += ' --proto'

    # Prepare environment with PYTHON_GIL if specified
    env = os.environ.copy()
    if gil is not None:
        env['PYTHON_GIL'] = str(gil)
        click.echo(f'SERVER: PYTHON_GIL={gil} {proc_cmd}')
    else:
        click.echo(f'SERVER: {proc_cmd}')

    proc = None
    try:
        proc = subprocess.Popen(proc_cmd, shell=True, preexec_fn=os.setsid, env=env)  # noqa: S602
        time.sleep(2)
        yield proc
    finally:
        if proc is not None and hasattr(proc, 'pid'):
            os.killpg(os.getpgid(proc.pid), signal.SIGTERM)
        click.echo('Server gone.')


def client(duration, concurrency, msgsize):
    exc_prefix = os.environ.get('BENCHMARK_EXC_PREFIX')
    py = 'python'
    if exc_prefix:
        py = f'{exc_prefix}/{py}'
    target = WD / 'client.py'
    cmd_parts = [
        py,
        str(target),
        f'--concurrency {concurrency}',
        f'--duration {duration}',
        f'--msize {msgsize}',
        '--output json',
    ]
    try:
        click.echo(f'CLIENT: {" ".join(cmd_parts)}')
        proc = subprocess.run(  # noqa: S602
            ' '.join(cmd_parts),
            shell=True,
            check=True,
            capture_output=True,
        )
        data = json.loads(proc.stdout.decode('utf8'))
        return data
    except Exception as e:
        click.echo(f'WARN: got exception {e} while loading client data', err=True)
        return {}


def benchmark(msgs=None, concurrencies=None):
    concurrencies = concurrencies or CONCURRENCIES
    msgs = msgs or MSGS
    results = {}
    # primer
    client(1, 1, 1024)
    time.sleep(1)
    # warm up
    client(1, max(concurrencies), 1024 * 100)
    time.sleep(2)
    # bench
    for concurrency in concurrencies:
        cres = results[concurrency] = {}
        for msg in msgs:
            res = client(10, concurrency, msg)
            cres[msg] = res
            time.sleep(3)
        time.sleep(1)
    return results


def run_benchmark_for_loop(loop, benchmark_fn, gil_modes):
    """Run benchmark for a single loop, handling GIL modes for tokioloop."""
    if loop == 'tokioloop' and gil_modes:
        # Run tokioloop with each GIL mode
        results = {}
        for gil in gil_modes:
            gil_label = 'gil' if gil == 1 else 'nogil'
            result = benchmark_fn(loop, gil)
            results[f'tokioloop:{gil_label}'] = result
        return results
    else:
        # Run other loops normally (no GIL mode)
        return benchmark_fn(loop, None)


def raw(loops, gil_modes=None):
    results = {}
    for loop in loops:

        def benchmark_fn(l, g):
            with server(l, gil=g):
                return benchmark(concurrencies=[CONCURRENCIES[0]])

        loop_results = run_benchmark_for_loop(loop, benchmark_fn, gil_modes)
        if isinstance(loop_results, dict):
            results.update(loop_results)
        else:
            results[loop] = loop_results
    return results


def stream(loops, gil_modes=None):
    results = {}
    for loop in loops:

        def benchmark_fn(l, g):
            with server(l, streams=True, gil=g):
                return benchmark(concurrencies=[CONCURRENCIES[0]])

        loop_results = run_benchmark_for_loop(loop, benchmark_fn, gil_modes)
        if isinstance(loop_results, dict):
            results.update(loop_results)
        else:
            results[loop] = loop_results
    return results


def proto(loops, gil_modes=None):
    results = {}
    for loop in loops:

        def benchmark_fn(l, g):
            with server(l, proto=True, gil=g):
                return benchmark(concurrencies=[CONCURRENCIES[0]])

        loop_results = run_benchmark_for_loop(loop, benchmark_fn, gil_modes)
        if isinstance(loop_results, dict):
            results.update(loop_results)
        else:
            results[loop] = loop_results
    return results


def concurrency(loops, gil_modes=None):
    results = {}
    for loop in loops:

        def benchmark_fn(l, g):
            with server(l, gil=g):
                return benchmark(msgs=[1024], concurrencies=CONCURRENCIES[1:])

        loop_results = run_benchmark_for_loop(loop, benchmark_fn, gil_modes)
        if isinstance(loop_results, dict):
            results.update(loop_results)
        else:
            results[loop] = loop_results
    return results


def _rloop_version():
    import rloop

    return rloop.__version__


BENCHMARKS = {
    'raw': raw,
    'stream': stream,
    'streams': stream,  # accept as a typo
    'proto': proto,
    'concurrency': concurrency,
}


@click.command()
@click.option(
    '--baseline',
    is_flag=True,
    help='Run baseline benchmarks (asyncio, rloop, uvloop) and save to baseline.json',
)
@click.option(
    '--gil',
    type=click.Choice(['0', '1']),
    help='GIL mode for tokioloop: 0 (nogil), 1 (gil), or omit for both (default)',
)
@click.argument(
    'benchmarks',
    nargs=-1,
    type=click.Choice(list(BENCHMARKS.keys()), case_sensitive=False),
)
def main(baseline, gil, benchmarks):
    """Run TokioLoop benchmarks.

    BENCHMARKS: One or more of: raw, stream, proto, concurrency (default: raw)
    """
    run_benchmarks = set(benchmarks) if benchmarks else {'raw'}

    # Determine which loops to run based on --baseline flag
    if baseline:
        loops = BASELINE_LOOPS
        output_file = WD / 'results' / 'baseline.json'
        if gil is not None:
            click.echo('Warning: --gil is ignored when running baseline benchmarks', err=True)
            gil = None
    else:
        loops = TOKIOLOOP_ONLY
        output_file = WD / 'results' / 'data.json'

    # Determine GIL modes to test
    if gil is None:
        # Default: run both GIL modes
        gil_modes = [0, 1]
    else:
        # Run specific GIL mode
        gil_modes = [int(gil)]

    now = datetime.datetime.utcnow()
    results = {}
    for benchmark_key in run_benchmarks:
        runner = BENCHMARKS[benchmark_key]
        results[benchmark_key] = runner(loops, gil_modes)

    output_file.parent.mkdir(parents=True, exist_ok=True)
    with open(output_file, 'w') as f:
        pyver = sys.version_info
        f.write(
            json.dumps(
                {
                    'cpu': CPU,
                    'run_at': int(now.timestamp()),
                    'pyver': f'{pyver.major}.{pyver.minor}',
                    'results': results,
                    'rloop': _rloop_version(),
                },
                indent=2,
            )
        )

    click.echo(f'Results saved to {output_file}')


if __name__ == '__main__':
    main()
