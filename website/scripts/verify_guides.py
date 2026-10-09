#!/usr/bin/env python3
"""Verify rendered guide screenshots, links, numbered steps and search coverage."""
import argparse, asyncio, json
from pathlib import Path
from urllib.request import urlopen
import websockets
from verify_browser import Chrome

async def check(args):
    data=json.loads(Path("src/data/site-content.json").read_text())
    guides=[page for page in data["docs"] if any(section.get("image") for section in page["sections"])]
    with urlopen("http://127.0.0.1:"+str(args.port)+"/json/list",timeout=10) as response:
        target=next(t for t in json.load(response) if t["id"]==args.target)
    if not target["url"].startswith("http://127.0.0.1:3010/"):
        raise AssertionError("Guide QA must use the website's own preview tab")
    failures=[];checks=0;images=0
    async with websockets.connect(target["webSocketDebuggerUrl"],max_size=None) as socket:
        chrome=Chrome(socket)
        await chrome.call("Page.enable")
        await chrome.call("Page.bringToFront")
        await chrome.call("Emulation.setFocusEmulationEnabled",{"enabled":True})
        await chrome.call("Runtime.enable")
        await chrome.call("Network.enable")
        original_theme=await chrome.evaluate("localStorage.getItem('repotunnel-theme')")
        try:
            for theme in ("light","dark"):
                await chrome.evaluate("localStorage.setItem('repotunnel-theme',"+json.dumps(theme)+")")
                for width in (320,375,768,1440):
                    await chrome.viewport(width,900)
                    for page in guides:
                        path="/docs/"+page["slug"]+"/"
                        await chrome.navigate(path)
                        count=await chrome.evaluate("document.querySelectorAll('.guide-figure img').length")
                        expected=sum(bool(s.get("image")) for s in page["sections"])
                        if count!=expected:failures.append(path+": screenshot count differs")
                        for index in range(count):
                            await chrome.evaluate("document.querySelectorAll('.guide-figure img')["+str(index)+"].scrollIntoView({behavior:'instant',block:'center'})")
                            await chrome.until("(()=>{const i=document.querySelectorAll('.guide-figure img')["+str(index)+"];return i.complete&&i.naturalWidth>0})()")
                        state=await chrome.evaluate("""(() => ({
                            overflow:document.documentElement.scrollWidth>document.documentElement.clientWidth,
                            images:[...document.querySelectorAll('.guide-figure')].map(f=>{
                                const i=f.querySelector('img'),r=i.getBoundingClientRect(),a=f.querySelector('.guide-image-link');
                                return {loaded:i.complete&&i.naturalWidth===Number(i.getAttribute('width'))&&i.naturalHeight===Number(i.getAttribute('height')),
                                  fits:r.left>=0&&r.right<=innerWidth+1,
                                  fullSize:a.getAttribute('href')===i.getAttribute('src')&&a.target==='_blank',
                                  caption:!!f.querySelector('figcaption p')?.textContent.trim(),
                                  source:!!f.querySelector('figcaption a[href^="https://"]')};
                            }),
                            badExternalLinks:[...document.querySelectorAll('.guide-links a[href^="https://"]')].filter(a=>a.target!=='_blank'||!a.relList.contains('noopener')||!a.relList.contains('noreferrer')).length,
                            steps:document.querySelectorAll('ol.guide-steps li').length
                        }))()""")
                        if state["overflow"] or state["badExternalLinks"] or not state["steps"] or any(not all(i.values()) for i in state["images"]):
                            failures.append(theme+" "+str(width)+" "+path+": "+json.dumps(state))
                        checks+=1;images+=count
            for path in ("/docs/getting-started/connect-chatgpt/","/docs/getting-started/home-and-local-chat/"):
                await chrome.navigate(path)
                if not await chrome.evaluate("document.querySelector('ol.guide-steps li')!==null"):
                    failures.append(path+": missing runtime/client walkthrough")
                checks+=1
            index=json.loads(Path("public/search-index.json").read_text())
            serialized=json.dumps(index).lower()
            for term in ("authtoken","published application","wireguard","load unpacked","pair device","ollama"):
                if term not in serialized:failures.append("Search index missing "+term)
            failures.extend(chrome.errors)
        finally:
            if original_theme is None:
                await chrome.evaluate("localStorage.removeItem('repotunnel-theme')")
            else:
                await chrome.evaluate("localStorage.setItem('repotunnel-theme',"+json.dumps(original_theme)+")")
            await chrome.call("Emulation.setFocusEmulationEnabled",{"enabled":False})
            await chrome.call("Emulation.clearDeviceMetricsOverride")
            await chrome.navigate("/")
    result={"pageVariants":checks,"imageLoads":images,"failures":failures}
    Path("docs/guide-browser-report.json").write_text(json.dumps(result,indent=2)+"\n")
    print(json.dumps(result,indent=2),flush=True)
    return bool(failures)

if __name__=="__main__":
    parser=argparse.ArgumentParser();parser.add_argument("--port",type=int,required=True);parser.add_argument("--target",required=True)
    raise SystemExit(asyncio.run(check(parser.parse_args())))
