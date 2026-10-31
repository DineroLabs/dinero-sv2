#!/usr/bin/env python3
"""Mine Core coupled-fixture historical prefix through ordinary PoW admission."""
import sys, pathlib, json, urllib.request, urllib.error, base64, time
core_source=pathlib.Path(sys.argv[1]).resolve()
sys.path.insert(0,str(core_source/"tests/integration"))
from test_orchard_daemon_wallet_cycle import historical_template_block
cookie_path,url,address=sys.argv[2:]
auth=base64.b64encode(pathlib.Path(cookie_path).read_bytes().strip()).decode()
next_rpc=0.0
def rpc(method,params):
 global next_rpc
 delay=next_rpc-time.monotonic()
 if delay>0:time.sleep(delay)
 next_rpc=time.monotonic()+.04
 req=urllib.request.Request(url,json.dumps({"jsonrpc":"2.0","id":1,"method":method,"params":params}).encode(),{"Authorization":"Basic "+auth,"Content-Type":"application/json"})
 with urllib.request.urlopen(req,timeout=20) as response:r=json.load(response)
 if r.get("error"):raise RuntimeError(r["error"])
 return r.get("result")
assert rpc("getblockcount",[])==0
profile=rpc("getconsensusinfo",[])
assert profile["pow_limit_bits"]=="0x207fffff"
genesis=rpc("getblockheader",[rpc("getblockhash",[0])])
anchor=genesis
for height in range(1,102):
 template=rpc("getblocktemplate",{"address":address,"capabilities":["orchard-immutable-jobs-v1"]})
 assert template["height"]==height
 wire,block_hash=historical_template_block(template,genesis,0x207fffff,1000000,time_anchor=anchor)
 assert rpc("submitblock",[wire.hex()])=={}
 assert rpc("getblockcount",[])==height
 assert rpc("getbestblockhash",[])==block_hash
 if height==1:anchor=rpc("getblockheader",[block_hash])
print("PASS current Core coupled prefix: 101 ordinary PoW blocks",flush=True)

job=rpc("mining.getjob",{"address":address})
assert job.get("height")==102 and len(job.get("header_hex",""))==256
print("PASS current Core typed Orchard job available at 102 before pool submission",flush=True)
