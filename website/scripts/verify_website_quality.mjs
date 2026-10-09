#!/usr/bin/env node
// Use isolated review dependencies and the already-open managed Chrome tab.
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';
const require = createRequire(path.resolve(process.env.REPOTUNNEL_QA_DEPS_DIR || '.repotunnel-tmp/website-manual-ui-review/browser-tools', 'package.json'));
const { chromium } = require('playwright-core');
const origin = 'http://127.0.0.1:3010';
const paths = [];
function routes(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) routes(file);
    else if (file.endsWith('.html')) paths.push('/' + path.relative('dist', file).replaceAll(path.sep, '/').replace(/index\.html$/, ''));
  }
}
routes('dist'); paths.sort();
const browser = await chromium.connectOverCDP('http://127.0.0.1:44085');
let page;
for (const candidate of browser.contexts().flatMap(context => context.pages())) {
  if (!candidate.url().startsWith(origin + '/')) continue;
  if (!process.env.REPOTUNNEL_QA_TAB_ID) { page = candidate; break; }
  const session = await candidate.context().newCDPSession(candidate);
  const { targetInfo } = await session.send('Target.getTargetInfo');
  await session.detach();
  if (targetInfo.targetId === process.env.REPOTUNNEL_QA_TAB_ID) { page = candidate; break; }
}
if (!page) throw new Error('The website QA tab is missing');
const failures = [], records = [], journeys = [];
page.on('pageerror', error => failures.push({ type: 'script', message: error.message }));
const check = (condition, message) => { if (!condition) failures.push({ type: 'journey', message }); };
async function go(route) {
  const response = await page.goto(origin + route, { waitUntil: 'load' });
  check(response?.status() === 200, 'Page response: ' + route);
  await page.evaluate(() => document.fonts.ready);
}
async function scroll(y) {
  await page.evaluate(y => {
    document.documentElement.style.scrollBehavior = 'auto';
    window.scrollTo({ top: y, behavior: 'instant' });
  }, y);
  await page.waitForTimeout(100);
}
try {
  await page.bringToFront();
  if (!process.argv.includes('--journeys-only')) for (const theme of ['light', 'dark']) {
    await page.evaluate(theme => localStorage.setItem('repotunnel-theme', theme), theme);
    for (const width of [375, 1440]) {
      await page.setViewportSize({ width, height: 900 });
      for (const route of paths) {
        await go(route);
        let stops = 0, footerSeen = false;
        const maxY = await page.evaluate(() => Math.max(0, document.documentElement.scrollHeight - innerHeight));
        const positions = [...new Set([0, ...Array.from({length: Math.ceil(maxY / 680)}, (_, i) => Math.min((i + 1) * 680, maxY)), maxY])];
        const issues = [];
        for (const y of positions) {
          await scroll(y); stops++;
          await page.evaluate(async () => {
            const visible = [...document.images].filter(image => {
              const r = image.getBoundingClientRect(); return r.top < innerHeight && r.bottom > 0;
            });
            await Promise.all(visible.map(image => image.complete ? Promise.resolve() : new Promise(resolve => {
              image.addEventListener('load', resolve, {once:true}); image.addEventListener('error', resolve, {once:true});
              setTimeout(resolve, 3000);
            })));
          });
          const state = await page.evaluate(() => {
            const visible = element => !!element.getClientRects().length && getComputedStyle(element).visibility !== 'hidden';
            const overlaps = (a,b) => Math.min(a.right,b.right)-Math.max(a.left,b.left)>1 && Math.min(a.bottom,b.bottom)-Math.max(a.top,b.top)>1;
            const codeIssues = [...document.querySelectorAll('.code-block')].filter(visible).filter(element => {
              const b=element.querySelector('.copy-button')?.getBoundingClientRect();
              const t=element.querySelector('.code-toolbar')?.getBoundingClientRect();
              const p=element.querySelector('pre')?.getBoundingClientRect();
              return !b||!t||!p||b.bottom>p.top+1||b.right>t.right+1||Math.abs((b.top+b.bottom-t.top-t.bottom)/2)>1;
            }).length;
            const controls = [...document.querySelectorAll('main .btn,main .copy-button')].filter(visible);
            const clipped = controls.filter(element => {const r=element.getBoundingClientRect();return r.left < -1 || r.right > innerWidth+1;}).length;
            const groupOverlap = [...document.querySelectorAll('.hero-actions,.product-hero-actions,.download-actions,.docs-pagination,.related-section .section-marker')].filter(element => {
              const children=[...element.children].filter(visible).map(e=>e.getBoundingClientRect());
              return children.some((a,i)=>children.slice(i+1).some(b=>overlaps(a,b)));
            }).length;
            const counts = new Map();
            [...document.querySelectorAll('main a.btn')].filter(visible).forEach(a=>counts.set(a.href,(counts.get(a.href)||0)+1));
            const footer = document.querySelector('footer')?.getBoundingClientRect();
            return { overflow:document.documentElement.scrollWidth > innerWidth+1, codeIssues, clipped, groupOverlap,
              duplicateActions:[...counts.values()].filter(count=>count>1).length,
              brokenImages:[...document.images].filter(image=>image.getBoundingClientRect().top<innerHeight&&image.getBoundingClientRect().bottom>0&&image.complete&&!image.naturalWidth).length,
              footerSeen:!!footer&&footer.top<innerHeight&&footer.bottom>0,
              related:[...document.querySelectorAll('.related-grid a')].map(a=>a.pathname),
              h1:document.querySelectorAll('h1').length,
              arrows:[...document.querySelectorAll('main a.btn,main button')].filter(visible).filter(e=>/[↗↘→←↓↑]/.test(e.textContent)).length };
          });
          footerSeen ||= state.footerSeen;
          for (const key of ['overflow','codeIssues','clipped','groupOverlap','duplicateActions','brokenImages','arrows']) {
            if (state[key]) issues.push({y,type:key,count:state[key]});
          }
          if (state.h1 !== 1) issues.push({y,type:'h1',count:state.h1});
          if (state.related.length && (state.related.length!==3 || new Set(state.related).size!==3 || state.related.includes(route))) issues.push({y,type:'relatedLinks'});
        }
        if (!footerSeen) issues.push({type:'footerNotReached'});
        if (issues.length) failures.push({type:'layout',route,theme,width,issues});
        records.push({route,theme,width,scrollStops:stops,footerSeen,issues});
      }
      console.log(JSON.stringify({phase:'scrolled-layout',theme,width,pages:paths.length,failures:failures.length}));
    }
  }
  await page.setViewportSize({width:1440,height:900}); await go('/');
  await scroll(800); await page.waitForTimeout(180);
  check(await page.locator('.site-header').evaluate(e=>!e.classList.contains('is-hidden')), 'Header shows while scrolling down');
  await scroll(500);
  try {
    await page.waitForFunction(() => {
      const header = document.querySelector('.site-header');
      return header?.classList.contains('is-hidden') && header.getBoundingClientRect().bottom <= 1;
    }, undefined, {timeout:1000});
  } catch { check(false, 'Header hides while scrolling up'); }
  await page.locator('.brand').focus();
  check(await page.locator('.site-header').evaluate(e=>!e.classList.contains('is-hidden')&&e.getBoundingClientRect().top>=-1), 'Keyboard focus reveals header');
  await page.locator('.brand').evaluate(e=>e.blur());
  await scroll(0);
  check(await page.locator('.site-header').evaluate(e=>!e.classList.contains('is-hidden')), 'Header visible at page top');
  journeys.push('Header direction, focus recovery and page top');
  await go('/product/chatgpt-extension/');
  check(await page.locator('.related-grid a[href="/product/continuity/"]').count()===1, 'Extension guide links to Continuity');
  await page.locator('.related-grid a[href="/product/continuity/"]').click();
  check(new URL(page.url()).pathname==='/product/continuity/', 'Related capability opens');
  await go('/solutions/two-ai-coding/');
  check(await page.locator('.related-grid a[href="/solutions/long-running-ai-work/"]').count()===1, 'Team workflow links to long-running work');
  journeys.push('Contextual capability and solution navigation');
  await go('/docs/getting-started/what-is-repotunnel/');
  check((await page.locator('h1').innerText())==='What is RepoTunnel', 'Intro heading punctuation');
  check((await page.locator('main').innerText()).includes('Model Context Protocol (MCP)'), 'Beginner guide explains MCP');
  await page.locator('.site-header [data-action="search-open"]').click();
  await page.locator('#site-search-input').fill('Model Context Protocol');
  await page.locator('#site-search-results a[href="/docs/getting-started/what-is-repotunnel/"]').waitFor();
  await page.keyboard.press('Escape');
  check(!await page.locator('#site-search').evaluate(e=>e.open), 'Search closes with Escape');
  journeys.push('Updated beginner explanation appears in rebuilt search');
  await go('/install/');
  await page.locator('[role="tab"]').first().focus(); await page.keyboard.press('ArrowRight');
  check(await page.locator('[role="tab"][aria-selected="true"]').innerText()==='Windows', 'Keyboard selects Windows installation');
  await page.keyboard.press('End');
  check((await page.locator('[role="tab"][aria-selected="true"]').innerText()).includes('macOS'), 'Keyboard selects macOS installation');
  journeys.push('Installation tab keyboard navigation');
  for (const width of [320,375,1440]) {
    await page.setViewportSize({width,height:900}); await go('/docs/installation/linux/');
    await page.evaluate(()=>Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:async text=>{window.qaCopiedText=text;}}}));
    const button=page.locator('.copy-button').first(), code=await page.locator('.code-block code').first().textContent();
    const initialWidth=(await button.boundingBox()).width;
    await button.focus(); await page.keyboard.press('Enter');
    await page.locator('.copy-label').first().filter({hasText:'Copied'}).waitFor();
    check(await page.evaluate(()=>window.qaCopiedText)===code,'Copy retains complete command at '+width);
    check((await button.boundingBox()).width===initialWidth,'Copy feedback keeps alignment at '+width);
    await page.evaluate(()=>{navigator.clipboard.writeText=async()=>{throw Error('Unavailable');};});
    await button.click(); await page.locator('.copy-label').first().filter({hasText:'Selected'}).waitFor();
    check(await page.evaluate(()=>getSelection().toString())===code,'Copy fallback selects complete command at '+width);
  }
  journeys.push('Code copy and fallback at 320, 375 and 1440 pixels');
  await page.setViewportSize({width:375,height:900}); await go('/');
  await page.locator('[data-action="menu"]').click();
  await scroll(700);await scroll(500);
  check(await page.locator('.mobile-nav').isVisible(),'Open mobile navigation remains visible during scroll');
  check(await page.locator('.site-header').evaluate(e=>!e.classList.contains('is-hidden')),'Header stays available for open navigation');
  await page.keyboard.press('Escape');
  check(await page.locator('[data-action="menu"]').getAttribute('aria-expanded')==='false','Mobile menu closes with Escape');
  await go('/docs/getting-started/what-is-repotunnel/');
  await page.locator('[data-action="docs-menu"]').click();
  check(await page.locator('.docs-nav-groups').isVisible(),'Mobile docs navigation opens');
  await page.keyboard.press('Escape');
  journeys.push('Mobile main and documentation menus');
  await page.emulateMedia({reducedMotion:'reduce'});await go('/');await scroll(800);await scroll(500);
  check(await page.locator('.site-header').evaluate(e=>getComputedStyle(e).transitionDuration==='0s'),'Reduced motion removes header transition');
  journeys.push('Reduced-motion header behavior');
} catch (error) {
  failures.push({type:'runner',message:error.message});
} finally {
  const cleanup = async (name, operation) => {
    let timer;
    try {
      await Promise.race([operation(),new Promise((_,reject)=>{
        timer=setTimeout(()=>reject(new Error('Cleanup timed out: '+name)),10000);
      })]);
    }
    catch (error) { failures.push({type:'cleanup',step:name,message:error.message}); }
    finally { clearTimeout(timer); }
  };
  await cleanup('media',()=>page.emulateMedia({reducedMotion:'no-preference'}));
  await cleanup('theme',()=>page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark')));
  await cleanup('viewport',()=>page.setViewportSize({width:1440,height:900}));
  await cleanup('home',()=>go('/'));
  await cleanup('device-metrics',async()=>{
    const cdp=await page.context().newCDPSession(page);
    try { await cdp.send('Emulation.clearDeviceMetricsOverride'); }
    finally { await cdp.detach(); }
  });
  await cleanup('disconnect',()=>browser.close());
}
const report={checkedAt:new Date().toISOString(),engine:'Playwright with existing managed Chrome',versions:{playwright:require('playwright-core/package.json').version},pages:paths.length,variants:records.length,scrollStops:records.reduce((sum,r)=>sum+r.scrollStops,0),footerCoverage:records.filter(r=>r.footerSeen).length,journeys,failures,records};
const output=process.env.REPOTUNNEL_QA_REPORT || (process.argv.includes('--journeys-only')?'docs/website-quality-journeys-report.json':'docs/website-quality-browser-report.json');
fs.writeFileSync(output,JSON.stringify(report,null,2)+'\n');
console.log(JSON.stringify({...report,records:undefined},null,2));
process.exitCode=failures.length?1:0;
