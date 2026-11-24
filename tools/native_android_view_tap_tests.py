import base64
import json
import itertools
import time
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from native_android_view_tap import execute as tap
from native_android_view_scroll import execute as scroll


class DeliveryTests(unittest.TestCase):
    def test_lost_delivery_reply_recovers_but_unresolved_delivery_never_repeats(self):
        owner=dict(package='org.example.client',activity='org.example.client.Main',generation='abcd0000-0000-0000-0000-000000000001',pid=123)
        request=dict(generation=owner['generation'],controller_id=owner['generation'],request_id=1,
            observation='abcd0000-0000-0000-0000-000000000002',target='abcd0000-0000-0000-0000-000000000003',now_ns=0,x=100,y=200)
        for operation,failure in itertools.product(('tap','scroll'),('none','registration','delivery','slow_delivery','pending','rejected')):
            execute=tap if operation=='tap' else scroll
            request={k:v for k,v in request.items() if k not in ('end_x','end_y','direction')}
            if operation=='scroll':request.update(end_x=100,end_y=50,direction='down')
            with self.subTest(operation=operation,failure=failure), TemporaryDirectory() as folder:
                registrations=0;taps=0
                receipt=dict(request=request,generation=owner['generation'],pid=123,last_id=1,now_ns=0,state='armed')
                def command(*args,timeout):
                    nonlocal registrations,taps
                    if args[:2]==('shell','pidof'):return b'123\n'
                    if args[:3]==('shell','content','call'):
                        registrations+=1
                        self.assertEqual(args[-1].count(':'),2)
                        self.assertEqual(json.loads(base64.b64decode(args[-1].split(':')[2])),request)
                        if failure=='registration':raise TimeoutError('registration response lost')
                        if failure=='rejected':receipt.update(state='rejected',reason='stale_observation')
                        return b'Result: Bundle[{submitted=true}]'
                    if args[:3]==('shell','dumpsys','window'):return b'mCurrentFocus=Window{1 u0 org.example.client/.Main}\n'
                    if args[:2]==('shell','input'):
                        self.assertEqual(args[2],'tap' if operation=='tap' else 'swipe')
                        taps+=1
                        if failure=='pending':receipt.update(state='pending')
                        else:
                            receipt.update(state='completed',down_handled=True,up_handled=True)
                            if operation=='scroll':receipt.update(moves=15,before_offset_px=[0,0],after_offset_px=[0,100])
                        if failure=='slow_delivery':
                            time.sleep(timeout)
                            raise TimeoutError('Delivery used its transport budget')
                        if failure in ('delivery','pending'):raise TimeoutError('delivery response lost')
                        return b''
                    return json.dumps(receipt).encode()
                if failure in ('registration','pending'):
                    for _ in range(2):
                        with self.assertRaises(TimeoutError):execute(command,folder,owner,request,.08)
                    self.assertEqual(json.loads((Path(folder)/'state.json').read_text())['pending'],request)
                else:
                    answer=execute(command,folder,owner,request,1)
                    self.assertEqual(answer,execute(command,folder,owner,request,1))
                    with self.assertRaisesRegex(RuntimeError,'owner changed'):execute(command,folder,{**owner,'pid':124},request,1)
                self.assertEqual(registrations,1)
                self.assertEqual(taps,0 if failure in ('registration','rejected') else 1)

if __name__=='__main__':unittest.main()
