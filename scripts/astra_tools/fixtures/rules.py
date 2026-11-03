"""Synthetic MMDB test data; no real geolocation information."""
from pathlib import Path


def geoip(path, category='telegram', networks=('127.0.0.0/8', '2001:db8::/32'), inverse=False):
    """Synthetic V2Ray GeoIPList; never uses real category data."""
    import ipaddress
    def varint(value):
        data = b''
        while value > 127:
            data += bytes([(value & 127) | 128])
            value >>= 7
        return data + bytes([value])
    def field(tag, data):
        return varint((tag << 3) | 2) + varint(len(data)) + data
    group = field(1, category.encode())
    for network in networks:
        net = ipaddress.ip_network(network)
        group += field(2, field(1, net.network_address.packed) + b'\x10' + varint(net.prefixlen))
    if inverse:
        group += b'\x18\x01'
    Path(path).write_bytes(field(1, group))

def encode(value):
    if isinstance(value,str):
        data=value.encode();return bytes([0x40+len(data)])+data
    if isinstance(value,int):
        data=value.to_bytes(max(1,(value.bit_length()+7)//8),'big');return bytes([0xc0+len(data)])+data
    if isinstance(value,list):
        return bytes([len(value),4])+b''.join(map(encode,value))
    if isinstance(value,dict):
        return bytes([0xe0+len(value)])+b''.join(encode(k)+encode(v) for k,v in value.items())
    raise TypeError(value)

def mmdb(path,style='country'):
    def record(code):
        if style=='country':return {'country':{'iso_code':code}}
        if style=='flat':return code
        return [code]
    left,right=encode(record('CN')),encode(record('US'))
    tree=(17).to_bytes(3,'big')+(17+len(left)).to_bytes(3,'big')
    metadata={'node_count':1,'record_size':24,'ip_version':4,'database_type':'GeoIP2-Country' if style=='country' else 'sing-geoip' if style=='flat' else 'Meta-geoip0','languages':['en'],'binary_format_major_version':2,'binary_format_minor_version':0,'build_epoch':1,'description':{'en':'Synthetic test countries'}}
    Path(path).write_bytes(tree+b'\0'*16+left+right+b'\xab\xcd\xefMaxMind.com'+encode(metadata))


def geosite(path, pattern=r'^rx[0-9]+\.test$', include_localhost=False):
    """Synthetic V2Ray GeoSiteList covering all four domain types and attributes."""
    def varint(value):
        data=b''
        while value>127:data+=bytes([(value&127)|128]);value>>=7
        return data+bytes([value])
    def string(tag,value):
        if isinstance(value,str):value=value.encode()
        return varint((tag<<3)|2)+varint(len(value))+value
    def domain(kind,value,attrs=()):
        result=varint(8)+varint(kind)+string(2,value)
        for attr in attrs:result+=string(3,string(1,attr)+b'\x10\x01')
        return result
    records=[domain(3,'exact.test'),domain(2,'suffix.test',['cn']),domain(0,'.keyword.',['fast']),domain(1,pattern,['cn','fast'])]
    if include_localhost:records.append(domain(3,'localhost'))
    group=string(1,'TEST')+b''.join(string(2,item) for item in records)
    other=string(1,'geolocation-!cn')+string(2,domain(2,'abroad.test'))
    Path(path).write_bytes(string(1,group)+string(1,other))
