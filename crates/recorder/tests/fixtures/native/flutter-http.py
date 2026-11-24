"""Typed endpoint fixture; no app/device behavior is claimed."""
import json,base64,struct,zlib
import sys

call = json.load(sys.stdin);request = call['request']
result = dict(status='completed', generation=request['generation'], id=request['id'],
    operation=request['operation'], logical_us=0, held=True, faulted=False)
if request['operation'] in ['observe','capture']:
    surface = dict(observation=request.get('observation','o'+str(int(request['id'][1:], 16))), revision=1, framework_frame=1, logical_us=0,
        view=None, nodes=[dict(id='s1', label='Note', tooltip='', value='', tappable=True, editable=False,
                              focused=False, rect=[0, 0, 10, 10])])
    if request['operation']=='observe':result['surface']=surface
    else:
        def chunk(kind,data):return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
        png=b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\xff\0\0\xff'))+chunk(b'IEND',b'')
        result['capture']=dict(state='complete',surface=surface,png_base64=base64.b64encode(png).decode(),raster='framework-layer-png')
print(json.dumps(dict(http_status=200, response=result)))
