#!/usr/bin/env python3
"""Opt-in Linux VM physical-path/TUN acceptance using a host-loopback STUN fixture.

Use only a disposable Colima VM. The test creates a temporary TUN and a UDP-port
specific policy route inside that VM, then removes both in finally. No host route
or production application configuration is changed. Requires VM root via sudo -n.
"""
import argparse
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--profile',required=True)
parser.add_argument('--binary',required=True,help='Absolute Linux astra-proxy path inside the VM')
args=parser.parse_args()
import json,socket,struct,subprocess,threading
udp=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);udp.bind(('127.0.0.1',0));port=udp.getsockname()[1];received=[]
def serve():
    while True:
        try:data,peer=udp.recvfrom(4096)
        except OSError:return
        if len(data)<20:continue
        received.append(peer)
        ip=bytes(a^b for a,b in zip(socket.inet_aton(peer[0]),struct.pack('!I',0x2112A442)))
        body=struct.pack('!HHBBH',0x20,8,0,1,peer[1]^0x2112)+ip
        udp.sendto(struct.pack('!HHI',0x101,len(body),0x2112A442)+data[8:20]+body,peer)
threading.Thread(target=serve,daemon=True).start()
code=r'''
import os,fcntl,struct,json,subprocess,socket,select,time,signal
port=PORT
binary=BINARY
fd=None
rule=False
def interrupted(signum,frame): raise RuntimeError('Test interrupted')
signal.signal(signal.SIGHUP,interrupted)
signal.signal(signal.SIGTERM,interrupted)
name='astratst0'
assert subprocess.run(['ip','link','show',name],capture_output=True).returncode != 0, 'Test TUN name already exists'
assert '31999:' not in subprocess.check_output(['ip','rule','show'],text=True), 'Test rule priority already exists'
assert ' table 31999 ' not in ' '+subprocess.check_output(['ip','route','show','table','all'],text=True)+' ', 'Test route table already exists'
def ip(*args):subprocess.run(['ip',*args],check=True,capture_output=True)
def drain():
    packets=[]
    while select.select([fd],[],[],0)[0]:packets.append(os.read(fd,65535))
    return packets
def probe(kind,egress={'kind':'interface','name':'eth0'}):
    request={'egress':egress,'probe':kind,'count':1,'interval_ms':0,'timeout_ms':100,'family':'ipv4','path_context':{'sampled_at_ms':'0','core_tun':True,'physical':None}}
    p=subprocess.run([binary,'--network-diagnostics-stream','/tmp/unused-config'],input=json.dumps(request),text=True,capture_output=True,timeout=35)
    return p.returncode,[json.loads(line) for line in p.stdout.splitlines() if line.startswith('{')],p.stderr
nat={'kind':'nat','request':{'agent_id':'isolated-linux','interface_id':'eth0','family':'ipv4','servers':[f'192.168.5.2:{port}'],'advanced':False}}
try:
    baseline=probe(nat)
    report=next(e['report'] for e in baseline[1] if e.get('kind')=='report')
    assert not report['partial'] and report['physical_path']['route_verified']
    print('BASELINE_NAT',report['observations'][0]['mapped_endpoint'])
    fd=os.open('/dev/net/tun',os.O_RDWR|os.O_NONBLOCK)
    fcntl.ioctl(fd,0x400454ca,struct.pack('16sH',name.encode(),0x1001))
    ip('addr','add','198.18.99.1/24','dev',name);ip('link','set',name,'up')
    ip('route','add','table','31999','192.168.5.2/32','dev',name)
    ip('rule','add','priority','31999','ipproto','udp','dport',str(port),'lookup','31999');rule=True
    control=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);control.sendto(b'control-via-tun',('192.168.5.2',port));control.close();time.sleep(.05)
    observed=drain();assert observed,'control UDP must enter real TUN'
    print('UNBOUND_CONTROL_TUN_PACKETS',len(observed))
    pinned=probe(nat);time.sleep(.05);in_tun=drain();assert not in_tun,'physical NAT must never enter TUN'
    r=next(e['report'] for e in pinned[1] if e.get('kind')=='report')
    if r['partial']:
        assert not r['observations'],'refused path must not send'
        print('TUN_ENABLED_NAT_REFUSED_SAFELY',r['notes'][-1])
    else:
        assert r['physical_path']['route_verified'] and r['physical_path']['binding_verified']
        assert all(o['response_received'] for o in r['observations'])
        print('TUN_ENABLED_NAT_PHYSICAL_SUCCESS',len(r['observations']))
    echo=probe({'kind':'icmp','host':'192.168.5.2'},{'kind':'system'})
    assert any(e.get('event')=='echo' and e['observation']['tun_local'] and e['observation']['rtt_ms'] is None and not e['observation']['proves_reachability'] for e in echo[1])
    assert not drain();print('SYSTEM_TUN_ICMP_NO_REACHABILITY')
    trace=probe({'kind':'trace','host':'192.168.5.2','max_hops':1,'probes_per_hop':1},{'kind':'system'})
    assert any(e.get('event')=='trace_summary' and e['probes_sent']==0 and not e['capability']['supported'] for e in trace[1])
    assert not drain();print('SYSTEM_TUN_TRACE_REFUSED')
    invalid=json.loads(json.dumps(nat));invalid['request']['interface_id']=name
    result=probe(invalid,{'kind':'interface','name':name});r=next(e['report'] for e in result[1] if e.get('kind')=='report')
    assert r['partial'] and not r['observations'];assert not drain();print('NAT_VIRTUAL_INTERFACE_REFUSED')
finally:
    if rule:subprocess.run(['ip','rule','del','priority','31999'],check=False)
    subprocess.run(['ip','route','flush','table','31999'],capture_output=True,check=False)
    if fd is not None:os.close(fd)
    print('CLEANED_TUN_TEST_ROUTE_AND_INTERFACE')
'''.replace('port=PORT','port='+str(port)).replace('binary=BINARY','binary='+repr(args.binary))
try:
    p=subprocess.run(['colima','ssh','--profile',args.profile,'--','sudo','-n','python3','-c',code],capture_output=True,text=True,timeout=85)
    print(p.stdout);print(p.stderr);print('HOST_STUN_PACKETS',len(received));raise SystemExit(p.returncode)
finally:udp.close()
