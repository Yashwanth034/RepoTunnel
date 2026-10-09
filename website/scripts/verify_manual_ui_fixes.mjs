#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
const require=createRequire(path.resolve('.repotunnel-tmp/website-manual-ui-review/browser-tools/package.json'));
const {chromium}=require('playwright-core');
const browser=await chromium.connectOverCDP('http://127.0.0.1:44085');
const target='571DA24A499FD76F6D4D19D76D6BAFAF', origin='http://127.0.0.1:3010';
let page,worker,tabId;
for(const p of browser.contexts().flatMap(c=>c.pages())){
  if(!p.url().startsWith(origin+'/'))continue;
  const session=await p.context().newCDPSession(p);
  const {targetInfo}=await session.send('Target.getTargetInfo');await session.detach();
  if(targetInfo.targetId===target){page=p;break;}
}
if(!page)throw Error('Owned QA tab missing');
page.setDefaultTimeout(10000);page.setDefaultNavigationTimeout(12000);
async function waveEval(fn,arg){let timer;try{return await Promise.race([worker.evaluate(fn,arg),new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('WAVE worker operation timed out')),15000);})]);}finally{clearTimeout(timer);}}
const report={checkedAt:new Date().toISOString(),status:'running',layout:[],wave:[],failures:[],screenshots:[]};
const out='docs/website-manual-ui-focused-verification.json';
const fail=(message,context={})=>report.failures.push({message,...context});
async function go(route){
  const response=await page.goto(origin+route+'?manual-ui-qa=1',{waitUntil:'load'});
  if(response?.status()!==200)fail('Unexpected preview response',{route,status:response?.status()});
  await page.evaluate(()=>document.fonts.ready);
}
async function scrollAll(){
  const max=await page.evaluate(()=>Math.max(0,document.documentElement.scrollHeight-innerHeight));
  const ys=[...new Set([0,...Array.from({length:Math.ceil(max/650)},(_,i)=>Math.min((i+1)*650,max)),max])];
  for(const y of ys){
    await page.evaluate(y=>{document.documentElement.style.scrollBehavior='auto';scrollTo({top:y,behavior:'instant'});},y);
    await page.waitForTimeout(35);
  }
  const footerReached=await page.locator('footer').evaluate(e=>{const r=e.getBoundingClientRect();return r.top<innerHeight&&r.bottom>0;});
  return {scrollStops:ys.length,footerReached};
}
async function layout(route,context){
  const traversal=await scrollAll();
  const state=await page.evaluate(()=>{
    const visible=e=>!!e.getClientRects().length;
    const cards=[...document.querySelectorAll('.download-card')].filter(visible).map(e=>{
      const c=e.getBoundingClientRect(),a=e.querySelector('.download-actions')?.getBoundingClientRect();
      return {top:c.top,bottom:c.bottom,actionTop:a?.top,actionBottom:a?.bottom};
    });
    const actionMisalignment=cards.some((a,i)=>cards.slice(i+1).some(b=>Math.abs(a.top-b.top)<1&&Math.abs(a.actionTop-b.actionTop)>1));
    const copyMisalignment=[...document.querySelectorAll('.code-toolbar')].filter(visible).some(t=>{
      const a=t.getBoundingClientRect(),b=t.querySelector('.copy-button').getBoundingClientRect();
      return Math.abs((a.top+a.bottom-b.top-b.bottom)/2)>1||b.right>a.right||b.left<a.left;
    });
    const action=document.querySelector('main .hero-actions')?.getBoundingClientRect();
    const footer=document.querySelector('footer')?.getBoundingClientRect();
    const note=document.querySelector('.docs-version-note');
    return {overflow:document.documentElement.scrollWidth>innerWidth+1,actionMisalignment,copyMisalignment,
      footerGap:action&&footer?footer.top-action.bottom:null,
      versionNote:note?.textContent,versionLink:note?.querySelector('a')?.getAttribute('href'),
      legacyClosingNotes:document.querySelectorAll('.docs-context').length,
      sourceLinks:document.querySelectorAll('.docs-article-meta a').length};
  });
  const record={route,...context,...traversal,...state};report.layout.push(record);
  for(const key of ['overflow','actionMisalignment','copyMisalignment'])if(state[key])fail(key,{route,...context});
  if(!traversal.footerReached)fail('Footer was not reached',{route,...context});
  if(route==='/404.html'&&state.footerGap<24)fail('404 recovery controls crowd the footer',{...context,gap:state.footerGap});
  if(route.startsWith('/docs/')&&(!state.versionNote||state.versionLink!='/changelog/'||state.legacyClosingNotes||state.sourceLinks!==1))fail('Guide version/source context missing or duplicated',{route,...context});
}
async function waveScan(route,context){
  const traversal=await scrollAll();
  await page.evaluate(()=>scrollTo({top:0,behavior:'instant'}));
  const title=await page.title();
  worker=browser.contexts().flatMap(c=>c.serviceWorkers()).find(w=>w.url().startsWith('chrome-extension://jbbplnpkjmmeebjpijfedlgcdilocofh/'));
  if(!worker)throw Error('Installed WAVE worker unavailable');
  if(!tabId){
    tabId=await waveEval(async url=>{
      const tabs=await chrome.tabs.query({url:'http://127.0.0.1:3010/*'});
      const matches=tabs.filter(t=>t.url===url);
      if(matches.length!==1)throw Error('Owned native QA tab is ambiguous');
      return matches[0].id;
    },page.url());
    await waveEval(id=>{
      globalThis.__manualWaveOriginal=serviceworker.func.sendResultsToSidebarWhenReady;
      serviceworker.func.sendResultsToSidebarWhenReady=function(action,data,target){
        if(target===id&&action==='waveResults'&&!globalThis.__manualWaveResult)globalThis.__manualWaveResult=data;
        return globalThis.__manualWaveOriginal.call(this,action,data,target);
      };
    },tabId);
    report.waveEngine=await waveEval(()=>({name:chrome.runtime.getManifest().name,version:chrome.runtime.getManifest().version}));
  }
  await waveEval(async({id,url})=>{
    if(serviceworker.func.isTabActive(id))await serviceworker.func.runWave(id,url);
    globalThis.__manualWaveResult=null;
    await serviceworker.func.runWave(id,url);
  },{id:tabId,url:page.url()});
  let result;
  for(let i=0;i<80;i++){
    result=await waveEval(()=>globalThis.__manualWaveResult);
    if(result?.statistics?.pagetitle===title)break;
    await page.waitForTimeout(125);
  }
  if(!result?.statistics||result.statistics.pagetitle!==title)throw Error('Current-page WAVE result not received: '+route);
  const frame=page.frames().find(f=>f.url().startsWith('chrome-extension://jbbplnpkjmmeebjpijfedlgcdilocofh/'));
  if(!frame)throw Error('WAVE sidebar not loaded');
  // A modal intentionally blocks pointer access to the extension iframe.
  // Activate extension tabs through their DOM handlers while retaining the modal scan state.
  const openTab=async selector=>context.state==='search-open'
    ? frame.locator(selector+' button').evaluate(element=>element.click())
    : frame.locator(selector+' button').click();
  await openTab('#detailstab');
  await openTab('#navigationtab');
  await frame.waitForFunction(()=>document.querySelector('#navlist')?.children.length>0);
  const orderTargets=await frame.locator('#navlist').evaluate(e=>e.children.length);
  await openTab('#structuretab');
  await frame.waitForFunction(()=>document.querySelector('#pageoutline')?.textContent?.trim().length>0);
  const structureText=await frame.locator('#pageoutline').innerText();
  await openTab('#contrasttab');
  const stats=result.statistics;
  const record={route,...context,...traversal,title,statistics:stats,detailsOrderStructureContrastOpened:true,orderTargets,structureText};
  report.wave.push(record);
  if(stats.error||stats.contrast)fail('WAVE error or contrast finding',{route,...context,error:stats.error,contrast:stats.contrast});
  await waveEval(async({id,url})=>{if(serviceworker.func.isTabActive(id))await serviceworker.func.runWave(id,url);},{id:tabId,url:page.url()});
}
try{
  await page.bringToFront();
  for(const theme of ['light','dark'])for(const width of [320,375,768,1440]){
    await page.setViewportSize({width,height:900});await page.evaluate(theme=>localStorage.setItem('repotunnel-theme',theme),theme);
    await go('/install/');
    for(const platform of ['linux','windows','macos']){
      await page.locator('#tab-'+platform).click();await layout('/install/',{theme,width,platform});
    }
    for(const route of ['/downloads/','/404.html','/docs/installation/linux/','/docs/connections/direct-https/']){
      await go(route);await layout(route,{theme,width});
      if(route==='/404.html'){
        await page.locator('main [data-action="search-open"]').click();
        await page.locator('#site-search-input').fill('ngrok');
        await page.locator('#site-search-results a[href="/docs/connections/ngrok/"]').waitFor();
        await page.keyboard.press('Escape');
        if(await page.locator('#site-search').evaluate(e=>e.open))fail('404 search did not close',{theme,width});
      }
    }
  }
  console.log(JSON.stringify({phase:'focused-layout',states:report.layout.length,failures:report.failures.length}));
  for(const theme of ['light','dark'])for(const width of [375,1440]){
    await page.setViewportSize({width,height:900});await page.evaluate(theme=>localStorage.setItem('repotunnel-theme',theme),theme);
    for(const route of ['/docs/installation/linux/','/docs/connections/direct-https/','/docs/connections/ngrok/','/downloads/','/404.html']){
      await go(route);await waveScan(route,{theme,width,state:'default'});
      if(route==='/404.html'){
        await page.locator('main [data-action="search-open"]').click();
        await waveScan(route,{theme,width,state:'search-open'});
      }
    }
    await go('/install/');
    for(const platform of ['linux','windows','macos']){
      await page.locator('#tab-'+platform).click();await waveScan('/install/',{theme,width,state:platform});
    }
    console.log(JSON.stringify({phase:'installed-wave',theme,width,states:report.wave.length,failures:report.failures.length}));
  }
  await page.setViewportSize({width:1440,height:900});await page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark'));
  for(const route of ['/install/','/404.html','/docs/installation/linux/']){
    for(const width of [1440,375]){
      await page.setViewportSize({width,height:900});await go(route);await scrollAll();
      await page.addStyleTag({content:'html{scrollbar-width:none!important}'});
      // Capture from the top with neutral focus so the fixed header/skip link
      // do not appear at the previous scrolled viewport position in a full-page image.
      await page.evaluate(()=>{document.activeElement?.blur();scrollTo({top:0,behavior:'instant'});});
      await page.waitForTimeout(250);
      const filename=route.replaceAll('/','-').replace(/^-/,'')+'-'+width+'.png';
      const dest='.repotunnel-tmp/website-manual-ui-review/after-'+filename;
      await page.screenshot({path:dest,fullPage:true});report.screenshots.push({route,width,path:dest});
    }
  }
}catch(error){fail('Runner failure',{message:error.message});}
finally{
  try{if(worker&&tabId)await waveEval(id=>{
    serviceworker.func.resetTab(id);
    if(globalThis.__manualWaveOriginal)serviceworker.func.sendResultsToSidebarWhenReady=globalThis.__manualWaveOriginal;
    delete globalThis.__manualWaveOriginal;delete globalThis.__manualWaveResult;
  },tabId);}catch(error){fail('WAVE cleanup failed',{detail:error.message});}
  try{
    await page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark'));
    await page.setViewportSize({width:1440,height:900});await go('/');
    const cdp=await page.context().newCDPSession(page);await cdp.send('Emulation.clearDeviceMetricsOverride');await cdp.detach();
  }catch(error){fail('Browser cleanup failed',{detail:error.message});}
  report.status=report.failures.length?'failed':'passed';
  fs.writeFileSync(out,JSON.stringify(report,null,2)+'\n');
  console.log(JSON.stringify({status:report.status,layoutStates:report.layout.length,waveStates:report.wave.length,failures:report.failures,screenshots:report.screenshots}));
  await browser.close();process.exitCode=report.failures.length?1:0;
}
