#!/usr/bin/env node
// Evaluate the installed WAVE extension on the website's own isolated QA tab.
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
const require=createRequire(path.resolve('.repotunnel-tmp/website-manual-ui-review/browser-tools/package.json'));
const {chromium}=require('playwright-core');
const origin='http://127.0.0.1:3010';
const phase=process.env.REPOTUNNEL_WAVE_PHASE||'baseline';
const out=process.env.REPOTUNNEL_WAVE_REPORT||'docs/website-wave-complete-'+phase+'.json';
const target=process.env.REPOTUNNEL_WAVE_TARGET;
if(!target)throw Error('An exact owned QA target is required');
const browser=await chromium.connectOverCDP('http://127.0.0.1:'+(process.env.REPOTUNNEL_WAVE_PORT||'45373'));
let page,worker,tabId,originalTheme;
for(const p of browser.contexts().flatMap(c=>c.pages())){
  const session=await p.context().newCDPSession(p);
  const {targetInfo}=await session.send('Target.getTargetInfo');await session.detach();
  if(targetInfo.targetId===target){page=p;break;}
}
if(!page||!page.url().startsWith(origin+'/'))throw Error('Owned website QA tab missing');
page.setDefaultTimeout(10000);page.setDefaultNavigationTimeout(15000);
const routes=[];
function walk(dir){for(const entry of fs.readdirSync(dir,{withFileTypes:true})){
  const p=path.join(dir,entry.name);
  if(entry.isDirectory())walk(p);
  else if(p.endsWith('.html'))routes.push('/'+path.relative('dist',p).replaceAll(path.sep,'/').replace(/index\.html$/,''));
}}
walk('dist');routes.sort();
const resume=process.env.REPOTUNNEL_WAVE_RESUME==='1'&&fs.existsSync(out);
const report=resume?JSON.parse(fs.readFileSync(out,'utf8')):{startedAt:new Date().toISOString(),phase,status:'collecting',engine:'Installed WAVE Evaluation Tool',routes,records:[],failures:[]};
if(resume){
  if(report.phase!==phase||JSON.stringify(report.routes)!==JSON.stringify(routes))throw Error('Resume inventory does not match this build');
  report.previousAttempts=[...(report.previousAttempts||[]),{finishedAt:report.finishedAt,failures:report.failures}];
  report.failures=[];report.status='collecting';delete report.finishedAt;
}
function save(){fs.writeFileSync(out,JSON.stringify(report,null,2)+'\n');}
async function wEval(fn,arg){
  let timer;
  try{return await Promise.race([worker.evaluate(fn,arg),new Promise((_,reject)=>{timer=setTimeout(()=>reject(Error('WAVE worker timed out')),15000);})]);}
  finally{clearTimeout(timer);}
}
async function scrollAll(){
  const max=await page.evaluate(()=>Math.max(0,document.documentElement.scrollHeight-innerHeight));
  const positions=[...new Set([0,...Array.from({length:Math.ceil(max/650)},(_,i)=>Math.min((i+1)*650,max)),max])];
  let footerReached=false,overflow=false;
  for(const y of positions){
    await page.evaluate(y=>{document.documentElement.style.scrollBehavior='auto';scrollTo({top:y,behavior:'instant'});},y);
    await page.waitForTimeout(40);
    overflow||=await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth+1);
    footerReached||=await page.locator('footer').evaluate(e=>{const r=e.getBoundingClientRect();return r.top<innerHeight&&r.bottom>0;});
    await page.evaluate(async()=>{
      await Promise.all([...document.images].filter(i=>{const r=i.getBoundingClientRect();return r.top<innerHeight&&r.bottom>0&&!i.complete;}).map(i=>new Promise(resolve=>{
        i.addEventListener('load',resolve,{once:true});i.addEventListener('error',resolve,{once:true});setTimeout(resolve,1000);
      })));
    });
  }
  await page.evaluate(()=>{document.activeElement?.blur();scrollTo({top:0,behavior:'instant'});});
  return {scrollStops:positions.length,footerReached,overflow};
}
async function go(route,theme,width){
  await page.setViewportSize({width,height:900});
  await page.evaluate(t=>localStorage.setItem('repotunnel-theme',t),theme);
  const response=await page.goto(origin+route+'?wave-complete-qa=1',{waitUntil:'load'});
  if(response?.status()!==200)throw Error('Page response '+response?.status()+': '+route);
  await page.evaluate(()=>document.fonts.ready);
  if(await page.evaluate(()=>document.documentElement.dataset.theme)!==theme)throw Error('Wrong theme');
}
async function auditDOM(){
  return page.evaluate(()=>{
    const clean=s=>(s||'').trim().replace(/\s+/g,' ');
    const visible=e=>!!e.getClientRects().length&&getComputedStyle(e).visibility!=='hidden';
    const xp=e=>{
      if(e===document.documentElement)return '/HTML';
      let i=1;for(let p=e.previousElementSibling;p;p=p.previousElementSibling)if(p.tagName===e.tagName)i++;
      return xp(e.parentElement)+'/'+e.tagName+'['+i+']';
    };
    const elements={};const problems=[];
    const names=e=>e.getAttribute('aria-label')||(e.getAttribute('aria-labelledby')||'').split(/\s+/).map(id=>document.getElementById(id)?.textContent||'').join(' ')||e.getAttribute('alt')||e.textContent||e.getAttribute('title')||'';
    const all=[document.documentElement,...document.querySelectorAll('body *')];
    const ids=new Set();
    for(const e of all){
      const aria=Object.fromEntries([...e.attributes].filter(a=>a.name.startsWith('aria-')||['role','id','href','alt','lang','tabindex'].includes(a.name)).map(a=>[a.name,a.value]));
      const item={tag:e.tagName,text:clean(e.textContent).slice(0,160),attributes:aria,visible:visible(e),insideLink:!!e.closest('a[href]'),accessibleName:clean(names(e)).slice(0,180)};
      elements[xp(e)]=item;
      if(e.id){if(ids.has(e.id))problems.push({type:'duplicate-id',id:e.id});ids.add(e.id);}
      for(const key of ['aria-labelledby','aria-describedby','aria-controls','aria-activedescendant']){
        if(e.hasAttribute(key))for(const id of e.getAttribute(key).trim().split(/\s+/).filter(Boolean)){
          if(!document.getElementById(id))problems.push({type:'missing-aria-reference',attribute:key,id,element:xp(e)});
        }
      }
      if(e.hasAttribute('aria-expanded')&&!['true','false'].includes(e.getAttribute('aria-expanded')))problems.push({type:'invalid-expanded-state',element:xp(e)});
      if(e.hasAttribute('aria-label')||e.hasAttribute('aria-labelledby')){
        const role=e.getAttribute('role');
        if((['DIV','SPAN','P','STRONG','EM','CODE'].includes(e.tagName)&&!role)||['generic','none','presentation'].includes(role))problems.push({type:'prohibited-accessible-name',element:xp(e)});
      }
      if(visible(e)&&e.tabIndex>0)problems.push({type:'positive-tabindex',element:xp(e)});
      if(visible(e)&&e.matches('a[href],button,input,select,textarea,[tabindex]')&&!e.disabled&&e.tabIndex>=0&&e.closest('[aria-hidden="true"]'))problems.push({type:'focusable-in-aria-hidden',element:xp(e)});
      if(visible(e)&&e.matches('button,a[href]')&&!clean(names(e)))problems.push({type:'unnamed-control',element:xp(e)});
      if(e.matches('img')&&!e.hasAttribute('alt'))problems.push({type:'missing-alt',element:xp(e)});
    }
    const main=document.querySelector('main');
    const headings=[...main.querySelectorAll('h1,h2,h3,h4,h5,h6')].filter(visible).map(e=>({level:Number(e.tagName.slice(1)),text:clean(e.textContent)}));
    if(headings.filter(h=>h.level===1).length!==1)problems.push({type:'h1-count'});
    for(let i=1;i<headings.length;i++)if(headings[i].level>headings[i-1].level+1)problems.push({type:'heading-level-skip',heading:headings[i]});
    if(document.querySelectorAll('main').length!==1||!document.querySelector('header')||!document.querySelector('footer'))problems.push({type:'landmarks'});
    for(const nav of document.querySelectorAll('nav'))if(!clean(names(nav))||(!nav.getAttribute('aria-label')&&!nav.getAttribute('aria-labelledby')))problems.push({type:'unnamed-navigation',element:xp(nav)});
    const skip=document.querySelector('.skip-link');
    if(!skip||!document.querySelector(skip.getAttribute('href')))problems.push({type:'skip-target'});
    const controls=[...document.querySelectorAll('a[href],button,input,select,textarea,summary,[tabindex]')].filter(e=>visible(e)&&!e.disabled&&e.tabIndex>=0).map(e=>({tag:e.tagName,name:clean(names(e)).slice(0,180),href:e.getAttribute('href'),aria:Object.fromEntries([...e.attributes].filter(a=>a.name.startsWith('aria-')).map(a=>[a.name,a.value]))}));
    const images=[...document.images].map(e=>({alt:e.alt,src:e.currentSrc||e.src,caption:clean(e.closest('figure')?.querySelector('figcaption')?.textContent),complete:e.complete,naturalWidth:e.naturalWidth}));
    const layoutProblems=[];
    for(const toolbar of document.querySelectorAll('.code-toolbar'))if(visible(toolbar)){
      const r=toolbar.getBoundingClientRect(),b=toolbar.querySelector('.copy-button')?.getBoundingClientRect();
      if(!b||Math.abs((r.top+r.bottom-b.top-b.bottom)/2)>1||b.right>r.right+1||b.left<r.left-1)layoutProblems.push({type:'copy-button-alignment'});
    }
    const cards=[...document.querySelectorAll('.download-card')].filter(visible).map(e=>({card:e.getBoundingClientRect(),actions:e.querySelector('.download-actions')?.getBoundingClientRect()}));
    for(let i=0;i<cards.length;i++)for(let j=i+1;j<cards.length;j++)if(Math.abs(cards[i].card.top-cards[j].card.top)<1&&cards[i].actions&&cards[j].actions&&Math.abs(cards[i].actions.top-cards[j].actions.top)>1)layoutProblems.push({type:'download-actions-alignment'});
    return {elements,problems,layoutProblems,headings,controls,images,lang:document.documentElement.lang,mainCount:document.querySelectorAll('main').length};
  });
}
async function scan(route,theme,width,state,traversal){
  const dom=await auditDOM();const title=await page.title();
  worker=browser.contexts().flatMap(c=>c.serviceWorkers()).find(w=>w.url().startsWith('chrome-extension://jbbplnpkjmmeebjpijfedlgcdilocofh/'));
  if(!worker)throw Error('Installed WAVE worker unavailable');
  if(!tabId){
    tabId=await wEval(async url=>{
      const tabs=await chrome.tabs.query({url:'http://127.0.0.1:3010/*'});
      const matches=tabs.filter(t=>t.url===url);
      if(matches.length!==1)throw Error('Owned tab is ambiguous');return matches[0].id;
    },page.url());
    await wEval(id=>{
      globalThis.__completeWaveOriginal=serviceworker.func.sendResultsToSidebarWhenReady;
      serviceworker.func.sendResultsToSidebarWhenReady=function(action,data,target){
        if(target===id&&action==='waveResults'&&!globalThis.__completeWaveResult)globalThis.__completeWaveResult=data;
        return globalThis.__completeWaveOriginal.call(this,action,data,target);
      };
    },tabId);
    report.version=await wEval(()=>chrome.runtime.getManifest().version);
  }
  await wEval(async({id,url})=>{
    if(serviceworker.func.isTabActive(id))await serviceworker.func.runWave(id,url);
    globalThis.__completeWaveResult=null;await serviceworker.func.runWave(id,url);
  },{id:tabId,url:page.url()});
  let result;
  for(let i=0;i<100;i++){
    result=await wEval(()=>globalThis.__completeWaveResult);
    if(result?.statistics?.pagetitle===title)break;
    await page.waitForTimeout(100);
  }
  if(!result?.statistics||result.statistics.pagetitle!==title)throw Error('WAVE result missing: '+route);
  let frame;
  for(let i=0;i<100;i++){
    frame=page.frames().find(f=>f.url().startsWith('chrome-extension://jbbplnpkjmmeebjpijfedlgcdilocofh/'));
    if(frame)break;
    await page.waitForTimeout(100);
  }
  if(!frame)throw Error('WAVE sidebar missing after waiting for attachment');
  await frame.locator('#detailstab button').evaluate(e=>e.click());
  await frame.locator('#navigationtab button').evaluate(e=>e.click());
  await frame.waitForFunction(()=>document.querySelector('#navlist')?.children.length>0,{},{timeout:10000});
  const orderTargets=await frame.locator('#navlist').evaluate(e=>e.children.length);
  await frame.locator('#structuretab button').evaluate(e=>e.click());
  await frame.waitForFunction(()=>!!document.querySelector('#pageoutline')?.textContent?.trim(),{},{timeout:10000});
  const structureText=await frame.locator('#pageoutline').innerText();
  await frame.locator('#contrasttab button').evaluate(e=>e.click());
  if(!orderTargets||!structureText.trim())throw Error('Order/Structure panel did not populate');
  const categories={};const findings=[];
  for(const category of ['error','contrast','alert','feature','structure','aria']){
    const source=result.categories?.[category];
    categories[category]={description:source?.description,count:source?.count,items:[]};
    for(const item of Object.values(source?.items||{})){
      const observations=(item.xpaths||[]).map((xpath,i)=>({xpath,hidden:item.hidden?.[i]||false,element:dom.elements[xpath]||null}));
      categories[category].items.push({id:item.id,description:item.description,count:item.count,observations});
      if(['error','contrast','alert'].includes(category))for(const observation of observations)findings.push({category,id:item.id,description:item.description,...observation});
    }
  }
  const record={route,theme,width,state,...traversal,title,statistics:result.statistics,categories,findings,semanticProblems:dom.problems,layoutProblems:[...dom.layoutProblems,...(traversal.overflow?[{type:'horizontal-overflow'}]:[])],headings:dom.headings,order:dom.controls,images:dom.images,detailsOrderStructureContrastOpened:true,orderTargets,structureText};
  report.records.push(record);save();
  await wEval(async({id,url})=>{if(serviceworker.func.isTabActive(id))await serviceworker.func.runWave(id,url);},{id:tabId,url:page.url()});
}
async function one(route,theme,width,state='default',action){
  if(resume&&report.records.some(r=>r.route===route&&r.theme===theme&&r.width===width&&r.state===state))return;
  await go(route,theme,width);const traversal=await scrollAll();
  if(!traversal.footerReached)throw Error('Footer not reached: '+route);
  if(action)await action();
  await scan(route,theme,width,state,traversal);
}
try{
  originalTheme=await page.evaluate(()=>localStorage.getItem('repotunnel-theme'));
  await page.bringToFront();
  const themes=phase==='baseline'?['light']:['light','dark'];
  const widths=phase==='baseline'?[1440]:[375,1440];
  for(const theme of themes)for(const width of widths){
    for(const route of routes){
      await one(route,theme,width);
      if(report.records.length%15===0)console.log(JSON.stringify({phase,records:report.records.length,last:route,theme,width}));
    }
    console.log(JSON.stringify({phase,group:'pages',theme,width,records:report.records.length}));
  }
  for(const theme of ['light','dark'])for(const width of [375,1440]){
    for(let i=0;i<4;i++)await one('/',theme,width,'faq-'+(i+1),()=>page.locator('.faq-items summary').nth(i).click());
    for(const [state,query]of [['search-empty',''],['search-results','ngrok'],['search-no-results','zzzz-nothing-99384']]){
      await one('/',theme,width,state,async()=>{
        await page.locator('.site-header [data-action="search-open"]').click();
        if(query){await page.locator('#site-search-input').fill(query);await page.waitForTimeout(700);}
      });
    }
    for(const platform of ['linux','windows','macos'])await one('/install/',theme,width,'install-'+platform,()=>page.locator('#tab-'+platform).click());
    await one('/404.html',theme,width,'404-search',async()=>{await page.locator('main [data-action="search-open"]').click();await page.locator('#site-search-input').fill('ngrok');await page.waitForTimeout(700);});
    if(width===375){
      for(const route of ['/','/product/','/docs/'])await one(route,theme,width,'main-menu',()=>page.locator('[data-action="menu"]').click());
      await one('/docs/getting-started/what-is-repotunnel/',theme,width,'docs-menu',()=>page.locator('[data-action="docs-menu"]').click());
    }
    console.log(JSON.stringify({phase,group:'interactive',theme,width,records:report.records.length}));
  }
}catch(error){report.failures.push({type:'runner',message:error.message});}
finally{
  try{if(worker&&tabId)await wEval(id=>{
    serviceworker.func.resetTab(id);
    if(globalThis.__completeWaveOriginal)serviceworker.func.sendResultsToSidebarWhenReady=globalThis.__completeWaveOriginal;
    delete globalThis.__completeWaveOriginal;delete globalThis.__completeWaveResult;
  },tabId);}catch(error){report.failures.push({type:'wave-cleanup',message:error.message});}
  try{
    await page.setViewportSize({width:1440,height:900});
    await page.evaluate(t=>t===null?localStorage.removeItem('repotunnel-theme'):localStorage.setItem('repotunnel-theme',t),originalTheme);
    await page.goto(origin+'/');
  }catch(error){report.failures.push({type:'page-cleanup',message:error.message});}
  report.finishedAt=new Date().toISOString();
  report.totals=Object.fromEntries(['error','contrast','alert','feature','structure','aria'].map(c=>[c,report.records.reduce((s,r)=>s+(r.statistics[c]||0),0)]));
  report.semanticProblemCount=report.records.reduce((s,r)=>s+r.semanticProblems.length,0);
  report.layoutProblemCount=report.records.reduce((s,r)=>s+(r.layoutProblems?.length||0),0);
  report.scrollStops=report.records.reduce((s,r)=>s+r.scrollStops,0);
  report.footerCoverage=report.records.filter(r=>r.footerReached).length;
  report.uniquePages=new Set(report.records.map(r=>r.route)).size;
  report.status=report.failures.length?'incomplete':report.totals.error||report.totals.contrast||report.totals.alert||report.semanticProblemCount||report.layoutProblemCount?'needs-review':'passed';
  save();console.log(JSON.stringify({...report,routes:undefined,records:undefined}));
  process.exit(report.status==='passed'||phase==='baseline'&&!report.failures.length?0:1);
}
