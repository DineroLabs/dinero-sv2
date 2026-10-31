#!/usr/bin/env python3
"""Real immutable solo mining at Core's isolated coupled Orchard boundary.

Requires the checked Core coupled-cycle qualification daemon (activation 102),
not a production daemon or datadir. The Core test library enforces the profile,
mines the historical prefix with real PoW, and owns every disposable file.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time


def port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def outputs_from_block(encoded):
    data = bytes.fromhex(encoded)
    cursor = 128
    def compact():
        nonlocal cursor
        first = data[cursor]; cursor += 1
        size = {253: 2, 254: 4, 255: 8}.get(first, 0)
        if not size:
            return first
        value = int.from_bytes(data[cursor:cursor+size], 'little'); cursor += size
        return value
    assert compact() >= 1
    cursor += 4
    if data[cursor:cursor+2] == b'\x00\x01':
        cursor += 2
    assert compact() == 1
    cursor += 36
    length = compact(); cursor += length + 4
    outputs = []
    for _ in range(compact()):
        value = int.from_bytes(data[cursor:cursor+8], 'little'); cursor += 8
        length = compact(); script = data[cursor:cursor+length]; cursor += length
        outputs.append((value, script))
    return outputs


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--core-source', required=True)
    parser.add_argument('--daemon', required=True)
    parser.add_argument('--pool', required=True)
    parser.add_argument('--miner', required=True)
    parser.add_argument('--gpu-backend', choices=('metal', 'opencl', 'cuda'))
    args = parser.parse_args()
    sys.path.insert(0, str(Path(args.core_source).resolve() / 'tests/integration'))
    from test_orchard_daemon_wallet_cycle import Node
    class CoupledNode(Node):
        def wait(self, predicate, label, seconds=90):
            if label == 'actual JSON-RPC listener':
                original = predicate
                def predicate():
                    log = self.root / f'process-{self.starts}.log'
                    return ('READY Orchard wallet cycle JSON-RPC daemon' in log.read_text(errors='replace') and original())
            return super().wait(predicate, label, seconds)
        def stop(self):
            process = self.process
            if process is None:
                return
            assert process.poll() is None, 'fixture exited before requested stop'
            process.stdin.write(b'stop\n'); process.stdin.flush(); process.stdin.close()
            assert process.wait(timeout=45) == 0, 'fixture did not stop cleanly'
            self.process = None
            log = (self.root / f'process-{self.starts}.log').read_text(errors='replace')
            assert log.count('PASS Orchard wallet cycle daemon stopped normally') == 1
    binaries = {name: Path(getattr(args, name)).resolve() for name in ('daemon', 'pool', 'miner')}
    for binary in binaries.values():
        assert binary.is_file()
    node = CoupledNode(binaries['daemon'], binaries['daemon'], coupled_release=True)
    # The environment switch exists only in Core's qualification entry point.
    os.environ['DINERO_TEST_AUTOMATIC_STORAGE_UPGRADE'] = '1'
    pool = miner = None
    report = {'case': 'orchard-solo-' + (args.gpu_backend or 'cpu'),
              'evidence': str(node.root), 'production_changed': False,
              'binary_sha256': {key: hashlib.sha256(value.read_bytes()).hexdigest() for key, value in binaries.items()}}
    try:
        node.start()
        assert node.rpc('wallet.createhd', [node.wallet, 12, '', node.password])['success']
        assert node.rpc('wallet.unlock', [node.password, 3600])['success']
        fee_address = node.rpc('wallet.getnewaddress', ['taproot', 'isolated pool fee'])['address']
        node.generate(101, fee_address)
        assert node.rpc('getblockcount') == 101
        job = node.rpc('mining.getjob', {'address': fee_address})
        assert job['height'] == 102 and job['profile'] == 'orchard-v1'
        reward = job['coinbase_value_una']
        pool_port = port()
        key = node.root / 'pool.key'
        pubkey = subprocess.check_output([str(binaries['pool']), '--print-pubkey', '--tp-key', str(key)], text=True).strip()
        assert len(pubkey) == 64 and all(c in '0123456789abcdef' for c in pubkey)
        journal = node.root / 'pplns.jsonl'
        with (node.root / 'pool.log').open('wb') as log:
            pool = subprocess.Popen([str(binaries['pool']), '--ops-bind=', '--bind', f'127.0.0.1:{pool_port}',
                '--rpc-url', f'http://127.0.0.1:{node.ports[0]}', '--cookie', str(node.datadir / '.cookie'),
                '--payout-address', fee_address, '--tp-key', str(key), '--pplns-journal', str(journal),
                '--poll-secs', '1', '--refresh-same-tip-secs', '2', '--vardiff-target-seconds', '0',
                '--share-leading-bits', '0', '--state-commitment-height', '1', '--utreexo-maturity-leaf-height', '20'],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            assert pool.poll() is None, 'pool exited before listener'
            try:
                with socket.create_connection(('127.0.0.1', pool_port), timeout=.2):
                    break
            except OSError:
                time.sleep(.1)
        else:
            raise RuntimeError('pool listener deadline')
        journal_before = journal.read_bytes() if journal.exists() else b''
        payout = bytes.fromhex('5120' + '71' * 32)
        command = [str(binaries['miner']), '--pool', f'127.0.0.1:{pool_port}', '--server-pubkey', pubkey,
                   '--reward-mode', 'solo', '--payout-script-hex', payout.hex(), '--no-save', '--json',
                   '--reconnect-secs', '0', '--max-blocks', '1']
        command += ['--backend', args.gpu_backend, '--batch-size', '4096'] if args.gpu_backend else ['--threads', '1']
        with (node.root / 'miner.log').open('wb') as log:
            miner = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log, stderr=log)
        assert miner.wait(timeout=60) == 0, 'solo miner failed; inspect isolated miner.log'
        node.wait(lambda: node.rpc('getblockcount') >= 102, 'actual solo block validation', seconds=30)
        block_hash = node.rpc('getblockhash', [102])
        raw = node.rpc('getblock', [block_hash, 0])
        outputs = outputs_from_block(raw)
        paid = [(value, script) for value, script in outputs if not script.startswith(b'\x6a')]
        assert paid == [(reward, payout)], 'solo reward redirected or split'
        tags = [script[2:7] for value, script in outputs if script.startswith(b'\x6a') and value == 0]
        assert sorted(tags) == sorted([b'DNRS\x02', b'DNRW\x01', b'DNRF\x01'])
        assert (journal.read_bytes() if journal.exists() else b'') == journal_before, 'solo work entered PPLNS'
        report.update(result='PASS', height=102, block_hash=block_hash, reward_una=reward,
                      exact_miner_payout=True, pplns_unchanged=True)
    finally:
        for child in (miner, pool):
            if child and child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=10)
        try:
            node.stop()
        finally:
            (node.root / 'result.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report), flush=True)


if __name__ == '__main__':
    main()
