#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
const require=createRequire(path.resolve('.repotunnel-tmp/website-manual-ui-review/browser-tools/package.json'));
const {chromium}=require('playwright-core');
const browser=await chromium.connectOverCDP('http://127.0.0.1:44085');
let page;
for(const p of browser.contexts().flatMap(c=>c.pages())){
 if(!p.url().startsWith('http://127.0.0.1:3010/'))continue;
 const cdp=await p.context().newCDPSession(p);
 const {targetInfo}=await cdp.send('Target.getTargetInfo');await cdp.detach();
 if(targetInfo.targetId==='571DA24A499FD76F6D4D19D76D6BAFAF'){page=p;break;}
}
if(!page)throw Error('Owned QA tab missing');
page.setDefaultTimeout(10000);page.setDefaultNavigationTimeout(12000);
const records=[];
try{
 await page.bringToFront();
 await page.evaluate(()=>localStorage.setItem('repotunnel-theme','dark'));
 for(const route of ['/install/','/404.html','/docs/installation/linux/']){
  for(const width of [1440,375]){
   await page.setViewportSize({width,height:900});
   await page.goto('http://127.0.0.1:3010'+route,{waitUntil:'load'});
   await page.evaluate(()=>document.fonts.ready);
   const max=await page.evaluate(()=>Math.max(0,document.documentElement.scrollHeight-innerHeight));
   for(let y=0;y<=max+650;y+=650){
    await page.evaluate(y=>scrollTo({top:y,behavior:'instant'}),Math.min(y,max));await page.waitForTimeout(35);
   }
   await page.addStyleTag({content:'html{scrollbar-width:none!important}'});
   await page.evaluate(()=>{document.activeElement?.blur();scrollTo({top:0,behavior:'instant'});});
   await page.waitForTimeout(300);
   const file='.repotunnel-tmp/website-manual-ui-review/after-'+route.replaceAll('/','-').replace(/^-/,'')+'-'+width+'.png';
   await page.screenshot({path:file,fullPage:true});
   const state=await page.evaluate(()=>({scrollY,active:document.activeElement?.tagName,headerTop:document.querySelector('header')?.getBoundingClientRect().top}));
   records.push({route,width,path:file,...state});console.log(JSON.stringify(records.at(-1)));
  }
 }
 fs.writeFileSync('.repotunnel-tmp/website-manual-ui-review/post-fix-capture-checks.json',JSON.stringify(records,null,2)+'\n');
}finally{
 await page.setViewportSize({width:1440,height:900});
 await page.goto('http://127.0.0.1:3010/',{waitUntil:'load'});
 const cdp=await page.context().newCDPSession(page);await cdp.send('Emulation.clearDeviceMetricsOverride');await cdp.detach();
 await browser.close();
}
