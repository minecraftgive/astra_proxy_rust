#!/usr/bin/env python3
"""Windows large-message/small-message frequency baseline with native load clients."""
import argparse
import concurrent.futures
import ctypes as C
from ctypes import wintypes as W
import hashlib
import http.server
import json
import os
import pathlib
import platform
import statistics
import subprocess
import threading
import time
from types import SimpleNamespace
from windows_modes_regression import Core, base, free_port, powershell, proxy_state, wininet_get

ROOT=pathlib.Path(__file__).resolve().parents[1]
K=C.WinDLL('kernel32',use_last_error=True)
P=C.WinDLL('psapi',use_last_error=True)
K.OpenProcess.argtypes=[W.DWORD,W.BOOL,W.DWORD];K.OpenProcess.restype=C.c_void_p
K.CloseHandle.argtypes=[C.c_void_p]
K.GetProcessTimes.argtypes=[C.c_void_p]+[C.POINTER(W.FILETIME)]*4


class Memory(C.Structure):
    _fields_=[('cb',W.DWORD),('PageFaultCount',W.DWORD)]+[(name,C.c_size_t) for name in ('PeakWorkingSetSize','WorkingSetSize','QuotaPeakPagedPoolUsage','QuotaPagedPoolUsage','QuotaPeakNonPagedPoolUsage','QuotaNonPagedPoolUsage','PagefileUsage','PeakPagefileUsage','PrivateUsage')]


P.GetProcessMemoryInfo.argtypes=[C.c_void_p,C.POINTER(Memory),W.DWORD]


class Meter:
    def __init__(self,pid):
        self.handle=K.OpenProcess(0x410,False,pid)
        if not self.handle:raise C.WinError(C.get_last_error())
        self.peak_private=0;self.peak_working=0;self.sample()
        self.private_start=self.memory.PrivateUsage;self.cpu_start=self.cpu()

    def cpu(self):
        times=[W.FILETIME() for _ in range(4)]
        if not K.GetProcessTimes(self.handle,*[C.byref(t) for t in times]):raise C.WinError(C.get_last_error())
        return sum((t.dwHighDateTime<<32)+t.dwLowDateTime for t in times[2:])/1e7

    def sample(self):
        value=Memory();value.cb=C.sizeof(value)
        if P.GetProcessMemoryInfo(self.handle,C.byref(value),C.sizeof(value)):
            self.memory=value;self.peak_private=max(self.peak_private,value.PrivateUsage);self.peak_working=max(self.peak_working,value.WorkingSetSize)

    def finish(self,wall):
        self.sample();cpu=self.cpu()-self.cpu_start;K.CloseHandle(self.handle)
        return dict(cpu_seconds=cpu,cpu_percent_one_core=cpu/wall*100,private_start_mib=self.private_start/1048576,private_end_mib=self.memory.PrivateUsage/1048576,peak_private_mib=self.peak_private/1048576,peak_working_mib=self.peak_working/1048576)


def native(args,case,kind,host,port,size,round_no,upstream,core):
    command=[str(args.helper),'bench',kind,host,str(port),str(args.concurrency),str(size),str(args.seconds)]
    meters={'upstream':Meter(upstream.pid)}
    if core:meters['proxy']=Meter(core.child.pid)
    start=time.perf_counter();child=subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
    meters['load_generator']=Meter(child.pid)
    try:
        while child.poll() is None:
            if time.perf_counter()-start>args.seconds+20:raise TimeoutError('native workload timed out')
            for meter in meters.values():meter.sample()
            time.sleep(.1)
        output,error=child.communicate(timeout=5)
        if child.returncode:raise RuntimeError(error.decode(errors='replace'))
        data=json.loads(output)
    finally:
        if child.poll() is None:child.kill();child.wait(timeout=5)
        wall=time.perf_counter()-start
        usage={name:meter.finish(wall) for name,meter in meters.items()}
    row=dict(case=case,transport=kind,message_bytes=size,concurrency=args.concurrency,round=round_no,usage=usage,**data)
    print(f"PASS {case} {size}B round {round_no}: {data['messages_per_second']:.0f} msg/s, {data['mib_per_second_one_direction']:.1f} MiB/s, p99 {data['latency_p99_ms']:.3f} ms",flush=True)
    return row


class Origin(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        n=int(self.path.strip('/'));body=self.server.bodies[n]
        self.send_response(200);self.send_header('Content-Length',str(n));self.end_headers();self.wfile.write(body)
    def log_message(self,*args):pass


def http_load(args,size,round_no,origin,core,upstream):
    meters={name:Meter(pid) for name,pid in [('proxy',core.child.pid),('upstream',upstream.pid),('python_driver',os.getpid())]}
    gate=threading.Barrier(args.concurrency+1)
    def worker():
        times=[];gate.wait();deadline=time.perf_counter()+args.seconds
        while time.perf_counter()<deadline:
            start=time.perf_counter();body=wininet_get(f'http://system-proxy.astra.invalid:{origin.server_port}/{size}')
            assert body==origin.bodies[size];times.append((time.perf_counter()-start)*1000)
        return times
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        jobs=[pool.submit(worker) for _ in range(args.concurrency)];gate.wait();start=time.perf_counter()
        while not all(job.done() for job in jobs):
            for meter in meters.values():meter.sample()
            time.sleep(.1)
        times=sorted(value for job in jobs for value in job.result())
    wall=time.perf_counter()-start
    usage={name:meter.finish(wall) for name,meter in meters.items()}
    percentile=lambda p:times[min(len(times)-1,int((len(times)-1)*p))]
    row=dict(case='system-wininet-http',transport='HTTP-new-request',message_bytes=size,concurrency=args.concurrency,round=round_no,messages=len(times),seconds=wall,messages_per_second=len(times)/wall,mib_per_second_one_direction=len(times)*size/wall/1048576,latency_p50_ms=percentile(.5),latency_p95_ms=percentile(.95),latency_p99_ms=percentile(.99),usage=usage)
    print(f"PASS WinINet HTTP {size}B round {round_no}: {row['messages_per_second']:.0f} req/s",flush=True)
    return row


def run(args,report):
    directory=args.output_dir;directory.mkdir(parents=True,exist_ok=True)
    port=free_port();log=(directory/'native-upstream.log').open('wb')
    server=subprocess.Popen([str(args.helper),'serve',str(port)],stdout=subprocess.PIPE,stderr=log)
    assert server.stdout.readline().strip()==b'READY'
    fake=SimpleNamespace(server_address=('127.0.0.1',port))
    core=None;original=proxy_state() if args.mode=='system-proxy' else None
    targets=('198.19.253.2','fd00:198:19:253::2');prefixes=[targets[0]+'/32',targets[1]+'/128']
    query="@(Get-NetRoute -ErrorAction SilentlyContinue | Where-Object { $_.DestinationPrefix -in @('"+"','".join(prefixes)+"') }).Count"
    try:
        local=free_port();http_port=free_port()
        config=base(fake,local).replace('shutdown_timeout_secs=1',f'http_listen="127.0.0.1:{http_port}"\nshutdown_timeout_secs=1')
        if args.mode=='system-proxy':config+='\n[system_proxy]\n'
        if args.mode=='tun':
            assert C.windll.shell32.IsUserAnAdmin(),'administrator required for Wintun'
            assert args.interface and args.interface.isdigit()
            assert powershell(query)=='0','test routes already exist'
            name='AstraLoad'+str(os.getpid())
            config+=f'\n[tun]\nname="{name}"\naddress="198.18.253.1"\nprefix=30\nipv6_address="fd00:198:18:253::1"\nipv6_prefix=126\nmtu=1500\ninterface="{args.interface}"\nroutes={json.dumps(prefixes)}\n'
        core=Core(args,directory,config,'load-'+args.mode);core.ready()
        cases=[]
        if args.mode=='normal':
            for case,kind,listener,owner in [('baseline-upstream','socks',port,None),('astra-socks','socks',local,core)]:
                cases.extend((case,kind,'127.0.0.1',listener,n,owner) for n in (64,65536,1048576))
            for case,listener,owner in [('baseline-upstream-udp',port,None),('astra-socks-udp',local,core)]:
                cases.extend((case,'socks-udp','127.0.0.1',listener,n,owner) for n in (64,1200))
        elif args.mode=='system-proxy':
            active=proxy_state();assert active['server']==f'127.0.0.1:{http_port}'
            cases=[('system-connect','connect','127.0.0.1',http_port,n,core) for n in (64,65536,1048576)]
        else:
            assert powershell(query)=='2'
            for family,host in zip(('v4','v6'),targets):
                cases.extend(('tun-'+family,'tcp',host,18081,n,core) for n in (64,65536,1048576))
                cases.extend(('tun-'+family+'-udp','udp',host,18082,n,core) for n in (64,1200))
        for round_no in range(1,args.rounds+1):
            for case,kind,host,listener,n,owner in cases:
                try:
                    report['results'].append(native(args,case,kind,host,listener,n,round_no,server,owner))
                except Exception:
                    report['failed_case']=dict(case=case,message_bytes=n,round=round_no)
                    raise
                (directory/'progress.json').write_text(json.dumps(report,indent=2)+'\n')
        if args.mode=='system-proxy':
            with http.server.ThreadingHTTPServer(('127.0.0.1',0),Origin) as origin:
                origin.bodies={n:bytes([0x5a])*n for n in (64,1048576)}
                threading.Thread(target=origin.serve_forever,daemon=True).start()
                try:
                    for round_no in range(1,args.rounds+1):
                        for n in origin.bodies:report['results'].append(http_load(args,n,round_no,origin,core,server))
                finally:origin.shutdown()
    finally:
        if core:core.close()
        server.kill();server.wait(timeout=5);log.close()
        if original is not None:assert proxy_state()==original,'system proxy restoration mismatch'
        if args.mode=='tun':
            assert powershell(query)=='0','owned route leaked'
            assert powershell(f"@(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue).Count")=='0','owned adapter leaked'
    report['restored']=True


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode',choices=['normal','system-proxy','tun'],required=True)
    parser.add_argument('--bin-dir',type=pathlib.Path,required=True)
    parser.add_argument('--helper',type=pathlib.Path,default=ROOT/'.test-tools/windows-load/windows-load.exe')
    parser.add_argument('--output-dir',type=pathlib.Path,required=True)
    parser.add_argument('--interface');parser.add_argument('--seconds',type=float,default=3)
    parser.add_argument('--rounds',type=int,default=3);parser.add_argument('--concurrency',type=int,default=4)
    parser.add_argument('--profile',choices=['release','debug','unknown'],default='unknown')
    args=parser.parse_args();args.bin_dir=args.bin_dir.resolve();args.helper=args.helper.resolve();args.output_dir=args.output_dir.resolve();args.log_level='warn'
    if args.seconds <= 0 or args.rounds < 1 or not 1 <= args.concurrency <= 64:parser.error('seconds and rounds must be positive; concurrency must be 1..64')
    args.output_dir.mkdir(parents=True,exist_ok=True)
    report=dict(mode=args.mode,passed=False,profile=args.profile,platform=platform.platform(),logical_cpus=os.cpu_count(),binary_sha256=hashlib.sha256((args.bin_dir/'astra-proxy.exe').read_bytes()).hexdigest(),seconds_per_case=args.seconds,rounds=args.rounds,results=[],scope=f'Local native echo through plaintext SOCKS; TCP sizes are application messages, not IP packets. {args.concurrency} persistent connections, one outstanding request each. Native upstream and generator CPU recorded separately. WinINet HTTP creates new requests and includes Python origin/driver overhead; not directly comparable to persistent TCP. No allocation stack sampling.')
    try:run(args,report);report['passed']=True
    except Exception as error:report['error']=repr(error);raise
    finally:(args.output_dir/'result.json').write_text(json.dumps(report,indent=2)+'\n')


if __name__=='__main__':main()
