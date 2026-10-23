#!/usr/bin/env python3
import json,os,sys,tty
tty.setraw(0)
os.write(1,'Do you trust the contents of this directory?'.encode())
actions=[];buffer=b'';trusted=False
while True:
    piece=os.read(0,1024)
    if not piece:break
    buffer+=piece
    while b'\r' in buffer:
        line,buffer=buffer.split(b'\r',1)
        actions.append(line.decode(errors='replace'))
        with open('/Users/jr/Desktop/projects/deppy-sijo-integration-20261023/docs/qa/assets/provider-usage-review-20261023/fake-modern-actions.json','w') as handle:json.dump(actions,handle)
        if not trusted and line in (b'',b'y'):
            trusted=True
        elif trusted and line==b'/usage':
            os.write(1,b'\r\nWeekly limit 30% used\r\n')
