#!/usr/bin/env python3
"""Audit the built site with the installed WAVE Chrome extension, not a substitute engine."""
import argparse
import asyncio
from collections import Counter
from datetime import datetime, timezone
import json
from pathlib import Path
from urllib.request import urlopen

import websockets
from verify_browser import Chrome

EXTENSION_ID = "jbbplnpkjmmeebjpijfedlgcdilocofh"
BASELINE = r"""(() => {
  const xpath = el => {
    if (el === document.documentElement) return '/HTML';
    const tag = el.tagName;
    let index = 1;
    for (let previous = el.previousElementSibling; previous; previous = previous.previousElementSibling)
      if (previous.tagName === tag) index++;
    return xpath(el.parentElement) + '/' + tag + '[' + index + ']';
  };
  return Object.fromEntries([...document.querySelectorAll('html body *')].map(el => {
    const style = getComputedStyle(el);
    return [xpath(el), {
      tag: el.tagName, text: el.textContent.trim().replace(/\s+/g, ' ').slice(0, 180),
      href: el.getAttribute('href'), alt: el.getAttribute('alt'),
      ariaLabel: el.getAttribute('aria-label'),
      linked: !!el.closest('a[href]'), visible: !!el.getClientRects().length,
      className: typeof el.className === 'string' ? el.className : '',
      color: style.color, background: style.backgroundColor,
      fontSize: style.fontSize, fontWeight: style.fontWeight
    }];
  }));
})()"""
INSTALL_CAPTURE = """(() => {
  globalThis.qaWaveCapture = null;
  globalThis.qaWaveSeen = new WeakSet();
  if (!serviceworker.func.qaOriginalResults) {
    serviceworker.func.qaOriginalResults = serviceworker.func.sendResultsToSidebarWhenReady;
    serviceworker.func.sendResultsToSidebarWhenReady = function(action, data, tabId) {
      if (action === 'waveResults' && tabId === globalThis.qaWaveTab && !globalThis.qaWaveSeen.has(data)) {
        globalThis.qaWaveSeen.add(data);
        globalThis.qaWaveCapture = {tabId, data};
      }
      return serviceworker.func.qaOriginalResults(action, data, tabId);
    };
  }
  return chrome.runtime.getManifest().version;
})()"""

async def wait_for(chrome, expression, timeout=20):
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        if await chrome.evaluate(expression):
            return
        await asyncio.sleep(0.1)
    raise AssertionError("WAVE state did not arrive: " + expression)

async def run(args):
    with urlopen(f"http://127.0.0.1:{args.port}/json/list", timeout=10) as response:
        targets = json.load(response)
    page_target = next(t for t in targets if t["id"] == args.target)
    if not page_target["url"].startswith("http://127.0.0.1:3010/"):
        raise AssertionError("Use the site's own local QA tab")
    worker_target = next(t for t in targets if t["type"] == "service_worker" and EXTENSION_ID in t["url"])
    paths = args.paths or [
        "/" + (relative[:-10] if relative.endswith("index.html") else relative)
        for relative in (p.relative_to("dist").as_posix() for p in sorted(Path("dist").rglob("*.html")))
    ]
    records = []
    async with websockets.connect(page_target["webSocketDebuggerUrl"], max_size=None) as page_socket, \
            websockets.connect(worker_target["webSocketDebuggerUrl"], max_size=None) as worker_socket:
        page, worker = Chrome(page_socket), Chrome(worker_socket)
        await worker.call("Debugger.enable")
        await page.call("Page.enable")
        await page.call("Page.bringToFront")
        original_theme = await page.evaluate("localStorage.getItem('repotunnel-theme')")
        version, audit_failure, cleanup_failures = None, None, []

        async def scan(path, theme, width, state="default"):
            scroll_stops, footer_reached = 0, False
            if args.scroll_pages:
                dimensions = await page.evaluate("({height:innerHeight,max:Math.max(0,document.documentElement.scrollHeight-innerHeight)})")
                step = max(1, int(dimensions["height"] * .8))
                positions = sorted(set([0, dimensions["max"]] + list(range(0, dimensions["max"] + 1, step))))
                for position in positions:
                    await page.evaluate("document.documentElement.style.scrollBehavior='auto';window.scrollTo({top:" + str(position) + ",behavior:'instant'});true")
                    await asyncio.sleep(.08)
                    footer_reached = footer_reached or await page.evaluate("(()=>{const r=document.querySelector('footer')?.getBoundingClientRect();return !!r&&r.top<innerHeight&&r.bottom>0})()")
                    scroll_stops += 1
                await page.evaluate("window.scrollTo({top:0,behavior:'instant'});true")
                await asyncio.sleep(.1)
            assert await page.evaluate("document.documentElement.dataset.theme") == theme, "WAVE page has the wrong theme"
            baseline = await page.evaluate(BASELINE)
            expected_title = await page.evaluate("document.title")
            await worker.evaluate("""(async()=> {
              globalThis.qaWaveCapture = null;
              const tab = await chrome.tabs.get(qaWaveTab);
              if (serviceworker.func.isTabActive(tab.id)) await serviceworker.func.runWave(tab.id, tab.url);
              await serviceworker.func.runWave(tab.id, tab.url);
              return true;
            })()""")
            await wait_for(worker, "qaWaveCapture !== null && qaWaveCapture.data.success === true && "
                           "qaWaveCapture.data.statistics.pagetitle === " + json.dumps(expected_title) +
                           " && serviceworker.vars.sidebarLoaded.includes(qaWaveTab)")
            data = await worker.evaluate("qaWaveCapture.data")
            assert data["statistics"]["pagetitle"] == expected_title, "WAVE returned a different page"
            findings = []
            for category in ("error", "contrast", "alert"):
                for item in data["categories"][category]["items"].values():
                    for index, xpath in enumerate(item.get("xpaths", [])):
                        findings.append({
                            "category": category, "id": item["id"], "description": item["description"],
                            "xpath": xpath, "hidden": item.get("hidden", [False] * (index + 1))[index],
                            "engineText": item.get("text", [False] * (index + 1))[index],
                            "element": baseline.get(xpath)
                        })
            record = {"path": path, "theme": theme, "width": width, "state": state,
                      "scrollStops": scroll_stops, "footerReached": footer_reached,
                      "expectedTitle": expected_title, "statistics": data["statistics"], "findings": findings}
            records.append(record)
            Path(args.output).write_text(json.dumps({"status": "in-progress", "extensionVersion": version, "records": records}, indent=2) + "\n")
            await worker.evaluate("serviceworker.func.runWave(qaWaveTab, 'http://127.0.0.1:3010/').then(()=>true)")
            await wait_for(page, "!document.querySelector('#wave_sidebar_container')")
            return record

        try:
            await page.call("Emulation.setFocusEmulationEnabled", {"enabled": True})
            version = await worker.evaluate(INSTALL_CAPTURE)
            await worker.evaluate("""(async()=> {
              const tab = (await chrome.tabs.query({})).find(t => t.url.startsWith('http://127.0.0.1:3010/'));
              globalThis.qaWaveTab = tab.id;
              if (serviceworker.func.isTabActive(tab.id)) await serviceworker.func.runWave(tab.id, tab.url);
              return tab.id;
            })()""")
            for theme in args.themes:
                await page.evaluate("localStorage.setItem('repotunnel-theme'," + json.dumps(theme) + ")")
                for width in args.widths:
                    await page.viewport(width, 1000)
                    for path in paths:
                        await page.navigate(path)
                        await scan(path, theme, width)
                        if len(records) % 20 == 0:
                            print(json.dumps({"progress": len(records), "path": path, "theme": theme, "width": width}), flush=True)
                    subset = [r for r in records if r["theme"] == theme and r["width"] == width]
                    print(json.dumps({"theme": theme, "width": width, "pages": len(subset),
                                      "errors": sum(r["statistics"]["error"] for r in subset),
                                      "contrast": sum(r["statistics"]["contrast"] for r in subset),
                                      "alerts": sum(r["statistics"]["alert"] for r in subset)}), flush=True)
                    if args.interactive:
                        states = [
                            ("/", "search-open", "document.querySelector('[data-action=search-open]').click()"),
                            ("/", "faq-expanded", "document.querySelector('.faq-items summary').click()"),
                            ("/install/", "installation-tab-2", "document.querySelectorAll('[role=tab]')[1].click()"),
                        ]
                        if width < 768:
                            states += [
                                ("/", "navigation-open", "document.querySelector('[data-action=menu]').click()"),
                                ("/docs/getting-started/what-is-repotunnel/", "docs-navigation-open",
                                 "document.querySelector('[data-action=docs-menu]').click()"),
                            ]
                        for path, state, action in states:
                            await page.navigate(path)
                            await page.evaluate(action)
                            await asyncio.wait_for(page.evaluate("new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))"), timeout=10)
                            await scan(path, theme, width, state)
        except Exception as error:
            audit_failure = {"type": type(error).__name__, "message": str(error)}
            print(json.dumps({"auditFailure": audit_failure}), flush=True)
        finally:
            async def cleanup(name, operation):
                try:
                    await asyncio.wait_for(operation(), 10)
                except Exception as error:
                    cleanup_failures.append({"step": name, "type": type(error).__name__, "message": str(error)})
                    print(json.dumps({"cleanupFailure": cleanup_failures[-1]}), flush=True)
            await cleanup("wave-worker-hooks", lambda: worker.evaluate("""(async()=> {
              if (globalThis.qaWaveTab && serviceworker.func.isTabActive(qaWaveTab))
                await serviceworker.func.runWave(qaWaveTab, 'http://127.0.0.1:3010/');
              if (serviceworker.func.qaOriginalResults) {
                serviceworker.func.sendResultsToSidebarWhenReady = serviceworker.func.qaOriginalResults;
                delete serviceworker.func.qaOriginalResults;
              }
              delete globalThis.qaWaveCapture;
              delete globalThis.qaWaveTab;
              delete globalThis.qaWaveSeen;
              return true;
            })()"""))
            await cleanup("worker-debugger", lambda: worker.call("Debugger.disable"))
            await cleanup("theme", lambda: page.evaluate("localStorage." + ("removeItem('repotunnel-theme')" if original_theme is None
                else "setItem('repotunnel-theme'," + json.dumps(original_theme) + ")")))
            await cleanup("focus", lambda: page.call("Emulation.setFocusEmulationEnabled", {"enabled": False}))
            await cleanup("device-metrics", lambda: page.call("Emulation.clearDeviceMetricsOverride"))
            await cleanup("home", lambda: page.navigate("/"))
    totals = {category: sum(r["statistics"][category] for r in records) for category in ("error", "contrast", "alert")}
    report = {
        "checkedAt": datetime.now(timezone.utc).isoformat(), "engine": "Installed WAVE Evaluation Tool",
        "extensionId": EXTENSION_ID, "extensionVersion": version,
        "uniquePages": len(paths), "pageStates": len(records), "totals": totals,
        "auditFailure": audit_failure, "cleanupFailures": cleanup_failures,
        "status": "failed" if audit_failure or cleanup_failures or totals["error"] or totals["contrast"] else "passed",
        "scrolledPages": args.scroll_pages, "scrollStops": sum(r["scrollStops"] for r in records),
        "footerCoverage": sum(r["footerReached"] for r in records),
        "findingTypes": dict(Counter(f["id"] for r in records for f in r["findings"])), "records": records
    }
    Path(args.output).write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "records"}, indent=2), flush=True)
    return int(bool(totals["error"] or totals["contrast"] or audit_failure or cleanup_failures))

if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", default="docs/wave-accessibility-report.json")
    parser.add_argument("--paths", nargs="*")
    parser.add_argument("--themes", nargs="+", default=["light", "dark"])
    parser.add_argument("--widths", type=int, nargs="+", default=[375, 1440])
    parser.add_argument("--interactive", action="store_true")
    parser.add_argument("--scroll-pages", action="store_true", help="Scroll through each page to its footer before the whole-document WAVE scan")
    raise SystemExit(asyncio.run(run(parser.parse_args())))
