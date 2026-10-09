#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import crypto from 'node:crypto';
import { createRequire } from 'node:module';
const require = createRequire(path.resolve('.repotunnel-tmp/website-quality-tools/browser-tools/package.json'));
const browser = await require('playwright-core').chromium.connectOverCDP('http://127.0.0.1:44085');
const page = browser.contexts().flatMap(context => context.pages()).find(page => page.url().startsWith('http://127.0.0.1:3010/'));
if (!page) throw new Error('The local website QA tab is missing');
const cdp = await page.context().newCDPSession(page);
const manifest = JSON.parse(fs.readFileSync('docs/platform-screenshots.json')).images;
const originals = new Map(manifest.map(record => [path.basename(record.path, '.png'), record]));
const routes = [];
function walk(dir) {
  for (const entry of fs.readdirSync(dir, {withFileTypes:true})) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(file);
    else if (file.endsWith('.html') && fs.readFileSync(file, 'utf8').includes('guide-image-link'))
      routes.push('/' + path.relative('dist', file).replaceAll(path.sep, '/').replace(/index\.html$/, ''));
  }
}
walk('dist');
const records = [], failures = [], verifiedOriginals = new Set();
const modes = [{theme:'light',width:375,scale:1},{theme:'dark',width:375,scale:1},{theme:'light',width:1440,scale:1},{theme:'dark',width:1440,scale:1},{theme:'light',width:375,scale:2}];
try {
  if (routes.length !== 5 || manifest.length !== 6) throw new Error('Guide coverage changed; review the expected route and screenshot inventory');
  await page.bringToFront();
  for (const mode of modes) {
    await page.evaluate(theme => localStorage.setItem('repotunnel-theme', theme), mode.theme);
    await cdp.send('Emulation.setDeviceMetricsOverride', {width:mode.width,height:900,deviceScaleFactor:mode.scale,mobile:mode.width<640});
    for (const route of routes) {
      await page.goto('http://127.0.0.1:3010' + route);
      for (const image of await page.locator('.guide-figure img').all()) {
        await image.evaluate(element => {document.documentElement.style.scrollBehavior='auto';window.scrollTo({top:scrollY+element.getBoundingClientRect().top-110,behavior:'instant'});});
        await page.waitForFunction(element => element.complete && element.naturalWidth > 0, await image.elementHandle(), {timeout:5000});
        const state = await image.evaluate(element => {
          const figure = element.closest('figure');
          return {src:new URL(element.currentSrc).pathname,original:figure.querySelector('.guide-image-link').getAttribute('href'),alt:element.alt,srcset:element.srcset,sizes:element.sizes,naturalWidth:element.naturalWidth,sourceHref:figure.querySelector('.guide-figure-links a').href,overflow:document.documentElement.scrollWidth>innerWidth+1};
        });
        const original = [...originals.values()].find(record => state.original.includes(path.basename(record.path, '.png') + '.'));
        if (!original) throw new Error('Unexpected original screenshot: ' + state.original);
        const bytes = fs.statSync(path.join('dist', state.src)).size;
        if (!state.src.endsWith('.webp') || !state.srcset || !state.sizes || !state.alt || state.overflow || !state.sourceHref.startsWith('https://'))
          failures.push({route,...mode,type:'image-markup-or-layout',state});
        if (mode.width===375 && mode.scale===1 && bytes>=original.bytes)
          failures.push({route,...mode,type:'mobile-transfer-not-reduced',bytes,originalBytes:original.bytes});
        if (!verifiedOriginals.has(original.path)) {
          const response = await fetch('http://127.0.0.1:3010' + state.original);
          const payload = Buffer.from(await response.arrayBuffer());
          if (!response.ok || crypto.createHash('sha256').update(payload).digest('hex')!==original.sha256)
            failures.push({route,type:'full-size-original-changed',original:state.original});
          verifiedOriginals.add(original.path);
        }
        records.push({route,...mode,...state,bytes,originalBytes:original.bytes,savedPercent:Math.round((1-bytes/original.bytes)*100)});
      }
      await page.evaluate(()=>window.scrollTo({top:document.documentElement.scrollHeight,behavior:'instant'}));
    }
  }
} catch(error) {failures.push({type:'runner',message:error.message});}
finally {
  for (const [step,operation] of [['metrics',()=>cdp.send('Emulation.clearDeviceMetricsOverride')],['theme',()=>page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark'))],['home',()=>page.goto('http://127.0.0.1:3010/')],['detach',()=>cdp.detach()],['disconnect',()=>browser.close()]]) {
    let timer;
    try {await Promise.race([operation(),new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('Cleanup timed out')),10000);})]);}
    catch(error) {failures.push({type:'cleanup',step,message:error.message});}
    finally {clearTimeout(timer);}
  }
}
if (verifiedOriginals.size !== manifest.length || records.length !== manifest.length * modes.length)
  failures.push({type:'incomplete-coverage',originals:verifiedOriginals.size,imageStates:records.length});
const mobile=records.filter(record=>record.theme==='light'&&record.width===375&&record.scale===1);
const report={checkedAt:new Date().toISOString(),routes,modes,imageStates:records.length,originalsVerified:verifiedOriginals.size,mobileBytes:mobile.reduce((sum,record)=>sum+record.bytes,0),originalBytes:mobile.reduce((sum,record)=>sum+record.originalBytes,0),failures,records};
fs.writeFileSync('docs/website-quality-guide-images-report.json',JSON.stringify(report,null,2)+'\n');
console.log(JSON.stringify({...report,records:undefined},null,2));
process.exitCode=failures.length?1:0;
