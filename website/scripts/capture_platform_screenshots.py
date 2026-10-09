#!/usr/bin/env python3
"""Refresh public setup screenshots in an isolated, signed-out browser context."""
import argparse, asyncio, base64, hashlib, json
from datetime import datetime, timezone
from urllib.parse import urlsplit,urlunsplit
from pathlib import Path
from urllib.request import urlopen, Request
import websockets
from verify_browser import Chrome

async def capture(port):
    origin="http://127.0.0.1:"+str(port)
    with urlopen(origin+"/json/version", timeout=10) as response:
        browser_url=json.load(response)["webSocketDebuggerUrl"]
    target_dir=Path("public/images/guides")
    target_dir.mkdir(parents=True, exist_ok=True)
    records=[]
    async with websockets.connect(browser_url,max_size=None) as socket:
        browser=Chrome(socket)
        context=(await browser.call("Target.createBrowserContext"))["browserContextId"]
        try:
            target=(await browser.call("Target.createTarget",{"url":"about:blank","browserContextId":context}))["targetId"]
            for _ in range(30):
                with urlopen(origin+"/json/list",timeout=10) as response:
                    targets=json.load(response)
                page=next((t for t in targets if t["id"]==target),None)
                if page:break
                await asyncio.sleep(.1)
            if not page:raise RuntimeError("Capture tab unavailable")
            async with websockets.connect(page["webSocketDebuggerUrl"],max_size=None) as tab_socket:
                tab=Chrome(tab_socket)
                await tab.call("Page.enable")
                await tab.viewport(1200,820)
                for name,url in [
                    ("ngrok-account","https://dashboard.ngrok.com/signup"),
                    ("route64-portal","https://manager.route64.org/account/login/"),
                    ("duckdns","https://www.duckdns.org/"),
                ]:
                    await tab.call("Page.navigate",{"url":url})
                    hosts=[urlsplit(url).hostname]
                    if name=="ngrok-account":hosts.append("login.ngrok.com")
                    await tab.until("document.readyState === 'complete' && "+json.dumps(hosts)+".includes(location.hostname) && !!document.title")
                    await tab.evaluate("document.fonts.ready.then(()=>true)")
                    await asyncio.sleep(.5)
                    state=await tab.evaluate("({url:location.href,title:document.title,text:document.body.innerText.slice(0,160)})")
                    shot=await tab.call("Page.captureScreenshot",{"format":"png","captureBeyondViewport":False})
                    payload=base64.b64decode(shot["data"])
                    path=target_dir/(name+".png");path.write_bytes(payload)
                    records.append({"name":name,"path":str(path),"source":url,"finalUrl":urlunsplit((*urlsplit(state["url"])[:3],"","")),"kind":"Signed-out public page screenshot","width":1200,"height":820,"sha256":hashlib.sha256(payload).hexdigest(),"bytes":len(payload),"title":state["title"]})
                    print(json.dumps(records[-1]),flush=True)
                video="https://customer-1mwganm1ma0xgnmj.cloudflarestream.com/4b75ad2aa58700602e94b148827687a2/iframe"
                await tab.viewport(1280,720)
                await tab.call("Page.bringToFront")
                await tab.call("Emulation.setFocusEmulationEnabled",{"enabled":True})
                await tab.call("Page.navigate",{"url":video})
                await tab.until("document.readyState==='complete' && !!document.querySelector('video')")
                await tab.evaluate("(()=>{let v=document.querySelector('video');v.muted=true;return v.play().then(()=>true)})()")
                await tab.until("document.querySelector('video').readyState>=2 && document.querySelector('video').duration>1")
                duration=await tab.evaluate("document.querySelector('video').duration")
                await tab.evaluate("document.querySelector('video').pause()")
                for position in (150,):
                    if position>=duration:continue
                    await tab.evaluate("(()=>{const v=document.querySelector('video');v.currentTime="+str(position)+";return true})()")
                    await tab.until("!document.querySelector('video').seeking && document.querySelector('video').readyState>=2")
                    await tab.evaluate("document.querySelector('video').pause()")
                    await asyncio.sleep(.7)
                    name="cloudflare-frame-"+str(position)
                    shot=await tab.call("Page.captureScreenshot",{"format":"png","captureBeyondViewport":False})
                    payload=base64.b64decode(shot["data"])
                    path=target_dir/(name+".png");path.write_bytes(payload)
                    records.append({"name":name,"path":str(path),"source":"https://developers.cloudflare.com/tunnel/get-started/","kind":"Frame from Cloudflare's official public setup tutorial","timeSeconds":position,"width":1280,"height":720,"sha256":hashlib.sha256(payload).hexdigest(),"bytes":len(payload)})
                    print(json.dumps(records[-1]),flush=True)
        finally:
            await browser.call("Target.disposeBrowserContext",{"browserContextId":context})
    for name,url in [
        ("chrome-load-unpacked","https://developer.chrome.com/static/docs/extensions/get-started/tutorial/hello-world/image/extensions-page-e0d64d89a6acf.png"),
        ("android-wireless-debugging","https://developer.android.com/static/studio/images/run/adb_wifi-wireless_debugging_setting.png"),
    ]:
        with urlopen(Request(url,headers={"User-Agent":"Mozilla/5.0"}),timeout=30) as response:
            payload=response.read()
        if payload[:8]!=b"\x89PNG\r\n\x1a\n":raise RuntimeError("Not a PNG: "+name)
        width=int.from_bytes(payload[16:20],"big");height=int.from_bytes(payload[20:24],"big")
        path=target_dir/(name+".png");path.write_bytes(payload)
        records.append({"name":name,"path":str(path),"source":url,"kind":"Official documentation illustration; unmodified","width":width,"height":height,"sha256":hashlib.sha256(payload).hexdigest(),"bytes":len(payload)})
        print(json.dumps(records[-1]),flush=True)
    Path("docs/platform-screenshots.json").write_text(json.dumps({"reviewedOn":datetime.now(timezone.utc).date().isoformat(),"images":records},indent=2)+"\n")
    print("Captured "+str(len(records))+" public images",flush=True)

if __name__=="__main__":
    parser=argparse.ArgumentParser();parser.add_argument("--port",type=int,required=True)
    asyncio.run(capture(parser.parse_args().port))
