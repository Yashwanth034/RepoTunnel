#!/usr/bin/env python3
"""Check the local website in its existing managed Chrome tab."""
import argparse
import asyncio
import json
from pathlib import Path
from urllib.request import urlopen
import websockets

class Chrome:
    def __init__(self, socket):
        self.socket, self.serial = socket, 0
        self.errors = []
        self.requests = {}

    async def call(self, method, params=None):
        self.serial += 1
        serial = self.serial
        await self.socket.send(json.dumps({"id": serial, "method": method, "params": params or {}}))
        deadline = asyncio.get_running_loop().time() + 30
        while True:
            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                raise TimeoutError("CDP method timed out: " + method)
            message = json.loads(await asyncio.wait_for(self.socket.recv(), remaining))
            if message.get("method") == "Runtime.exceptionThrown":
                self.errors.append(message["params"]["exceptionDetails"].get("text", "Script exception"))
            if message.get("method") == "Network.requestWillBeSent":
                p = message["params"]
                self.requests[p["requestId"]] = p["request"]["url"]
            if message.get("method") == "Network.loadingFailed":
                p = message["params"]
                url = self.requests.get(p["requestId"], "")
                if url.startswith("http://127.0.0.1:3010/") and not p.get("canceled"):
                    self.errors.append(url + ": " + p.get("errorText", "Network failure"))
            if message.get("id") != serial:
                continue
            if "error" in message:
                raise RuntimeError(message["error"])
            return message.get("result", {})

    async def evaluate(self, expression):
        result = await self.call("Runtime.evaluate", {
            "expression": expression, "awaitPromise": True, "returnByValue": True
        })
        if result.get("exceptionDetails"):
            raise RuntimeError(result["exceptionDetails"])
        return result.get("result", {}).get("value")

    async def until(self, expression):
        for _ in range(100):
            if await self.evaluate(expression):
                return
            await asyncio.sleep(0.05)
        raise AssertionError("Browser state did not arrive: " + expression)

    async def navigate(self, path):
        marker = json.dumps("qa-navigation-" + str(self.serial))
        await self.evaluate("window.qaDocumentBeforeNavigation = " + marker)
        result = await self.call("Page.navigate", {"url": "http://127.0.0.1:3010" + path})
        if result.get("errorText"):
            raise AssertionError("Navigation failed: " + result["errorText"])
        expected = json.dumps(path)
        await self.until("window.qaDocumentBeforeNavigation !== " + marker + " && location.pathname === " + expected + " && document.readyState === 'complete'")
        await self.evaluate("document.fonts.ready.then(() => true)")

    async def viewport(self, width, height):
        await self.call("Emulation.setDeviceMetricsOverride", {
            "width": width, "height": height, "deviceScaleFactor": 1, "mobile": False
        })

    async def escape(self):
        for kind in ("rawKeyDown", "keyUp"):
            await self.call("Input.dispatchKeyEvent", {
                "type": kind, "key": "Escape", "code": "Escape",
                "windowsVirtualKeyCode": 27, "nativeVirtualKeyCode": 27
            })

async def check(args):
    with urlopen("http://127.0.0.1:" + str(args.port) + "/json/list", timeout=10) as response:
        targets = json.load(response)
    target = next(t for t in targets if t["id"] == args.target)
    if not target["url"].startswith("http://127.0.0.1:3010/"):
        raise AssertionError("QA must use the website's own local tab")
    async with websockets.connect(target["webSocketDebuggerUrl"], max_size=None, open_timeout=10) as socket:
        chrome = Chrome(socket)
        await chrome.call("Page.enable")
        await chrome.call("Page.bringToFront")
        await chrome.call("Emulation.setFocusEmulationEnabled", {"enabled": True})
        await chrome.call("Runtime.enable")
        await chrome.call("Network.enable")
        failures, checks, code_blocks = [], 0, 0
        original_theme = await chrome.evaluate("localStorage.getItem('repotunnel-theme')")
        try:
            if args.copy_only:
                for width in (320, 375, 1440):
                    await chrome.viewport(width, 900)
                    await chrome.navigate("/docs/installation/linux/")
                    values = await chrome.evaluate("""[...document.querySelectorAll('.code-block')].map(e=>{
                        const b=e.querySelector('.copy-button').getBoundingClientRect(),p=e.querySelector('pre').getBoundingClientRect();
                        return {buttonBottom:b.bottom,codeTop:p.top};
                    })""")
                    if not values or any(v["buttonBottom"] > v["codeTop"] + 1 for v in values):
                        failures.append("Copy control overlaps code at " + str(width) + "px: " + json.dumps(values))
                    checks += 1
                print(json.dumps({"checks":checks,"failures":failures},indent=2),flush=True)
                return len(failures)
            await chrome.evaluate("localStorage.setItem('repotunnel-theme','light')")
            await chrome.viewport(1366, 640)
            await chrome.navigate("/")
            action = await chrome.evaluate("""(() => {
              const r = document.querySelector('.hero-actions a[href="/downloads/"]').getBoundingClientRect();
              return {top:r.top,bottom:r.bottom,height:innerHeight};
            })()""")
            checks += 1
            if not (action["top"] >= 0 and action["bottom"] <= action["height"]):
                failures.append("Download action is clipped on a laptop viewport: " + json.dumps(action))
            if args.hero_only:
                print(json.dumps({"checks": checks, "failures": failures}, indent=2), flush=True)
                return len(failures)
            paths = []
            for page in sorted(Path("dist").rglob("*.html")):
                path = page.relative_to("dist").as_posix()
                paths.append("/" + (path[:-10] if path.endswith("index.html") else path))
            for width, height in [(320, 812), (375, 812), (640, 900), (768, 900), (1024, 768), (1440, 900), (1920, 1080)]:
                await chrome.viewport(width, height)
                for path in paths:
                    await chrome.navigate(path)
                    layout = await chrome.evaluate("""(() => ({
                      width:document.documentElement.clientWidth, scroll:document.documentElement.scrollWidth,
                      duplicateActions:(()=>{
                        const counts=new Map();
                        [...document.querySelectorAll('.site-header a,main a')].filter(e=>e.matches('.btn,.header-download') && e.getClientRects().length).forEach(e=>counts.set(e.href,(counts.get(e.href)||0)+1));
                        return [...counts.values()].filter(n=>n>1).length;
                      })(),
                      codeCount:document.querySelectorAll('.code-block').length,
                      codeIssues:[...document.querySelectorAll('.code-block')].filter(e=>{
                        if (!e.getClientRects().length) return false;
                        const button=e.querySelector('.copy-button'),toolbar=e.querySelector('.code-toolbar');
                        if (!button || !toolbar) return true;
                        const b=button.getBoundingClientRect(),t=toolbar.getBoundingClientRect(),p=e.querySelector('pre').getBoundingClientRect();
                        return b.bottom>p.top+1 || Math.abs((b.top+b.bottom-t.top-t.bottom)/2)>1 || b.right>t.right+1;
                      }).length,
                      clipped:[...document.querySelectorAll('.header-inner a,.header-inner button,main .btn,main .copy-button')].filter(e=>{
                        if (!e.getClientRects().length || getComputedStyle(e).visibility==='hidden') return false;
                        const r=e.getBoundingClientRect(),parent=e.closest('.header-inner,.download-card,.listing-card,.code-block');
                        const p=parent?.getBoundingClientRect();
                        return r.left < -1 || r.right > innerWidth+1 || (p && (r.left<p.left-1 || r.right>p.right+1));
                      }).length,
                      overlap:[...document.querySelectorAll('.header-inner,.hero-actions,.product-hero-actions,.download-actions,.docs-pagination,.callout-actions,.release-row')].filter(e=>{
                        const controls=[...e.querySelectorAll('a,button')].filter(c=>c.getClientRects().length).map(c=>c.getBoundingClientRect());
                        return controls.some((a,i)=>controls.slice(i+1).some(b=>Math.min(a.right,b.right)-Math.max(a.left,b.left)>1 && Math.min(a.bottom,b.bottom)-Math.max(a.top,b.top)>1));
                      }).length,
                      arrows:[...document.querySelectorAll('a,button')].filter(e=>e.getClientRects().length && /[↗↘→←↓↑]/.test(e.textContent)).length,
                      unnamed:[...document.querySelectorAll('button,a[href],input')].filter(e => {
                        if (!e.getClientRects().length || getComputedStyle(e).visibility === 'hidden') return false;
                        return !(e.getAttribute('aria-label') || e.getAttribute('aria-labelledby') ||
                          e.textContent.trim() || e.getAttribute('title') || e.labels?.length);
                      }).length
                    }))()""")
                    checks += 1
                    if layout["scroll"] > layout["width"] + 1:
                        failures.append(str(width) + "px overflow on " + path + ": " + json.dumps(layout))
                    code_blocks += layout["codeCount"]
                    if layout["codeIssues"]:
                        failures.append("Misaligned copy toolbar at " + str(width) + "px on " + path)
                    if layout["clipped"]:
                        failures.append("Clipped controls at " + str(width) + "px on " + path)
                    if layout["overlap"]:
                        failures.append("Overlapping controls at " + str(width) + "px on " + path)
                    if layout["duplicateActions"]:
                        failures.append("Repeated primary actions on " + path)
                    if layout["arrows"]:
                        failures.append("Decorative arrows remain on " + path)
                    if layout["unnamed"]:
                        failures.append("Unnamed visible controls on " + path)
                print("Checked " + str(len(paths)) + " pages at " + str(width) + "px", flush=True)
            await chrome.viewport(1440, 900)
            await chrome.navigate("/")
            await chrome.evaluate("document.querySelector('[data-action=search-open]').click()")
            await chrome.until("document.querySelector('#site-search').open && document.activeElement.id === 'site-search-input'")
            initial_results = await chrome.evaluate("document.querySelector('#site-search-results').innerHTML")
            await chrome.evaluate("""(() => {
                const input=document.querySelector('#site-search-input');
                input.value='connection'; input.dispatchEvent(new Event('input',{bubbles:true}));
            })()""")
            await chrome.until("document.querySelectorAll('#site-search-results a').length > 0 && document.querySelector('#site-search-results').innerHTML !== " + json.dumps(initial_results))
            assert await chrome.evaluate("[...document.querySelectorAll('#site-search-results a')].every(a => a.pathname !== '/404.html')"), "Search exposes 404"
            await chrome.escape()
            assert not await chrome.evaluate("document.querySelector('#site-search').open"), "Search fails to close"
            await chrome.evaluate("document.querySelector('.faq-items summary').click()")
            await chrome.until("document.querySelector('.faq-items details').open")
            checks += 3

            await chrome.evaluate("document.documentElement.dataset.theme='light'")
            await chrome.evaluate("document.querySelector('[data-action=theme]').click()")
            assert await chrome.evaluate("document.documentElement.dataset.theme") == "dark", "Theme toggle fails"
            await chrome.navigate("/docs/getting-started/what-is-repotunnel/")
            assert await chrome.evaluate("document.documentElement.dataset.theme") == "dark", "Theme does not persist"
            assert not await chrome.evaluate("document.querySelector('[data-action=docs-menu]').getClientRects().length"), "Desktop docs show a mobile-only menu"
            checks += 3

            await chrome.viewport(375, 812)
            await chrome.navigate("/")
            await chrome.evaluate("document.querySelector('[data-action=menu]').click()")
            assert await chrome.evaluate("getComputedStyle(document.querySelector('.mobile-nav')).display") != "none", "Mobile navigation is hidden"
            await chrome.escape()
            assert await chrome.evaluate("document.querySelector('[data-action=menu]').getAttribute('aria-expanded')") == "false", "Mobile navigation fails to close"
            assert await chrome.evaluate("document.activeElement.matches('[data-action=menu]')"), "Mobile navigation loses keyboard focus"
            await chrome.navigate("/docs/getting-started/what-is-repotunnel/")
            assert await chrome.evaluate("document.querySelector('[data-action=docs-menu]').getClientRects().length") > 0, "Mobile docs menu is not visible"
            await chrome.evaluate("document.querySelector('[data-action=docs-menu]').click()")
            assert await chrome.evaluate("getComputedStyle(document.querySelector('.docs-nav-groups')).display") != "none", "Documentation menu is hidden"
            await chrome.escape()
            assert await chrome.evaluate("document.activeElement.matches('[data-action=docs-menu]')"), "Docs navigation loses keyboard focus"
            checks += 5

            for width, height in [(375, 812), (1366, 768)]:
                await chrome.viewport(width, height)
                for path in ["/", "/downloads/", "/docs/getting-started/what-is-repotunnel/"]:
                    await chrome.navigate(path)
                    assert await chrome.evaluate("document.documentElement.dataset.theme") == "dark", "Dark theme does not persist on " + path
                    assert await chrome.evaluate("document.documentElement.scrollWidth <= innerWidth + 1"), "Dark theme overflows on " + path
                    checks += 1

            await chrome.navigate("/install/")
            await chrome.evaluate("document.querySelectorAll('[role=tab]')[1].click()")
            assert await chrome.evaluate("""(() => {
                const t=document.querySelectorAll('[role=tab]')[1];
                return t.getAttribute('aria-selected')==='true' &&
                  !document.getElementById(t.getAttribute('aria-controls')).hidden;
            })()"""), "Installation tabs show the wrong panel"
            checks += 1

            await chrome.navigate("/docs/installation/linux/")
            await chrome.evaluate("""Object.defineProperty(navigator,'clipboard',{configurable:true,
                value:{writeText:async text=>{window.qaCopiedText=text}}})""")
            expected = await chrome.evaluate("document.querySelector('.code-block code').textContent")
            initial_width = await chrome.evaluate("document.querySelector('.copy-button').getBoundingClientRect().width")
            await chrome.evaluate("document.querySelector('.copy-button').click()")
            await chrome.until("document.querySelector('.copy-label').textContent==='Copied'")
            assert await chrome.evaluate("window.qaCopiedText") == expected, "Copy changes the command"
            assert await chrome.evaluate("document.querySelector('.copy-button').getBoundingClientRect().width") == initial_width, "Copy feedback shifts the button"
            await chrome.evaluate("document.querySelector('.copy-button').click()")
            await asyncio.sleep(1.8)
            assert await chrome.evaluate("document.querySelector('.copy-label').textContent") == "Copy", "Repeated clicks leave stale copy feedback"
            await chrome.evaluate("navigator.clipboard.writeText=async()=>{throw new Error('Clipboard unavailable')}")
            await chrome.evaluate("document.querySelector('.copy-button').click()")
            await chrome.until("document.querySelector('.copy-label').textContent==='Selected'")
            assert await chrome.evaluate("getSelection().toString()") == expected, "Clipboard fallback does not select the command"
            assert await chrome.evaluate("document.querySelector('.copy-button').getBoundingClientRect().width") == initial_width, "Fallback feedback shifts the button"
            checks += 6

            await chrome.navigate("/")
            await chrome.call("Emulation.setEmulatedMedia", {"features": [{"name": "prefers-reduced-motion", "value": "reduce"}]})
            await asyncio.sleep(0.1)
            assert await chrome.evaluate("document.getAnimations().filter(a=>a.playState==='running').length") == 0, "Reduced-motion preference is ignored"
            await chrome.call("Emulation.setEmulatedMedia", {"features": []})
            await asyncio.sleep(0.1)
            assert await chrome.evaluate("document.getAnimations().filter(a=>a.playState==='running').length") > 0, "Reference flow animation is missing"
            checks += 2
            failures.extend(chrome.errors)
            print(json.dumps({"checks": checks, "pages": len(paths), "code_blocks_checked":code_blocks, "failures": failures}, indent=2), flush=True)
            return len(failures)
        finally:
            if failures:
                print(json.dumps({"layout_failures": failures}, indent=2), flush=True)
            await chrome.call("Emulation.setFocusEmulationEnabled", {"enabled": False})
            await chrome.call("Emulation.setEmulatedMedia", {"features": []})
            await chrome.evaluate("localStorage." + ("removeItem('repotunnel-theme')" if original_theme is None else "setItem('repotunnel-theme'," + json.dumps(original_theme) + ")"))
            await chrome.call("Emulation.clearDeviceMetricsOverride")
            await chrome.navigate("/")

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--hero-only", action="store_true")
    parser.add_argument("--copy-only", action="store_true")
    args = parser.parse_args()
    raise SystemExit(asyncio.run(check(args)))
