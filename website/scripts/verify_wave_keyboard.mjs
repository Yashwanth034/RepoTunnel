#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
const require=createRequire(path.resolve('.repotunnel-tmp/website-manual-ui-review/browser-tools/package.json'));
const {chromium}=require('playwright-core');
const browser=await chromium.connectOverCDP('http://127.0.0.1:'+(process.env.REPOTUNNEL_WAVE_PORT||'45373'));
const origin='http://127.0.0.1:3010',target=process.env.REPOTUNNEL_WAVE_TARGET;
if(!target)throw Error('Exact owned QA target required');
let page;
for(const p of browser.contexts().flatMap(c=>c.pages())){
 const s=await p.context().newCDPSession(p);const {targetInfo}=await s.send('Target.getTargetInfo');await s.detach();
 if(targetInfo.targetId===target){page=p;break;}
}
if(!page||!page.url().startsWith(origin+'/'))throw Error('Owned QA tab missing');
page.setDefaultTimeout(10000);
const routes=[];
function walk(dir){for(const e of fs.readdirSync(dir,{withFileTypes:true})){const p=path.join(dir,e.name);if(e.isDirectory())walk(p);else if(p.endsWith('.html'))routes.push('/'+path.relative('dist',p).replaceAll(path.sep,'/').replace(/index\.html$/,''));}}
walk('dist');routes.sort();
const report={checkedAt:new Date().toISOString(),status:'running',pages:[],journeys:[],failures:[],screenshots:[]};
const out=process.env.REPOTUNNEL_INTERACTION_REPORT||'docs/website-wave-interaction-verification.json';
function save(){fs.writeFileSync(out,JSON.stringify(report,null,2)+'\n');}
save();
function assert(ok,message){if(!ok)throw Error(message);}
async function go(route,theme='light',width=1440){
 await page.setViewportSize({width,height:900});await page.evaluate(t=>localStorage.setItem('repotunnel-theme',t),theme);
 await page.goto(origin+route+'?keyboard-wave-qa=1',{waitUntil:'load'});await page.evaluate(()=>document.fonts.ready);
}
async function journey(name,theme,width,fn){
 try{const detail=await fn();report.journeys.push({name,theme,width,status:'passed',...detail});}
 catch(e){report.failures.push({name,theme,width,message:e.message});}
 save();
}
const originalTheme=await page.evaluate(()=>localStorage.getItem('repotunnel-theme'));
try{
 await page.bringToFront();
 for(const route of routes){
  try{
   await go(route);
   const expected=await page.evaluate(()=>{
    const visible=e=>!!e.getClientRects().length&&getComputedStyle(e).visibility!=='hidden';
    return [...document.querySelectorAll('a[href],button,input,select,textarea,summary,[tabindex]')].filter(e=>visible(e)&&!e.disabled&&e.tabIndex>=0&&!e.closest('[inert]')).map((e,i)=>{
     e.dataset.keyboardQa=String(i);return {tag:e.tagName,name:(e.getAttribute('aria-label')||e.textContent||e.getAttribute('placeholder')||'').trim().replace(/\s+/g,' ').slice(0,150),href:e.getAttribute('href')};
    });
   });
   assert(expected.length>0&&expected[0].name==='Skip to content','Skip link is not first in keyboard order');
   await page.evaluate(()=>document.activeElement?.blur());
   let traversed=0;
   for(let i=0;i<expected.length;i++){
    await page.keyboard.press('Tab');
    const active=await page.evaluate(()=>({index:document.activeElement?.dataset.keyboardQa,name:document.activeElement?.textContent,tag:document.activeElement?.tagName}));
    assert(active.index===String(i),'Keyboard order mismatch at '+i+': expected '+expected[i].name+', got '+active.tag+' '+active.name);
    traversed++;
   }
   await page.locator('.skip-link').focus();await page.keyboard.press('Enter');
   const skip=await page.evaluate(()=>{
    const e=document.querySelector(document.querySelector('.skip-link').getAttribute('href'));
    return {target:e?.id,focused:document.activeElement===e};
   });
   assert(skip.focused,'Skip link did not focus its content target');
   report.pages.push({route,status:'passed',traversed,skip,order:expected});
  }catch(e){report.failures.push({route,message:e.message});}
  save();
  if(report.pages.length%15===0)console.log(JSON.stringify({phase:'keyboard-order',pages:report.pages.length,failures:report.failures.length}));
 }
 for(const theme of ['light','dark'])for(const width of [375,1440]){
  await journey('Search keyboard, focus containment and Escape',theme,width,async()=>{
   await go('/',theme,width);
   const trigger=page.locator('.site-header [data-action="search-open"]');await trigger.focus();await page.keyboard.press('Enter');
   await page.waitForFunction(()=>document.activeElement?.id==='site-search-input');
   await page.locator('#site-search-input').fill('ngrok');
   await page.locator('#site-search-results a[href="/docs/connections/ngrok/"]').waitFor();
   let browserChromeStops=0;
   for(let i=0;i<24;i++){
    await page.keyboard.press('Tab');
    const focus=await page.evaluate(()=>({inside:!!document.activeElement?.closest('#site-search'),pageFocus:document.hasFocus(),tag:document.activeElement?.tagName}));
    if(!focus.pageFocus&&focus.tag==='BODY')browserChromeStops++;
    else assert(focus.inside,'Search focus reached background page content');
   }
   assert(await page.evaluate(()=>!!document.activeElement?.closest('#site-search')),'Tab did not return from browser UI to the dialog');
   await page.keyboard.press('Escape');
   assert(!(await page.locator('#site-search').evaluate(e=>e.open)),'Search did not close');
   assert(await trigger.evaluate(e=>e===document.activeElement),'Search did not restore focus to opener');
   return {pageContentTabs:24-browserChromeStops,browserChromeStops,backgroundFocusLeaks:0,restoredFocus:true};
  });
  await journey('Installation tab keyboard states',theme,width,async()=>{
   await go('/install/',theme,width);await page.locator('#tab-linux').focus();
   const cases=[['ArrowRight','windows'],['ArrowRight','macos'],['ArrowRight','linux'],['End','macos'],['Home','linux'],['ArrowLeft','macos']];
   for(const [key,platform]of cases){
    await page.keyboard.press(key);
    assert(await page.locator('#tab-'+platform).evaluate(e=>e===document.activeElement&&e.getAttribute('aria-selected')==='true'&&e.tabIndex===0),'Tab state mismatch: '+platform);
    assert(await page.locator('#install-'+platform).evaluate(e=>!e.hidden),'Selected platform is hidden');
    assert(await page.locator('[role="tab"]').evaluateAll(es=>es.filter(e=>e.tabIndex===0).length===1),'Multiple tabs in keyboard sequence');
   }
   return {transitions:cases.length};
  });
  await journey('FAQs and first-run links',theme,width,async()=>{
   await go('/',theme,width);
   for(const summary of await page.locator('.faq-items summary').all()){
    await summary.focus();await page.keyboard.press('Enter');assert(await summary.evaluate(e=>e.parentElement.open),'FAQ did not open');
    await page.keyboard.press('Enter');assert(!(await summary.evaluate(e=>e.parentElement.open)),'FAQ did not close');
   }
   const links=await page.locator('.process-link').evaluateAll(es=>es.map(e=>{
    const r=e.getBoundingClientRect(),s=getComputedStyle(e);return {tag:e.tagName,href:e.getAttribute('href'),height:r.height,underlined:s.textDecorationLine.includes('underline')};
   }));
   assert(links.length===3&&links.every(e=>e.tag==='A'&&e.href&&e.height>=32&&e.underlined),'First-run link semantics or target sizes');
   for(const link of await page.locator('.process-link').all()){
    await link.focus();
    assert(await link.evaluate(e=>getComputedStyle(e).outlineStyle!=='none'),'First-run link lacks a focus indicator');
   }
   return {faqToggles:8,links};
  });
  if(width===375){
   await journey('Mobile navigation keyboard and Escape',theme,width,async()=>{
    await go('/',theme,width);const toggle=page.locator('[data-action="menu"]');await toggle.focus();await page.keyboard.press('Enter');
    assert(await toggle.getAttribute('aria-expanded')==='true','Menu expanded state not updated');
    await page.keyboard.press('Tab');assert(await page.evaluate(()=>!!document.activeElement?.closest('.mobile-nav')),'Mobile menu links not reached');
    await page.keyboard.press('Escape');
    assert(await toggle.evaluate(e=>e===document.activeElement&&e.getAttribute('aria-expanded')==='false'),'Menu Escape did not restore collapsed trigger');
    return {restoredFocus:true};
   });
   await journey('Documentation menu keyboard and Escape',theme,width,async()=>{
    await go('/docs/getting-started/what-is-repotunnel/',theme,width);const toggle=page.locator('[data-action="docs-menu"]');await toggle.focus();await page.keyboard.press('Enter');
    assert(await toggle.getAttribute('aria-expanded')==='true','Documentation expanded state not updated');
    await page.keyboard.press('Tab');assert(await page.evaluate(()=>!!document.activeElement?.closest('.docs-sidebar')),'Documentation sidebar controls not reached');
    await page.keyboard.press('Tab');assert(await page.evaluate(()=>!!document.activeElement?.closest('#docs-nav-groups')),'Documentation navigation links not reached');
    await page.keyboard.press('Escape');
    assert(await toggle.evaluate(e=>e===document.activeElement&&e.getAttribute('aria-expanded')==='false'),'Documentation Escape did not restore trigger');
    return {restoredFocus:true};
   });
  }
  await journey('Copy control and alignment',theme,width,async()=>{
   await go('/docs/connections/direct-https/',theme,width);
   const button=page.locator('.copy-button').first();await button.focus();await page.keyboard.press('Enter');
   await page.waitForFunction(()=>['Copied','Selected'].includes(document.querySelector('.copy-button .copy-label')?.textContent));
   const state=await button.evaluate(e=>({label:e.querySelector('.copy-label')?.textContent,name:e.getAttribute('aria-label'),live:e.querySelector('.copy-label')?.getAttribute('aria-live')}));
   assert(state.name&&state.live==='polite','Copy control feedback or name missing');
   return state;
  });
 }
}catch(e){report.failures.push({name:'Runner',message:e.message});}
finally{
 try{await page.setViewportSize({width:1440,height:900});await page.evaluate(t=>t===null?localStorage.removeItem('repotunnel-theme'):localStorage.setItem('repotunnel-theme',t),originalTheme);await page.goto(origin+'/');}catch(e){report.failures.push({name:'cleanup',message:e.message});}
 report.finishedAt=new Date().toISOString();report.status=report.failures.length?'failed':'passed';report.tabStops=report.pages.reduce((s,p)=>s+p.traversed,0);save();
 console.log(JSON.stringify({status:report.status,pages:report.pages.length,tabStops:report.tabStops,journeys:report.journeys.length,failures:report.failures}));
 process.exit(report.failures.length?1:0);
}
